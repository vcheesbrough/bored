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
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Removed {
    /// The row was deleted.
    Deleted,
    /// No such row — a concurrent delete got there first.
    Gone,
    /// A child appeared since the sweep (`foreign_key_violation`).
    HasChildren,
}

/// Run `statement` (a `DELETE … WHERE id = $1`) and say what it did, turning
/// the restricting foreign key's violation into [`Removed::HasChildren`].
pub(crate) async fn delete_parent(
    db: &Db,
    statement: &'static str,
    id: &str,
    query: DbQuery,
) -> Result<Removed, ApiError> {
    match sqlx::query(statement)
        .bind(id)
        .execute(db)
        .traced(query)
        .await
    {
        Ok(result) if result.rows_affected() == 0 => Ok(Removed::Gone),
        Ok(_) => Ok(Removed::Deleted),
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
        // Re-read just before acting, and delete by id *and* column below, so
        // a moved card is never removed from its new home.
        let still_here: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM cards WHERE id = $1 AND column_id = $2)",
        )
        .bind(&entity_id)
        .bind(&card.column_id)
        .fetch_one(&state.db)
        .traced(DbQuery::new(name, DbOperation::Select, "cards"))
        .await?;
        if !still_here {
            continue;
        }
        let column_id = card.column_id.clone();
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

        sqlx::query("DELETE FROM cards WHERE id = $1 AND column_id = $2")
            .bind(&entity_id)
            .bind(&column_id)
            .execute(&state.db)
            .traced(DbQuery::new(name, DbOperation::Delete, "cards"))
            .await?;
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
