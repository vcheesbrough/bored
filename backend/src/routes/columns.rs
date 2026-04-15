use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use crate::models::{DbBoard, DbColumn};
use crate::routes::boards::AppState;

pub async fn list_columns(
    State(state): State<AppState>,
    Path(board_id): Path<String>,
) -> Result<Json<Vec<shared::Column>>, StatusCode> {
    // Verify board exists
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
    // Verify board exists
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
        Some(c) => Ok((StatusCode::CREATED, Json(c.into_api()))),
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
        Some(c) => Ok(Json(c.into_api())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn delete_column(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    match state
        .db
        .delete::<Option<DbColumn>>(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(StatusCode::NOT_FOUND),
    }
}
