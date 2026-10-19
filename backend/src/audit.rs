//! Audit recording + history queries + restore replay.
//!
//! Card body `update` rows may merge in place when the client repeats the same
//! `audit_edit_session` (one editing stretch). Everything else — tag changes
//! included — is append-only.

use serde_json::Value;
use sqlx::types::Json;
use tokio::sync::broadcast::Sender;
use ulid::Ulid;

use crate::auth::Claims;
use crate::db::{Db, DbOperation, DbQuery, Traced as _};
use crate::error::ApiError;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::{DbAuditLog, DbBoard, DbCard, DbColumn};

fn audit_ulid() -> String {
    Ulid::new().to_string().to_lowercase()
}

/// Groups cascade deletes so `POST /api/audit/:id/restore` can replay them in bulk.
pub fn new_batch_group() -> String {
    audit_ulid()
}

/// A restore that would recreate a column whose board, or a card whose
/// column, no longer exists. Restoring the parent first is the fix, so this is
/// the client's 422 — with a message saying which rule it broke — rather than
/// the foreign-key violation (a 500) the insert would otherwise hit.
const PARENT_GONE: ApiError =
    ApiError::Unprocessable(Some("restore the board or column this belonged to first"));

/// The parent tables a restore checks before inserting a child.
#[derive(Clone, Copy)]
enum Parent {
    Board,
    Column,
}

/// Whether the parent row `id` exists.
async fn row_exists(db: &Db, parent: Parent, id: &str) -> Result<bool, sqlx::Error> {
    // One literal statement per table: a table name cannot be a bound
    // parameter, and a literal needs no injection audit.
    let (statement, table) = match parent {
        Parent::Board => (
            "SELECT EXISTS (SELECT 1 FROM boards WHERE id = $1)",
            "boards",
        ),
        Parent::Column => (
            "SELECT EXISTS (SELECT 1 FROM columns WHERE id = $1)",
            "columns",
        ),
    };
    sqlx::query_scalar(statement)
        .bind(id)
        .fetch_one(db)
        .traced(DbQuery::new("audit.row_exists", DbOperation::Select, table))
        .await
}

/// Parameters for [`record_and_broadcast`].
pub struct AuditRecord<'a> {
    pub claims: &'a Claims,
    pub board_id: String,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub snapshot_before: Option<Value>,
    pub snapshot_after: Option<Value>,
    pub restored_from: Option<String>,
    pub batch_group: Option<String>,
    pub audit_edit_session: Option<&'a str>,
}

