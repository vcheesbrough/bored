//! Append-only audit recording + history queries + restore replay.

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

/// Insert one audit row and broadcast `AuditAppended` to the board stream.
pub async fn record_and_broadcast(
    db: &Surreal<Db>,
    events: &Sender<BroadcastEvent>,
    claims: &Claims,
    board_id: String,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    snapshot_before: Option<Value>,
    snapshot_after: Option<Value>,
    restored_from: Option<String>,
    batch_group: Option<String>,
) -> Result<shared::AuditLogEntry, surrealdb::Error> {
    let id = audit_ulid();
    let actor_sub = claims.sub.clone();
    let actor_display_name = claims.display_name();

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
             batch_group = $batch_group \
             RETURN AFTER",
        )
        .bind(("id", id))
        .bind(("actor_sub", actor_sub))
        .bind(("actor_display_name", actor_display_name))
        .bind(("entity_type", entity_type.to_string()))
        .bind(("entity_id", entity_id.to_string()))
        .bind(("board_id", board_id.clone()))
        .bind(("action", action.to_string()))
        .bind(("snapshot_before", snapshot_before))
        .bind(("snapshot_after", snapshot_after))
        .bind(("restored_from", restored_from))
        .bind(("batch_group", batch_group))
        .await?
        .take(0)?;

    let entry = row.expect("audit CREATE must return row").into_api();
    let _ = events.send(BroadcastEvent {
        board_id,
        event: BoardEvent::AuditAppended {
            entry: entry.clone(),
        },
    });
    Ok(entry)
}

pub async fn list_board_history(
    db: &Surreal<Db>,
    board_ulid: &str,
) -> Result<Vec<shared::AuditLogEntry>, surrealdb::Error> {
    let rows: Vec<DbAuditLog> = db
        .query(
            "SELECT * FROM audit_log WHERE board_id = $bid ORDER BY created_at DESC",
        )
        .bind(("bid", board_ulid.to_string()))
        .await?
        .take(0)?;
    Ok(rows.into_iter().map(DbAuditLog::into_api).collect())
}

pub async fn list_column_history(
    db: &Surreal<Db>,
    column_id: &str,
) -> Result<Vec<shared::AuditLogEntry>, surrealdb::Error> {
    let rows: Vec<DbAuditLog> = db
        .query(
            "SELECT * FROM audit_log \
             WHERE entity_type = 'column' AND entity_id = $cid \
             ORDER BY created_at DESC",
        )
        .bind(("cid", column_id.to_string()))
        .await?
        .take(0)?;
    Ok(rows.into_iter().map(DbAuditLog::into_api).collect())
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

async fn load_audit(db: &Surreal<Db>, audit_id: &str) -> Result<Option<DbAuditLog>, surrealdb::Error> {
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
                claims,
                row.board_id.clone(),
                "board",
                &row.entity_id,
                "restore",
                None,
                Some(after),
                Some(original_audit_id),
                None,
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
                claims,
                row.board_id.clone(),
                "column",
                &row.entity_id,
                "restore",
                None,
                Some(after),
                Some(original_audit_id),
                None,
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
            let after =
                serde_json::to_value(card.clone()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let mut out = Vec::new();
            let entry = record_and_broadcast(
                db,
                events,
                claims,
                row.board_id.clone(),
                "card",
                &row.entity_id,
                "restore",
                None,
                Some(after),
                Some(original_audit_id),
                None,
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
