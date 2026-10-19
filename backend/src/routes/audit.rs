use axum::{
    Extension, Json,
    extract::{Path, State},
};

use crate::audit;
use crate::auth::Claims;
use crate::db::{DbOperation, DbQuery, Traced as _};
use crate::error::ApiError;
use crate::models::DbColumn;
use crate::routes::boards::{AppState, find_board_by_slug};

#[tracing::instrument(skip_all, fields(bored.board.slug = %slug))]
pub async fn board_history(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Vec<shared::AuditLogEntry>>, ApiError> {
    let board = match find_board_by_slug(&state.db, &slug).await? {
        Some(b) => b,
        None => return Err(ApiError::NotFound),
    };
    let rows = audit::list_board_history(&state.db, &board.id).await?;
    Ok(Json(rows))
}

#[tracing::instrument(skip_all, fields(bored.column.id = %col_id))]
pub async fn column_history(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
) -> Result<Json<Vec<shared::AuditLogEntry>>, ApiError> {
    let col: Option<DbColumn> = sqlx::query_as("SELECT * FROM columns WHERE id = $1")
        .bind(&col_id)
        .fetch_optional(&state.db)
        .traced(DbQuery::new(
            "audit.column_history",
            DbOperation::Select,
            "columns",
        ))
        .await?;
    let Some(col) = col else {
        return Err(ApiError::NotFound);
    };
    let board_ulid = col.board_id;
    let rows = audit::list_column_history(&state.db, &col_id, &board_ulid).await?;
    Ok(Json(rows))
}

#[tracing::instrument(skip_all, fields(bored.card.id = %card_id))]
pub async fn card_history(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> Result<Json<Vec<shared::AuditLogEntry>>, ApiError> {
    let card: Option<crate::models::DbCard> = sqlx::query_as("SELECT * FROM cards WHERE id = $1")
        .bind(&card_id)
        .fetch_optional(&state.db)
        .traced(DbQuery::new(
            "audit.card_history",
            DbOperation::Select,
            "cards",
        ))
        .await?;
    let Some(card) = card else {
        return Err(ApiError::NotFound);
    };
    let col: Option<DbColumn> = sqlx::query_as("SELECT * FROM columns WHERE id = $1")
        .bind(&card.column_id)
        .fetch_optional(&state.db)
        .traced(DbQuery::new(
            "audit.card_history",
            DbOperation::Select,
            "columns",
        ))
        .await?;
    // The column is normally still there; if it is not, the card's own history
    // rows still come back and only the link half narrows to nothing rather
    // than scanning every board's link history.
    let board_ulid = col.map(|c| c.board_id).unwrap_or_default();
    let rows = audit::list_card_history(&state.db, &card_id, &board_ulid).await?;
    Ok(Json(rows))
}

#[tracing::instrument(skip_all, fields(bored.audit.id = %audit_id))]
pub async fn restore_audit(
    State(state): State<AppState>,
    Path(audit_id): Path<String>,
    claims: Extension<Claims>,
) -> Result<Json<Vec<shared::AuditLogEntry>>, ApiError> {
    let rows = audit::restore_from_audit(&state.db, &claims, &state.events, &audit_id).await?;
    Ok(Json(rows))
}