async fn try_merge_card_update_audit(
    db: &Db,
    events: &Sender<BroadcastEvent>,
    rec: &AuditRecord<'_>,
    session: &str,
) -> Result<Option<shared::AuditLogEntry>, sqlx::Error> {
    let last: Option<DbAuditLog> = sqlx::query_as(
        "SELECT * FROM audit_log WHERE entity_type = 'card' AND entity_id = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(rec.entity_id)
    .fetch_optional(db)
    .traced(DbQuery::new(
        "audit.try_merge_card_update_audit",
        DbOperation::Select,
        "audit_log",
    ))
    .await?;
    let Some(last) = last else {
        return Ok(None);
    };
    let can_merge = last.action == "update"
        && last.actor_sub == rec.claims.sub
        && last.batch_group.is_none()
        && last.restored_from.is_none()
        && last.audit_edit_session.as_deref() == Some(session);
    if !can_merge {
        return Ok(None);
    }

    // An optimistic compare-and-set on `created_at`: if another write merged
    // into (and so restamped) this row since it was read, nothing matches.
    let merged: Option<DbAuditLog> = sqlx::query_as(
        "UPDATE audit_log SET snapshot_after = $2, created_at = clock_timestamp() \
         WHERE id = $1 AND created_at = $3 RETURNING *",
    )
    .bind(&last.id)
    .bind(rec.snapshot_after.clone().map(Json))
    .bind(last.created_at)
    .fetch_optional(db)
    .traced(DbQuery::new(
        "audit.try_merge_card_update_audit",
        DbOperation::Update,
        "audit_log",
    ))
    .await?;

    let Some(db_row) = merged else {
        // Lost optimistic CAS (or row vanished): card write stands — append a fresh audit row.
        return Ok(None);
    };
    let entry = db_row.into_api();
    let board_id = rec.board_id.clone();
    let _ = events.send(BroadcastEvent {
        board_id,
        event: BoardEvent::AuditAppended {
            entry: Box::new(entry.clone()),
        },
    });
    Ok(Some(entry))
}

/// Insert one audit row (or merge a card body update) and broadcast `AuditAppended`.
pub async fn record_and_broadcast(
    db: &Db,
    events: &Sender<BroadcastEvent>,
    rec: AuditRecord<'_>,
) -> Result<shared::AuditLogEntry, sqlx::Error> {
    if rec.entity_type == "card"
        && rec.action == "update"
        && let Some(sess) = rec.audit_edit_session.filter(|s| !s.is_empty())
        && let Some(entry) = try_merge_card_update_audit(db, events, &rec, sess).await?
    {
        return Ok(entry);
    }

    let id = audit_ulid();
    let actor_sub = rec.claims.sub.clone();
    let actor_display_name = rec.claims.display_name();
    let aes = rec
        .audit_edit_session
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // `fetch_one`: an INSERT … RETURNING that succeeds always returns its row,
    // and if it somehow did not, sqlx reports `RowNotFound` — a 500, as the
    // old "CREATE returned no row" error was.
    let db_row: DbAuditLog = sqlx::query_as(
        "INSERT INTO audit_log (id, actor_sub, actor_display_name, entity_type, entity_id, \
         board_id, action, snapshot_before, snapshot_after, restored_from, batch_group, \
         audit_edit_session) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
         RETURNING *",
    )
    .bind(id)
    .bind(actor_sub)
    .bind(actor_display_name)
    .bind(rec.entity_type)
    .bind(rec.entity_id)
    .bind(&rec.board_id)
    .bind(rec.action)
    .bind(rec.snapshot_before.map(Json))
    .bind(rec.snapshot_after.map(Json))
    .bind(rec.restored_from)
    .bind(rec.batch_group)
    .bind(aes)
    .fetch_one(db)
    .traced(DbQuery::new(
        "audit.record_and_broadcast",
        DbOperation::Insert,
        "audit_log",
    ))
    .await?;

    let entry = db_row.into_api();
    let board_id = rec.board_id;
    let _ = events.send(BroadcastEvent {
        board_id,
        event: BoardEvent::AuditAppended {
            entry: Box::new(entry.clone()),
        },
    });
    Ok(entry)
}

pub async fn list_board_history(
    db: &Db,
    board_ulid: &str,
) -> Result<Vec<shared::AuditLogEntry>, sqlx::Error> {
    let rows: Vec<DbAuditLog> = sqlx::query_as(
        "SELECT * FROM audit_log WHERE board_id = $1 ORDER BY created_at DESC, id DESC",
    )
    .bind(board_ulid)
    .fetch_all(db)
    .traced(DbQuery::new(
        "audit.list_board_history",
        DbOperation::Select,
        "audit_log",
    ))
    .await?;
    Ok(rows.into_iter().map(DbAuditLog::into_api).collect())
}

/// Column-scoped history: that column's audit rows plus card rows whose before/after
/// snapshots reference the column (same rules as the history drawer).
pub async fn list_column_history(
    db: &Db,
    column_id: &str,
    board_ulid: &str,
) -> Result<Vec<shared::AuditLogEntry>, sqlx::Error> {
    let all = list_board_history(db, board_ulid).await?;
    Ok(all
        .into_iter()
        .filter(|e| e.matches_history_column_scope(column_id))
        .collect())
}

/// Card-scoped history: the card's own rows plus every `card_link` row with the
/// card at either end, newest first.
///
/// Two queries rather than one `OR`: the card rows hit the
/// `(entity_type, entity_id, created_at)` index directly, and the link rows
/// narrow on `board_id` first (via `audit_board_time`) before the snapshot
/// comparison, rather than scanning every board's `card_link` history. The
/// merged list is re-sorted newest first, on the timestamp and then the id —
/// the same order each query used.
pub async fn list_card_history(
    db: &Db,
    card_id: &str,
    board_ulid: &str,
) -> Result<Vec<shared::AuditLogEntry>, sqlx::Error> {
    let mut rows: Vec<DbAuditLog> = sqlx::query_as(
        "SELECT * FROM audit_log WHERE entity_type = 'card' AND entity_id = $1 \
         ORDER BY created_at DESC, id DESC",
    )
    .bind(card_id)
    .fetch_all(db)
    .traced(DbQuery::new(
        "audit.list_card_history",
        DbOperation::Select,
        "audit_log",
    ))
    .await?;
    // A delete row only has `snapshot_before`, a create row only
    // `snapshot_after`, so both sides are checked. `->>` reads a jsonb field
    // as text (NULL when the snapshot or the key is absent).
    let link_rows: Vec<DbAuditLog> = sqlx::query_as(
        "SELECT * FROM audit_log WHERE board_id = $1 AND entity_type = 'card_link' AND ( \
             snapshot_before->>'predecessor_id' = $2 OR snapshot_before->>'successor_id' = $2 OR \
             snapshot_after->>'predecessor_id' = $2 OR snapshot_after->>'successor_id' = $2 \
         ) ORDER BY created_at DESC, id DESC",
    )
    .bind(board_ulid)
    .bind(card_id)
    .fetch_all(db)
    .traced(DbQuery::new(
        "audit.list_card_history",
        DbOperation::Select,
        "audit_log",
    ))
    .await?;
    rows.extend(link_rows);
    // `cmp` on a tuple compares field by field; `b` before `a` is newest first.
    rows.sort_by(|a, b| (b.created_at, &b.id).cmp(&(a.created_at, &a.id)));
    Ok(rows.into_iter().map(DbAuditLog::into_api).collect())
}

async fn load_audit(db: &Db, audit_id: &str) -> Result<Option<DbAuditLog>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM audit_log WHERE id = $1")
        .bind(audit_id)
        .fetch_optional(db)
        .traced(DbQuery::new(
            "audit.load_audit",
            DbOperation::Select,
            "audit_log",
        ))
        .await
}

