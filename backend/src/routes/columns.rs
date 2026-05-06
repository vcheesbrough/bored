use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};

use crate::audit;
use crate::auth::Claims;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::{DbCard, DbColumn};
use crate::routes::boards::{editor_sub, find_board_by_slug, AppState};

pub async fn list_columns(
    State(state): State<AppState>,
    Path(board_slug): Path<String>,
) -> Result<Json<Vec<shared::Column>>, StatusCode> {
    let board = match find_board_by_slug(&state.db, &board_slug).await? {
        Some(b) => b,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let board_ulid = board.id.id.to_raw();

    let columns: Vec<DbColumn> = state
        .db
        .query(
            "SELECT * FROM columns WHERE board = type::thing('boards', $id) ORDER BY position ASC",
        )
        .bind(("id", board_ulid))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(columns.into_iter().map(DbColumn::into_api).collect()))
}

pub async fn create_column(
    State(state): State<AppState>,
    Path(board_slug): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::CreateColumnRequest>,
) -> Result<(StatusCode, Json<shared::Column>), StatusCode> {
    let board = match find_board_by_slug(&state.db, &board_slug).await? {
        Some(b) => b,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let board_ulid = board.id.id.to_raw();

    let id = ulid::Ulid::new().to_string().to_lowercase();
    let editor = editor_sub(&claims);

    let column: Option<DbColumn> = state
        .db
        .query("CREATE type::thing('columns', $id) SET board = type::thing('boards', $board_id), name = $name, position = $position, last_edited_by = $editor")
        .bind(("id", id))
        .bind(("board_id", board_ulid.clone()))
        .bind(("name", payload.name))
        .bind(("position", payload.position))
        .bind(("editor", editor))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match column {
        Some(c) => {
            let api_col = c.into_api();
            let snapshot_after = serde_json::to_value(api_col.clone())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            audit::record_and_broadcast(
                &state.db,
                &state.events,
                audit::AuditRecord {
                    claims: &claims,
                    board_id: board_ulid.clone(),
                    entity_type: "column",
                    entity_id: &api_col.id,
                    action: "create",
                    snapshot_before: None,
                    snapshot_after: Some(snapshot_after),
                    restored_from: None,
                    batch_group: None,
                },
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let _ = state.events.send(BroadcastEvent {
                board_id: board_ulid,
                event: BoardEvent::ColumnCreated {
                    column: api_col.clone(),
                },
            });
            Ok((StatusCode::CREATED, Json(api_col)))
        }
        None => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn update_column(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::UpdateColumnRequest>,
) -> Result<Json<shared::Column>, StatusCode> {
    let existing: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Destructure early to capture the board ID for the SSE event.
    let existing = match existing {
        Some(c) => c,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let board_id = existing.board.id.to_raw();

    // Build a partial update map from whichever fields were supplied.
    let mut patch = serde_json::Map::new();
    if let Some(name) = payload.name {
        patch.insert("name".to_string(), serde_json::Value::String(name));
    }
    if let Some(position) = payload.position {
        patch.insert(
            "position".to_string(),
            serde_json::Value::Number(position.into()),
        );
    }
    if patch.is_empty() {
        return Ok(Json(existing.into_api()));
    }
    let snapshot_before = serde_json::to_value(existing.clone().into_api())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    patch.insert(
        "last_edited_by".to_string(),
        serde_json::Value::String(editor_sub(&claims)),
    );

    let column: Option<DbColumn> = state
        .db
        .update(("columns", &col_id))
        .merge(serde_json::Value::Object(patch))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match column {
        Some(c) => {
            let api_col = c.into_api();
            let snapshot_after = serde_json::to_value(api_col.clone())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let action = if payload.position.is_some() {
                "move"
            } else {
                "update"
            };

            audit::record_and_broadcast(
                &state.db,
                &state.events,
                audit::AuditRecord {
                    claims: &claims,
                    board_id: board_id.clone(),
                    entity_type: "column",
                    entity_id: &api_col.id,
                    action,
                    snapshot_before: Some(snapshot_before),
                    snapshot_after: Some(snapshot_after),
                    restored_from: None,
                    batch_group: None,
                },
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let _ = state.events.send(BroadcastEvent {
                board_id,
                event: BoardEvent::ColumnUpdated {
                    column: api_col.clone(),
                },
            });
            Ok(Json(api_col))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn delete_column(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    claims: Extension<Claims>,
) -> Result<StatusCode, StatusCode> {
    let existing: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Destructure early to capture the board ID for the SSE event.
    let existing = match existing {
        Some(c) => c,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let board_id = existing.board.id.to_raw();
    let batch = audit::new_batch_group();

    let cards: Vec<DbCard> = state
        .db
        .query(
            "SELECT * FROM cards WHERE column = type::thing('columns', $cid) ORDER BY position ASC",
        )
        .bind(("cid", col_id.clone()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    for card in cards {
        let entity_id = card.id.id.to_raw();
        let snapshot_before = serde_json::to_value(card.clone().into_api())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        audit::record_and_broadcast(
            &state.db,
            &state.events,
            audit::AuditRecord {
                claims: &claims,
                board_id: board_id.clone(),
                entity_type: "card",
                entity_id: &entity_id,
                action: "delete",
                snapshot_before: Some(snapshot_before),
                snapshot_after: None,
                restored_from: None,
                batch_group: Some(batch.clone()),
            },
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let _: Option<DbCard> = state
            .db
            .delete(("cards", &entity_id))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let col_snap = serde_json::to_value(existing.clone().into_api())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        audit::AuditRecord {
            claims: &claims,
            board_id: board_id.clone(),
            entity_type: "column",
            entity_id: &col_id,
            action: "delete",
            snapshot_before: Some(col_snap),
            snapshot_after: None,
            restored_from: None,
            batch_group: Some(batch),
        },
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    state
        .db
        .delete::<Option<DbColumn>>(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let _ = state.events.send(BroadcastEvent {
        board_id,
        event: BoardEvent::ColumnDeleted { column_id: col_id },
    });

    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/boards/:slug/columns/reorder`
///
/// Accepts a complete ordered list of column IDs (`{ order: ["id1","id2",…] }`)
/// and assigns `position = index` to each. This is a bulk operation — the client
/// sends the full desired order and the server rewrites every position in one
/// pass. Column IDs not present in the list are skipped (their position is
/// unchanged), so the caller must include every column to guarantee a consistent
/// result. The board is looked up by slug; the ULID is used for the DB query
/// guard that prevents cross-board IDOR writes.
pub async fn reorder_columns(
    State(state): State<AppState>,
    Path(board_slug): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::ColumnsReorderRequest>,
) -> Result<Json<Vec<shared::Column>>, StatusCode> {
    let board = match find_board_by_slug(&state.db, &board_slug).await? {
        Some(b) => b,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let board_ulid = board.id.id.to_raw();

    let editor = editor_sub(&claims);
    let batch = audit::new_batch_group();

    // Update each column's position to its index in the supplied order.
    // The WHERE clause scopes the update to this board, so a foreign column ID
    // silently no-ops (matches zero rows) rather than mutating another board's
    // state — preventing cross-board IDOR writes.
    for (index, col_id) in payload.order.iter().enumerate() {
        let before: Option<DbColumn> = state
            .db
            .select(("columns", col_id.as_str()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let Some(col_before) = before else {
            continue;
        };
        if col_before.board.id.to_raw() != board_ulid {
            continue;
        }
        if col_before.position == index as i32 {
            continue;
        }

        let snapshot_before = serde_json::to_value(col_before.into_api())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        state
            .db
            .query(
                "UPDATE type::thing('columns', $id) SET position = $pos, last_edited_by = $editor \
                 WHERE board = type::thing('boards', $board_id)",
            )
            .bind(("id", col_id.clone()))
            .bind(("pos", index as i32))
            .bind(("board_id", board_ulid.clone()))
            .bind(("editor", editor.clone()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let after: Option<DbColumn> = state
            .db
            .select(("columns", col_id.as_str()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let Some(col_after) = after else {
            continue;
        };
        let snapshot_after = serde_json::to_value(col_after.into_api())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        audit::record_and_broadcast(
            &state.db,
            &state.events,
            audit::AuditRecord {
                claims: &claims,
                board_id: board_ulid.clone(),
                entity_type: "column",
                entity_id: col_id.as_str(),
                action: "move",
                snapshot_before: Some(snapshot_before),
                snapshot_after: Some(snapshot_after),
                restored_from: None,
                batch_group: Some(batch.clone()),
            },
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    // Re-fetch the full ordered list so we can return it and broadcast it.
    let columns: Vec<DbColumn> = state
        .db
        .query(
            "SELECT * FROM columns WHERE board = type::thing('boards', $id) ORDER BY position ASC",
        )
        .bind(("id", board_ulid.clone()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let api_cols: Vec<shared::Column> = columns.into_iter().map(DbColumn::into_api).collect();

    let _ = state.events.send(BroadcastEvent {
        board_id: board_ulid,
        event: BoardEvent::ColumnsReordered {
            columns: api_cols.clone(),
        },
    });

    Ok(Json(api_cols))
}
