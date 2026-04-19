use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use crate::events::BoardEvent;
use crate::models::{DbBoard, DbColumn};
use crate::routes::boards::AppState;

pub async fn list_columns(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
) -> Result<Json<Vec<shared::Column>>, StatusCode> {
    // Verify the board exists so we return 404 rather than an empty list for
    // unknown board IDs — callers shouldn't silently see zero columns.
    let board: Option<DbBoard> = state
        .db
        .select(("boards", &board_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if board.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    let columns: Vec<DbColumn> = state
        .db
        .query(
            "SELECT * FROM columns WHERE board = type::thing('boards', $id) ORDER BY position ASC",
        )
        .bind(("id", board_id.clone()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(columns.into_iter().map(DbColumn::into_api).collect()))
}

pub async fn create_column(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    Json(payload): Json<shared::CreateColumnRequest>,
) -> Result<(StatusCode, Json<shared::Column>), StatusCode> {
    let board: Option<DbBoard> = state
        .db
        .select(("boards", &board_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if board.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    let id = ulid::Ulid::new().to_string().to_lowercase();

    let column: Option<DbColumn> = state
        .db
        .query("CREATE type::thing('columns', $id) SET board = type::thing('boards', $board_id), name = $name, position = $position")
        .bind(("id", id))
        .bind(("board_id", board_id))
        .bind(("name", payload.name))
        .bind(("position", payload.position))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match column {
        Some(c) => {
            let api_col = c.into_api();
            let _ = state.events.send(BoardEvent::ColumnCreated {
                column: api_col.clone(),
            });
            Ok((StatusCode::CREATED, Json(api_col)))
        }
        None => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn update_column(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    Json(payload): Json<shared::UpdateColumnRequest>,
) -> Result<Json<shared::Column>, StatusCode> {
    let existing: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if existing.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

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

    let column: Option<DbColumn> = state
        .db
        .update(("columns", &col_id))
        .merge(serde_json::Value::Object(patch))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match column {
        Some(c) => {
            let api_col = c.into_api();
            let _ = state.events.send(BoardEvent::ColumnUpdated {
                column: api_col.clone(),
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

    if existing.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    // Cascade: delete all cards in this column before deleting the column itself.
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

    let _ = state
        .events
        .send(BoardEvent::ColumnDeleted { column_id: col_id });

    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/boards/:id/columns/reorder`
///
/// Accepts a complete ordered list of column IDs (`{ order: ["id1","id2",…] }`)
/// and assigns `position = index` to each. This is a bulk operation — the client
/// sends the full desired order and the server rewrites every position in one
/// transaction. Column IDs not present in the list are skipped (their position
/// is unchanged), so the caller must include every column to guarantee a
/// consistent result.
pub async fn reorder_columns(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
    Json(payload): Json<shared::ColumnsReorderRequest>,
) -> Result<Json<Vec<shared::Column>>, StatusCode> {
    // Verify the board exists before touching any columns.
    let board: Option<DbBoard> = state
        .db
        .select(("boards", &board_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if board.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    // Update each column's position to its index in the supplied order.
    // The WHERE clause scopes the update to this board, so a foreign column ID
    // silently no-ops (matches zero rows) rather than mutating another board's
    // state — preventing cross-board IDOR writes.
    for (index, col_id) in payload.order.iter().enumerate() {
        state
            .db
            .query("UPDATE type::thing('columns', $id) SET position = $pos WHERE board = type::thing('boards', $board_id)")
            .bind(("id", col_id.clone()))
            .bind(("pos", index as i32))
            .bind(("board_id", board_id.clone()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    // Re-fetch the full ordered list so we can return it and broadcast it.
    let columns: Vec<DbColumn> = state
        .db
        .query(
            "SELECT * FROM columns WHERE board = type::thing('boards', $id) ORDER BY position ASC",
        )
        .bind(("id", board_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let api_cols: Vec<shared::Column> = columns.into_iter().map(DbColumn::into_api).collect();

    let _ = state.events.send(BoardEvent::ColumnsReordered {
        columns: api_cols.clone(),
    });

    Ok(Json(api_cols))
}