async fn batch_delete_entries(db: &Db, batch_group: &str) -> Result<Vec<DbAuditLog>, sqlx::Error> {
    // Link rows are excluded on purpose: a cascade batch records the links it
    // removed, but a deleted link is not replayable — by the time the batch is
    // restored another link may have closed the loop it would complete — so
    // the cards and columns come back without them.
    //
    // Newest first is parent first: a cascade deletes cards, then columns,
    // then the board, so replaying in reverse recreates each parent before
    // the children its foreign keys require.
    sqlx::query_as(
        "SELECT * FROM audit_log WHERE batch_group = $1 AND action = 'delete' \
         AND entity_type <> 'card_link' ORDER BY created_at DESC, id DESC",
    )
    .bind(batch_group)
    .fetch_all(db)
    .traced(DbQuery::new(
        "audit.batch_delete_entries",
        DbOperation::Select,
        "audit_log",
    ))
    .await
}

/// Recreate one entity from a `delete` audit snapshot (`snapshot_before`).
async fn restore_one_delete(
    db: &Db,
    claims: &Claims,
    events: &Sender<BroadcastEvent>,
    row: &DbAuditLog,
) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    let snapshot = row
        .snapshot_before
        .clone()
        .map(|json| json.0)
        .ok_or(ApiError::UNPROCESSABLE)?;
    let original_audit_id = row.id.clone();
    let editor = claims.sub.clone();

    match row.entity_type.as_str() {
        "board" => {
            let b: shared::Board =
                serde_json::from_value(snapshot).map_err(|_| ApiError::UNPROCESSABLE)?;
            let exists: Option<DbBoard> = sqlx::query_as("SELECT * FROM boards WHERE id = $1")
                .bind(&b.id)
                .fetch_optional(db)
                .traced(DbQuery::new(
                    "audit.restore_one_delete",
                    DbOperation::Select,
                    "boards",
                ))
                .await?;
            if exists.is_some() {
                return Err(ApiError::CONFLICT);
            }
            // A board created since under the same name trips
            // `board_name_unique`: a 409, as the API gives any duplicate name.
            let created: Option<DbBoard> = sqlx::query_as(
                "INSERT INTO boards (id, name, last_edited_by) VALUES ($1, $2, $3) RETURNING *",
            )
            .bind(&b.id)
            .bind(&b.name)
            .bind(&editor)
            .fetch_optional(db)
            .traced(DbQuery::new(
                "audit.restore_one_delete",
                DbOperation::Insert,
                "boards",
            ))
            .await?;
            // Announce the row the database just created, not the snapshot it
            // was created from (card #472). The snapshot is history: its
            // `created_at` / `updated_at` are the deleted row's, while the
            // recreated row has fresh ones from the schema defaults.
            let b = created
                .ok_or_else(|| ApiError::internal("restore created no board row"))?
                .into_api();
            let after = serde_json::to_value(b.clone())?;
            let _ = events.send(BroadcastEvent {
                board_id: b.id.clone(),
                event: BoardEvent::BoardCreated { board: b },
            });
            let entry = record_and_broadcast(
                db,
                events,
                AuditRecord {
                    claims,
                    board_id: row.board_id.clone(),
                    entity_type: "board",
                    entity_id: &row.entity_id,
                    action: "restore",
                    snapshot_before: None,
                    snapshot_after: Some(after),
                    restored_from: Some(original_audit_id),
                    batch_group: None,
                    audit_edit_session: None,
                },
            )
            .await?;
            Ok(vec![entry])
        }
        "column" => {
            let c: shared::Column =
                serde_json::from_value(snapshot).map_err(|_| ApiError::UNPROCESSABLE)?;
            let exists: Option<DbColumn> = sqlx::query_as("SELECT * FROM columns WHERE id = $1")
                .bind(&c.id)
                .fetch_optional(db)
                .traced(DbQuery::new(
                    "audit.restore_one_delete",
                    DbOperation::Select,
                    "columns",
                ))
                .await?;
            if exists.is_some() {
                return Err(ApiError::CONFLICT);
            }
            if !row_exists(db, Parent::Board, &c.board_id).await? {
                return Err(PARENT_GONE);
            }
            let created: Option<DbColumn> = sqlx::query_as(
                "INSERT INTO columns (id, board_id, name, position, last_edited_by) \
                 VALUES ($1, $2, $3, $4, $5) RETURNING *",
            )
            .bind(&c.id)
            .bind(&c.board_id)
            .bind(&c.name)
            .bind(c.position)
            .bind(&editor)
            .fetch_optional(db)
            .traced(DbQuery::new(
                "audit.restore_one_delete",
                DbOperation::Insert,
                "columns",
            ))
            .await?;
            // The recreated row, not the snapshot — see the board arm above.
            let c = created
                .ok_or_else(|| ApiError::internal("restore created no column row"))?
                .into_api();
            let after = serde_json::to_value(c.clone())?;
            let entry = record_and_broadcast(
                db,
                events,
                AuditRecord {
                    claims,
                    board_id: row.board_id.clone(),
                    entity_type: "column",
                    entity_id: &row.entity_id,
                    action: "restore",
                    snapshot_before: None,
                    snapshot_after: Some(after),
                    restored_from: Some(original_audit_id),
                    batch_group: None,
                    audit_edit_session: None,
                },
            )
            .await?;
            let _ = events.send(BroadcastEvent {
                board_id: row.board_id.clone(),
                event: BoardEvent::ColumnCreated { column: c },
            });
            Ok(vec![entry])
        }
        "card" => {
            let card: shared::Card =
                serde_json::from_value(snapshot).map_err(|_| ApiError::UNPROCESSABLE)?;
            let exists: Option<DbCard> = sqlx::query_as("SELECT * FROM cards WHERE id = $1")
                .bind(&card.id)
                .fetch_optional(db)
                .traced(DbQuery::new(
                    "audit.restore_one_delete",
                    DbOperation::Select,
                    "cards",
                ))
                .await?;
            if exists.is_some() {
                return Err(ApiError::CONFLICT);
            }
            if !row_exists(db, Parent::Column, &card.column_id).await? {
                return Err(PARENT_GONE);
            }
            // The card keeps its old number when it can. It cannot when the
            // number is no longer free (a snapshot taken before the import
            // renumbered a duplicate, card #471) or was never a real one (0,
            // or too large to have been issued): then it gets a fresh number
            // from the sequence, as a new card would, rather than tripping
            // `cards_number_unique`. `CASE` evaluates `nextval` only on the
            // branch that needs it, so a kept number burns no sequence value.
            let number = i32::try_from(card.number).unwrap_or(0);
            let created: Option<DbCard> = sqlx::query_as(
                "INSERT INTO cards (id, column_id, body, position, number, tags, last_edited_by) \
                 VALUES ($1, $2, $3, $4, \
                     CASE WHEN $5 > 0 AND NOT EXISTS (SELECT 1 FROM cards WHERE number = $5) \
                          THEN $5 ELSE nextval('card_number_seq')::integer END, \
                     $6, $7) RETURNING *",
            )
            .bind(&card.id)
            .bind(&card.column_id)
            .bind(&card.body)
            .bind(card.position)
            .bind(number)
            .bind(&card.tags)
            .bind(&editor)
            .fetch_optional(db)
            .traced(DbQuery::new(
                "audit.restore_one_delete",
                DbOperation::Insert,
                "cards",
            ))
            .await?;
            // The recreated row, not the snapshot — see the board arm above.
            let card = created
                .ok_or_else(|| ApiError::internal("restore created no card row"))?
                .into_api();
            let after = serde_json::to_value(card.clone())?;
            let mut out = Vec::new();
            let entry = record_and_broadcast(
                db,
                events,
                AuditRecord {
                    claims,
                    board_id: row.board_id.clone(),
                    entity_type: "card",
                    entity_id: &row.entity_id,
                    action: "restore",
                    snapshot_before: None,
                    snapshot_after: Some(after),
                    restored_from: Some(original_audit_id),
                    batch_group: None,
                    audit_edit_session: None,
                },
            )
            .await?;
            out.push(entry);
            let _ = events.send(BroadcastEvent {
                board_id: row.board_id.clone(),
                event: BoardEvent::CardCreated { card },
            });
            Ok(out)
        }
        _ => Err(ApiError::UNPROCESSABLE),
    }
}

