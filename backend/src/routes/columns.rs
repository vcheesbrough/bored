use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};

use crate::auth::Claims;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::DbColumn;
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

    let existing = match existing {
        Some(c) => c,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let board_id = existing.board.id.to_raw();

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
) -> Result<StatusCode, StatusCode> {
    let existing: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let existing = match existing {
        Some(c) => c,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let board_id = existing.board.id.to_raw();

    state
        .db
        .query("DELETE cards WHERE column = type::thing('columns', $id)")
        .bind(("id", col_id.clone()))
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
/// Accepts a complete ordered list of column IDs and assigns `position = index`
/// to each. The board is looked up by slug; the ULID is used for the DB query
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
    for (index, col_id) in payload.order.iter().enumerate() {
        state
            .db
            .query("UPDATE type::thing('columns', $id) SET position = $pos, last_edited_by = $editor WHERE board = type::thing('boards', $board_id)")
            .bind(("id", col_id.clone()))
            .bind(("pos", index as i32))
            .bind(("board_id", board_ulid.clone()))
            .bind(("editor", editor.clone()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

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
