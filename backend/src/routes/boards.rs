use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use surrealdb::{engine::local::Db, Surreal};

use crate::models::DbBoard;

#[derive(Clone)]
pub struct AppState {
    pub db: Surreal<Db>,
}

pub async fn list_boards(
    State(state): State<AppState>,
) -> Result<Json<Vec<shared::Board>>, StatusCode> {
    let boards: Vec<DbBoard> = state
        .db
        .select("boards")
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(boards.into_iter().map(DbBoard::into_api).collect()))
}

pub async fn create_board(
    State(state): State<AppState>,
    Json(payload): Json<shared::CreateBoardRequest>,
) -> Result<(StatusCode, Json<shared::Board>), StatusCode> {
    let id = ulid::Ulid::new().to_string().to_lowercase();

    let board: Option<DbBoard> = state
        .db
        .create(("boards", &id))
        .content(serde_json::json!({ "name": payload.name }))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match board {
        Some(b) => Ok((StatusCode::CREATED, Json(b.into_api()))),
        None => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn get_board(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<shared::Board>, StatusCode> {
    let board: Option<DbBoard> = state
        .db
        .select(("boards", &id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match board {
        Some(b) => Ok(Json(b.into_api())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn update_board(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<shared::UpdateBoardRequest>,
) -> Result<Json<shared::Board>, StatusCode> {
    // Check board exists first
    let existing: Option<DbBoard> = state
        .db
        .select(("boards", &id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if existing.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    let board: Option<DbBoard> = state
        .db
        .update(("boards", &id))
        .merge(serde_json::json!({ "name": payload.name }))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match board {
        Some(b) => Ok(Json(b.into_api())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn delete_board(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    // Delete all columns belonging to this board
    state
        .db
        .query("DELETE columns WHERE board = type::thing('boards', $id)")
        .bind(("id", id.clone()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Delete the board itself
    let _: Option<DbBoard> = state
        .db
        .delete(("boards", &id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}