async fn restore_batch(
    db: &Db,
    claims: &Claims,
    events: &Sender<BroadcastEvent>,
    batch_group: &str,
) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    let rows = batch_delete_entries(db, batch_group).await?;
    let mut restored = Vec::new();
    for r in rows {
        let mut v = restore_one_delete(db, claims, events, &r).await?;
        restored.append(&mut v);
    }
    Ok(restored)
}

/// The card **content** (body + tags) captured by one audit row's
/// `snapshot_after`, or a 422 when the row is not a card version to begin with.
///
/// The tags come back as an `Option`, and the distinction matters: `Some(list)`
/// is a snapshot that genuinely recorded the card's tags (possibly as an empty
/// list), while `None` is a row written *before* tags existed, where the key is
/// simply absent. An absent key is unknown, not empty — treating it as an empty
/// list would let restoring an old body silently delete tags the card has
/// carried ever since. `None` therefore means "this row says nothing about
/// tags", and the caller leaves them alone.
fn card_content_version(row: &DbAuditLog) -> Result<(String, Option<Vec<String>>), ApiError> {
    if row.entity_type != "card"
        || !matches!(
            row.action.as_str(),
            "create" | "baseline" | "update" | "restore"
        )
    {
        return Err(ApiError::UNPROCESSABLE);
    }

    let snapshot = row
        .snapshot_after
        .as_ref()
        .map(|json| &json.0)
        .ok_or(ApiError::UNPROCESSABLE)?;
    if snapshot.get("id").and_then(Value::as_str) != Some(row.entity_id.as_str()) {
        return Err(ApiError::UNPROCESSABLE);
    }
    let body = snapshot
        .get("body")
        .and_then(Value::as_str)
        .ok_or(ApiError::UNPROCESSABLE)?
        .to_string();
    // No `unwrap_or_default()` here: a missing key has to stay `None` all the
    // way to the caller so it can tell "no tags" from "tags not recorded".
    let tags = snapshot.get("tags").and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    });
    Ok((body, tags))
}

