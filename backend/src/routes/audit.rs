use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};

use crate::audit;
use crate::auth::Claims;
use crate::models::DbColumn;
use crate::routes::boards::{find_board_by_slug, AppState};

pub async fn board_history(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Vec<shared::AuditLogEntry>>, StatusCode> {
    let board = match find_board_by_slug(&state.db, &slug).await? {
        Some(b) => b,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let id = board.id.id.to_raw();
    let rows = audit::list_board_history(&state.db, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows))
}

pub async fn column_history(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
) -> Result<Json<Vec<shared::AuditLogEntry>>, StatusCode> {
    let col: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(col) = col else {
        return Err(StatusCode::NOT_FOUND);
    };
    let board_ulid = col.board.id.to_raw();
    let rows = audit::list_column_history(&state.db, &col_id, &board_ulid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows))
}

pub async fn card_history(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> Result<Json<Vec<shared::AuditLogEntry>>, StatusCode> {
    let card: Option<crate::models::DbCard> = state
        .db
        .select(("cards", &card_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if card.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let rows = audit::list_card_history(&state.db, &card_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows))
}

pub async fn restore_audit(
    State(state): State<AppState>,
    Path(audit_id): Path<String>,
    claims: Extension<Claims>,
) -> Result<Json<Vec<shared::AuditLogEntry>>, StatusCode> {
    let rows = audit::restore_from_audit(&state.db, &claims, &state.events, &audit_id).await?;
    Ok(Json(rows))
}
