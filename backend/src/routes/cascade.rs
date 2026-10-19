//! Deleting a column or a board, and everything in it (card #471).
//!
//! The routes remove children themselves — cards (with their links), then
//! columns — writing an audit row for each, because history must record every
//! deletion. The foreign keys are `ON DELETE RESTRICT`, so a parent can only go
//! once it really is empty. Two rules follow:
//!
//! * **A parent's audit row is written after its delete succeeds.** A card
//!   created or moved into the column while the cascade runs makes the parent
//!   delete fail; the history must not then claim a deletion that did not
//!   happen.
//! * **The cascade sweeps again** when that happens: the late child is
//!   deleted (and audited) like the rest, then the parent is retried. After
//!   [`ATTEMPTS`] rounds of losing that race the route gives up with a 409 —
//!   the column is busier than a delete can keep up with, and the caller can
//!   simply retry. Every child deleted on the way stays deleted and audited
//!   under the batch, so a restore still brings them back.

use axum::Extension;

use crate::audit;
use crate::auth::Claims;
use crate::db::{Db, DbOperation, DbQuery, Traced as _};
use crate::error::ApiError;
use crate::models::DbCard;
use crate::routes::boards::AppState;

/// How many times a cascade sweeps for late children before giving up.
pub(crate) const ATTEMPTS: usize = 5;

/// What a parent's `DELETE` did.
#[derive(Debug)]
pub(crate) enum Removed<T> {
    /// The row was deleted; this is it as it stood at that moment (so its
    /// audit snapshot holds any rename made while the cascade ran).
    Deleted(T),
    /// No such row — a concurrent delete got there first.
    Gone,
    /// A child appeared since the sweep (`foreign_key_violation`).
    HasChildren,
}

/// Run `statement` (a `DELETE … WHERE id = $1 RETURNING *`) and say what it
/// did, turning the restricting foreign key's violation into
/// [`Removed::HasChildren`].
pub(crate) async fn delete_parent<T>(
    db: &Db,
    statement: &'static str,
    id: &str,
    query: DbQuery,
) -> Result<Removed<T>, ApiError>
where
    // What `query_as` needs of a row type: decodable from a Postgres row, and
    // movable across the await (`Send + Unpin`).
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    match sqlx::query_as::<_, T>(statement)
        .bind(id)
        .fetch_optional(db)
        .traced(query)
        .await
    {
        Ok(Some(row)) => Ok(Removed::Deleted(row)),
        Ok(None) => Ok(Removed::Gone),
        // 23503 is `foreign_key_violation`.
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23503") => {
            Ok(Removed::HasChildren)
        }
        Err(error) => Err(error.into()),
    }
}

/// Delete `cards`, each with its links first (so their rows share the
/// batch), and an audit row before the card itself goes.
pub(crate) async fn delete_cards(
    state: &AppState,
    claims: &Extension<Claims>,
    board_id: &str,
    batch: &str,
    cards: Vec<DbCard>,
    name: &'static str,
) -> Result<(), ApiError> {
    for card in cards {
        let entity_id = card.id.clone();
        // The card may have been moved out of this column (or deleted) since
        // the sweep read it; then it is no longer this cascade's to delete.
        // So each card is re-read *and locked* (`FOR UPDATE`) in a transaction
        // that lasts until its row is deleted: a concurrent move or edit waits
        // for the lock and then finds the card gone, instead of slipping in
        // between the audit row below and the delete. The re-read also makes
        // the audit row snapshot the card as it is now, not as the sweep first
        // saw it (an edit made meanwhile would otherwise be lost on restore).
        //
        // The links and the audit row go through the pool, outside the
        // transaction: neither touches the locked `cards` row (deleting a
        // referencing link row takes no lock on the card), so there is nothing
        // for them to wait on.
        let mut tx = state.db.begin().await?;
        let current: Option<DbCard> =
            sqlx::query_as("SELECT * FROM cards WHERE id = $1 AND column_id = $2 FOR UPDATE")
                .bind(&entity_id)
                .bind(&card.column_id)
                .fetch_optional(&mut *tx)
                .traced(DbQuery::new(name, DbOperation::Select, "cards"))
                .await?;
        let Some(card) = current else {
            // Nothing locked; dropping `tx` just ends it.
            continue;
        };
        crate::routes::links::cascade_delete_card_links(
            state,
            claims,
            board_id,
            &entity_id,
            Some(batch),
        )
        .await?;
        let snapshot_before = serde_json::to_value(card.into_api())?;
        audit::record_and_broadcast(
            &state.db,
            &state.events,
            audit::AuditRecord {
                claims,
                board_id: board_id.to_string(),
                entity_type: "card",
                entity_id: &entity_id,
                action: "delete",
                snapshot_before: Some(snapshot_before),
                snapshot_after: None,
                restored_from: None,
                batch_group: Some(batch.to_string()),
                audit_edit_session: None,
            },
        )
        .await?;

        let deleted = sqlx::query("DELETE FROM cards WHERE id = $1")
            .bind(&entity_id)
            .execute(&mut *tx)
            .traced(DbQuery::new(name, DbOperation::Delete, "cards"))
            .await?;
        // The row is locked by this transaction, so it must still be there.
        if deleted.rows_affected() != 1 {
            return Err(ApiError::internal("locked card vanished before its delete"));
        }
        tx.commit().await?;
    }
    Ok(())
}

/// The audit row for a column or board the cascade has just deleted.
pub(crate) async fn record_parent_delete(
    state: &AppState,
    claims: &Extension<Claims>,
    board_id: &str,
    entity_type: &'static str,
    entity_id: &str,
    snapshot_before: serde_json::Value,
    batch: &str,
) -> Result<(), ApiError> {
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        audit::AuditRecord {
            claims,
            board_id: board_id.to_string(),
            entity_type,
            entity_id,
            action: "delete",
            snapshot_before: Some(snapshot_before),
            snapshot_after: None,
            restored_from: None,
            batch_group: Some(batch.to_string()),
            audit_edit_session: None,
        },
    )
    .await?;
    Ok(())
}

/// A cascade that kept losing the race to new children: the 409 the route
/// returns after [`ATTEMPTS`] rounds.
pub(crate) const STILL_BUSY: ApiError =
    ApiError::Conflict(Some("items were added while deleting; try again"));