/// Restore the content — body **and** tags — of an active card from a
/// historical `snapshot_after`.
///
/// Both move together on purpose: a version in the drawer is one moment in the
/// card's life, so restoring it puts the card back to that moment rather than
/// to a hybrid of an old body and today's tags.
///
/// The one exception is a row from before tags existed ([`card_content_version`]
/// returns `None`). Such a row never recorded the card's tags, so there is no
/// "moment" to put them back to — the restore touches the body only and leaves
/// today's tags standing, rather than deleting them on the strength of a field
/// the snapshot never had.
async fn restore_card_content(
    db: &Db,
    claims: &Claims,
    events: &Sender<BroadcastEvent>,
    row: &DbAuditLog,
) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    let (target_body, target_tags) = card_content_version(row)?;
    let existing: Option<DbCard> = sqlx::query_as("SELECT * FROM cards WHERE id = $1")
        .bind(&row.entity_id)
        .fetch_optional(db)
        .traced(DbQuery::new(
            "audit.restore_card_content",
            DbOperation::Select,
            "cards",
        ))
        .await?;
    let existing = existing.ok_or(ApiError::NotFound)?;
    // Nothing to restore only when every half this row actually carries already
    // matches. A legacy row (`None`) carries no tags, so the body alone decides
    // — otherwise today's tags, which the restore will not touch, could make an
    // already-current row look restorable.
    let tags_already_current = target_tags
        .as_ref()
        .is_none_or(|target| existing.tags == *target);
    if existing.body == target_body && tags_already_current {
        return Err(ApiError::CONFLICT);
    }

    let snapshot_before = serde_json::to_value(existing.into_api())?;
    // `COALESCE($4, tags)`: for a legacy row `target_tags` is `None`, bound as
    // NULL, so the column keeps whatever the card carries today.
    let updated: Option<DbCard> = sqlx::query_as(
        "UPDATE cards SET body = $2, tags = COALESCE($4, tags), last_edited_by = $3 \
         WHERE id = $1 RETURNING *",
    )
    .bind(&row.entity_id)
    .bind(target_body)
    .bind(&claims.sub)
    .bind(target_tags)
    .fetch_optional(db)
    .traced(DbQuery::new(
        "audit.restore_card_content",
        DbOperation::Update,
        "cards",
    ))
    .await?;
    let updated = updated.ok_or(ApiError::NotFound)?.into_api();
    let snapshot_after = serde_json::to_value(updated.clone())?;
    let original_audit_id = row.id.clone();

    let entry = record_and_broadcast(
        db,
        events,
        AuditRecord {
            claims,
            board_id: row.board_id.clone(),
            entity_type: "card",
            entity_id: &row.entity_id,
            action: "restore",
            snapshot_before: Some(snapshot_before),
            snapshot_after: Some(snapshot_after),
            restored_from: Some(original_audit_id),
            batch_group: None,
            audit_edit_session: None,
        },
    )
    .await?;

    let _ = events.send(BroadcastEvent {
        board_id: row.board_id.clone(),
        event: BoardEvent::CardUpdated { card: updated },
    });
    Ok(vec![entry])
}

