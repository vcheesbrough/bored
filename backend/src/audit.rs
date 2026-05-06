//! Audit recording + history queries + restore replay.
//!
//! Card body `update` rows may merge in place when the client repeats the same
//! `audit_edit_session` (one editing stretch). Everything else is append-only.

use axum::http::StatusCode;
use serde_json::{json, Value};
use surrealdb::{engine::local::Db, Surreal};
use tokio::sync::broadcast::Sender;
use ulid::Ulid;

use crate::auth::Claims;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::{DbAuditLog, DbBoard, DbCard, DbColumn};

fn audit_ulid() -> String {
    Ulid::new().to_string().to_lowercase()
}

/// Groups cascade deletes so `POST /api/audit/:id/restore` can replay them in bulk.
pub fn new_batch_group() -> String {
    audit_ulid()
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
    db: &Surreal<Db>,
    events: &Sender<BroadcastEvent>,
    rec: &AuditRecord<'_>,
    session: &str,
) -> Result<Option<shared::AuditLogEntry>, surrealdb::Error> {
    let rows: Vec<DbAuditLog> = db
        .query(
            "SELECT * FROM audit_log \
             WHERE entity_type = 'card' AND entity_id = $eid \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(("eid", rec.entity_id.to_string()))
        .await?
        .take(0)?;
    let Some(last) = rows.into_iter().next() else {
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

    let audit_thing = last.id.id.to_raw();
    let expected_created_at = last.created_at.clone();
    let merged: Vec<DbAuditLog> = db
        .query(
            "UPDATE type::thing('audit_log', $aid) SET \
             snapshot_after = $snapshot_after, \
             created_at = time::now() \
             WHERE created_at = $expected_created_at \
             RETURN AFTER",
        )
        .bind(("aid", audit_thing))
        .bind(("snapshot_after", rec.snapshot_after.clone()))
        .bind(("expected_created_at", expected_created_at))
        .await?
        .take(0)?;

    let Some(db_row) = merged.into_iter().next() else {
        return Err(surrealdb::Error::from(
            surrealdb::error::Api::InternalError("merge audit UPDATE returned no row".into()),
        ));
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
    db: &Surreal<Db>,
    events: &Sender<BroadcastEvent>,
    rec: AuditRecord<'_>,
) -> Result<shared::AuditLogEntry, surrealdb::Error> {
    if rec.entity_type == "card" && rec.action == "update" {
        if let Some(sess) = rec.audit_edit_session.filter(|s| !s.is_empty()) {
            if let Some(entry) = try_merge_card_update_audit(db, events, &rec, sess).await? {
                return Ok(entry);
            }
        }
    }

    let id = audit_ulid();
    let actor_sub = rec.claims.sub.clone();
    let actor_display_name = rec.claims.display_name();
    let aes = rec
        .audit_edit_session
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let row: Option<DbAuditLog> = db
        .query(
            "CREATE type::thing('audit_log', $id) SET \
             actor_sub = $actor_sub, \
             actor_display_name = $actor_display_name, \
             entity_type = $entity_type, \
             entity_id = $entity_id, \
             board_id = $board_id, \
             action = $action, \
             snapshot_before = $snapshot_before, \
             snapshot_after = $snapshot_after, \
             restored_from = $restored_from, \
             batch_group = $batch_group, \
             audit_edit_session = $audit_edit_session \
             RETURN AFTER",
        )
        .bind(("id", id))
        .bind(("actor_sub", actor_sub))
        .bind(("actor_display_name", actor_display_name))
        .bind(("entity_type", rec.entity_type.to_string()))
        .bind(("entity_id", rec.entity_id.to_string()))
        .bind(("board_id", rec.board_id.clone()))
        .bind(("action", rec.action.to_string()))
        .bind(("snapshot_before", rec.snapshot_before))
        .bind(("snapshot_after", rec.snapshot_after))
        .bind(("restored_from", rec.restored_from))
        .bind(("batch_group", rec.batch_group))
        .bind(("audit_edit_session", aes))
        .await?
        .take(0)?;

    let Some(db_row) = row else {
        return Err(surrealdb::Error::from(
            surrealdb::error::Api::InternalError("audit CREATE returned no row".into()),
        ));
    };
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

/// Insert a synthetic **`baseline`** audit row for entities that existed before audit was enabled,
/// using each row's current API snapshot as `snapshot_after`, `updated_at` as `created_at`, and
/// `last_edited_by` (falling back to `"anonymous"`) as `actor_sub` / `actor_display_name`.
///
/// Idempotent per `(entity_type, entity_id)`: skips whenever **any** audit row already exists for
/// that pair so normal mutation history is never duplicated.
///
/// Does **not** broadcast SSE — avoids flooding clients at startup.
pub(crate) async fn migrate_audit_baselines(db: &Surreal<Db>) -> surrealdb::Result<()> {
    #[derive(Debug, serde::Deserialize, Hash, Eq, PartialEq)]
    struct AuditEntityPair {
        entity_type: String,
        entity_id: String,
    }

    let existing: Vec<AuditEntityPair> = db
        .query("SELECT entity_type, entity_id FROM audit_log")
        .await?
        .take(0)
        .unwrap_or_default();
    let covered: std::collections::HashSet<AuditEntityPair> = existing.into_iter().collect();

    let skip = |ty: &str, id: &str, covered: &std::collections::HashSet<AuditEntityPair>| {
        covered.contains(&AuditEntityPair {
            entity_type: ty.to_string(),
            entity_id: id.to_string(),
        })
    };

    async fn insert_baseline_row(
        db: &Surreal<Db>,
        entity_type: &str,
        entity_id: &str,
        board_id: String,
        actor_sub: String,
        snapshot_after: Value,
        created_at: surrealdb::sql::Datetime,
    ) -> surrealdb::Result<()> {
        let id = audit_ulid();
        let actor_display_name = actor_sub.clone();
        db.query(
            "CREATE type::thing('audit_log', $id) SET \
             actor_sub = $actor_sub, \
             actor_display_name = $actor_display_name, \
             entity_type = $entity_type, \
             entity_id = $entity_id, \
             board_id = $board_id, \
             action = 'baseline', \
             snapshot_before = NONE, \
             snapshot_after = $snapshot_after, \
             restored_from = NONE, \
             batch_group = NONE, \
             audit_edit_session = NONE, \
             created_at = $created_at \
             RETURN AFTER",
        )
        .bind(("id", id))
        .bind(("actor_sub", actor_sub))
        .bind(("actor_display_name", actor_display_name))
        .bind(("entity_type", entity_type.to_string()))
        .bind(("entity_id", entity_id.to_string()))
        .bind(("board_id", board_id))
        .bind(("snapshot_after", snapshot_after))
        .bind(("created_at", created_at))
        .await?
        .check()?;
        Ok(())
    }

    let boards: Vec<DbBoard> = db
        .query("SELECT * FROM boards ORDER BY created_at ASC")
        .await?
        .take(0)?;

    for board in boards {
        let entity_id = board.id.id.to_raw();
        if skip("board", &entity_id, &covered) {
            continue;
        }
        let actor_sub = board
            .last_edited_by
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        let ts = board.updated_at.clone();
        let snap =
            serde_json::to_value(board.into_api()).expect("DbBoard serializes to snapshot_after");
        insert_baseline_row(
            db,
            "board",
            &entity_id,
            entity_id.clone(),
            actor_sub,
            snap,
            ts,
        )
        .await?;
    }

    let columns: Vec<DbColumn> = db
        .query("SELECT * FROM columns ORDER BY board ASC, position ASC")
        .await?
        .take(0)?;

    let col_to_board: std::collections::HashMap<String, String> = columns
        .iter()
        .map(|c| (c.id.id.to_raw(), c.board.id.to_raw()))
        .collect();

    for col in columns {
        let entity_id = col.id.id.to_raw();
        if skip("column", &entity_id, &covered) {
            continue;
        }
        let board_id = col.board.id.to_raw();
        let actor_sub = col
            .last_edited_by
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        let ts = col.updated_at.clone();
        let snap =
            serde_json::to_value(col.into_api()).expect("DbColumn serializes to snapshot_after");
        insert_baseline_row(db, "column", &entity_id, board_id, actor_sub, snap, ts).await?;
    }

    let cards: Vec<DbCard> = db
        .query("SELECT * FROM cards ORDER BY column ASC, position ASC")
        .await?
        .take(0)?;

    for card in cards {
        let entity_id = card.id.id.to_raw();
        if skip("card", &entity_id, &covered) {
            continue;
        }
        let Some(board_id) = col_to_board.get(&card.column.id.to_raw()).cloned() else {
            continue;
        };
        let actor_sub = card
            .last_edited_by
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        let ts = card.updated_at.clone();
        let snap =
            serde_json::to_value(card.into_api()).expect("DbCard serializes to snapshot_after");
        insert_baseline_row(db, "card", &entity_id, board_id, actor_sub, snap, ts).await?;
    }

    Ok(())
}

pub async fn list_board_history(
    db: &Surreal<Db>,
    board_ulid: &str,
) -> Result<Vec<shared::AuditLogEntry>, surrealdb::Error> {
    let rows: Vec<DbAuditLog> = db
        .query("SELECT * FROM audit_log WHERE board_id = $bid ORDER BY created_at DESC")
        .bind(("bid", board_ulid.to_string()))
        .await?
        .take(0)?;
    Ok(rows.into_iter().map(DbAuditLog::into_api).collect())
}

/// Column-scoped history: that column's audit rows plus card rows whose before/after
/// snapshots reference the column (same rules as the history drawer).
pub async fn list_column_history(
    db: &Surreal<Db>,
    column_id: &str,
    board_ulid: &str,
) -> Result<Vec<shared::AuditLogEntry>, surrealdb::Error> {
    let all = list_board_history(db, board_ulid).await?;
    Ok(all
        .into_iter()
        .filter(|e| e.matches_history_column_scope(column_id))
        .collect())
}

pub async fn list_card_history(
    db: &Surreal<Db>,
    card_id: &str,
) -> Result<Vec<shared::AuditLogEntry>, surrealdb::Error> {
    let rows: Vec<DbAuditLog> = db
        .query(
            "SELECT * FROM audit_log \
             WHERE entity_type = 'card' AND entity_id = $cid \
             ORDER BY created_at DESC",
        )
        .bind(("cid", card_id.to_string()))
        .await?
        .take(0)?;
    Ok(rows.into_iter().map(DbAuditLog::into_api).collect())
}

async fn load_audit(
    db: &Surreal<Db>,
    audit_id: &str,
) -> Result<Option<DbAuditLog>, surrealdb::Error> {
    db.select(("audit_log", audit_id)).await
}

async fn batch_delete_entries(
    db: &Surreal<Db>,
    batch_group: &str,
) -> Result<Vec<DbAuditLog>, surrealdb::Error> {
    let rows: Vec<DbAuditLog> = db
        .query(
            "SELECT * FROM audit_log WHERE batch_group = $bg AND action = 'delete' ORDER BY created_at DESC",
        )
        .bind(("bg", batch_group.to_string()))
        .await?
        .take(0)?;
    Ok(rows)
}

/// Recreate one entity from a `delete` audit snapshot (`snapshot_before`).
async fn restore_one_delete(
    db: &Surreal<Db>,
    claims: &Claims,
    events: &Sender<BroadcastEvent>,
    row: &DbAuditLog,
) -> Result<Vec<shared::AuditLogEntry>, StatusCode> {
    let snapshot = row
        .snapshot_before
        .clone()
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let original_audit_id = row.id.id.to_raw();
    let editor = claims.sub.clone();

    match row.entity_type.as_str() {
        "board" => {
            let b: shared::Board =
                serde_json::from_value(snapshot).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
            let exists: Option<DbBoard> = db
                .select(("boards", &b.id))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            if exists.is_some() {
                return Err(StatusCode::CONFLICT);
            }
            let _: Option<DbBoard> = db
                .create(("boards", &b.id))
                .content(json!({
                    "name": b.name,
                    "last_edited_by": editor.clone(),
                }))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let after =
                serde_json::to_value(b.clone()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(vec![entry])
        }
        "column" => {
            let c: shared::Column =
                serde_json::from_value(snapshot).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
            let exists: Option<DbColumn> = db
                .select(("columns", &c.id))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            if exists.is_some() {
                return Err(StatusCode::CONFLICT);
            }
            let _: Option<DbColumn> = db
                .query(
                    "CREATE type::thing('columns', $id) SET \
                     board = type::thing('boards', $board_id), \
                     name = $name, position = $position, last_edited_by = $editor",
                )
                .bind(("id", c.id.clone()))
                .bind(("board_id", c.board_id.clone()))
                .bind(("name", c.name.clone()))
                .bind(("position", c.position))
                .bind(("editor", editor.clone()))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .take(0)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let after =
                serde_json::to_value(c.clone()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let _ = events.send(BroadcastEvent {
                board_id: row.board_id.clone(),
                event: BoardEvent::ColumnCreated { column: c },
            });
            Ok(vec![entry])
        }
        "card" => {
            let card: shared::Card =
                serde_json::from_value(snapshot).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
            let exists: Option<DbCard> = db
                .select(("cards", &card.id))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            if exists.is_some() {
                return Err(StatusCode::CONFLICT);
            }
            let _: Option<DbCard> = db
                .query(
                    "CREATE type::thing('cards', $id) SET \
                     column = type::thing('columns', $col_id), \
                     body = $body, position = $position, number = $number, \
                     last_edited_by = $editor",
                )
                .bind(("id", card.id.clone()))
                .bind(("col_id", card.column_id.clone()))
                .bind(("body", card.body.clone()))
                .bind(("position", card.position))
                .bind(("number", card.number as i64))
                .bind(("editor", editor.clone()))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .take(0)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let after = serde_json::to_value(card.clone())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            out.push(entry);
            let _ = events.send(BroadcastEvent {
                board_id: row.board_id.clone(),
                event: BoardEvent::CardCreated { card },
            });
            Ok(out)
        }
        _ => Err(StatusCode::UNPROCESSABLE_ENTITY),
    }
}

async fn restore_batch(
    db: &Surreal<Db>,
    claims: &Claims,
    events: &Sender<BroadcastEvent>,
    batch_group: &str,
) -> Result<Vec<shared::AuditLogEntry>, StatusCode> {
    let rows = batch_delete_entries(db, batch_group)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut restored = Vec::new();
    for r in rows {
        let mut v = restore_one_delete(db, claims, events, &r).await?;
        restored.append(&mut v);
    }
    Ok(restored)
}

/// `POST /api/audit/:id/restore` — replays a `delete`.
///
/// When the referenced row is a **board** or **column** delete that was recorded as
/// part of a cascade batch (`batch_group`), the entire batch is replayed in reverse
/// dependency order. Card deletes that merely share a board-wide batch group still
/// restore only that card unless the referenced audit row is the column/board delete.
pub async fn restore_from_audit(
    db: &Surreal<Db>,
    claims: &Claims,
    events: &Sender<BroadcastEvent>,
    audit_id: &str,
) -> Result<Vec<shared::AuditLogEntry>, StatusCode> {
    let row = load_audit(db, audit_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if row.action != "delete" {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    if let Some(ref bg) = row.batch_group {
        match row.entity_type.as_str() {
            "board" | "column" => restore_batch(db, claims, events, bg).await,
            _ => restore_one_delete(db, claims, events, &row).await,
        }
    } else {
        restore_one_delete(db, claims, events, &row).await
    }
}