/// `POST /api/audit/:id/restore` — replays a delete or restores an active card's content.
///
/// Card content restores read `snapshot_after.body` and `snapshot_after.tags` from a
/// card create, baseline, update, or earlier restore row. Delete restores retain the
/// cascade behavior below.
///
/// When the referenced row is a **board** or **column** delete that was recorded as
/// part of a cascade batch (`batch_group`), the entire batch is replayed in reverse
/// dependency order. Card deletes that merely share a board-wide batch group still
/// restore only that card unless the referenced audit row is the column/board delete.
pub async fn restore_from_audit(
    db: &Db,
    claims: &Claims,
    events: &Sender<BroadcastEvent>,
    audit_id: &str,
) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    let row = load_audit(db, audit_id).await?.ok_or(ApiError::NotFound)?;

    if row.action == "delete" {
        if let Some(ref bg) = row.batch_group {
            match row.entity_type.as_str() {
                "board" | "column" => restore_batch(db, claims, events, bg).await,
                _ => restore_one_delete(db, claims, events, &row).await,
            }
        } else {
            restore_one_delete(db, claims, events, &row).await
        }
    } else {
        restore_card_content(db, claims, events, &row).await
    }
}

// `#[cfg(test)]` keeps this module out of the shipped binary — it is compiled
// only under `cargo test`.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build an audit row whose `snapshot_after` is exactly `snapshot`, so a
    /// test can control precisely which keys the snapshot carries.
    fn card_row(snapshot: Value) -> DbAuditLog {
        DbAuditLog {
            id: "01jtestauditrow".to_string(),
            actor_sub: "user-1".to_string(),
            actor_display_name: "Test User".to_string(),
            entity_type: "card".to_string(),
            entity_id: "card-1".to_string(),
            board_id: "board-1".to_string(),
            action: "update".to_string(),
            snapshot_before: None,
            snapshot_after: Some(Json(snapshot)),
            restored_from: None,
            batch_group: None,
            audit_edit_session: None,
            created_at: chrono::DateTime::UNIX_EPOCH,
        }
    }

    /// A row written before tags existed has no `tags` key at all. That must
    /// surface as `None` — "not recorded" — rather than an empty list, because
    /// the restore path uses it to decide whether to touch the card's tags.
    #[test]
    fn legacy_snapshot_without_tags_key_yields_none() {
        let row = card_row(json!({ "id": "card-1", "body": "old body" }));

        let (body, tags) = card_content_version(&row).expect("legacy row is a valid card version");

        assert_eq!(body, "old body");
        assert_eq!(
            tags, None,
            "a snapshot with no `tags` key says nothing about tags, so restoring \
             it must not clear the tags the card carries today"
        );
    }

    /// A snapshot recorded *since* tags shipped always has the key, so an empty
    /// array is a genuine "this card had no tags" and has to stay
    /// distinguishable from the legacy case above.
    #[test]
    fn empty_tags_array_is_recorded_not_absent() {
        let row = card_row(json!({ "id": "card-1", "body": "b", "tags": [] }));

        let (_, tags) = card_content_version(&row).expect("valid card version");

        assert_eq!(tags, Some(Vec::new()));
    }

    /// The ordinary case: a populated array round-trips in order.
    #[test]
    fn populated_tags_are_parsed_in_order() {
        let row = card_row(json!({ "id": "card-1", "body": "b", "tags": ["bug", "urgent"] }));

        let (_, tags) = card_content_version(&row).expect("valid card version");

        assert_eq!(tags, Some(vec!["bug".to_string(), "urgent".to_string()]));
    }

    /// Non-string entries are skipped rather than aborting the whole restore:
    /// the snapshot is data we already wrote, so the tolerant read keeps one
    /// malformed entry from making a version permanently unrestorable.
    #[test]
    fn non_string_tag_entries_are_skipped() {
        let row = card_row(json!({ "id": "card-1", "body": "b", "tags": ["bug", 7, null] }));

        let (_, tags) = card_content_version(&row).expect("valid card version");

        assert_eq!(tags, Some(vec!["bug".to_string()]));
    }

    /// A snapshot belonging to a different card must never be restorable onto
    /// this row's entity.
    #[test]
    fn mismatched_snapshot_id_is_rejected() {
        let row = card_row(json!({ "id": "card-2", "body": "b", "tags": [] }));

        // A bodiless 422, as the restore route has always answered.
        assert!(matches!(
            card_content_version(&row).unwrap_err(),
            ApiError::Unprocessable(None)
        ));
    }
}
