use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use crate::models::{DbCard, DbColumn};
use crate::routes::boards::AppState;

pub async fn list_cards(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
) -> Result<Json<Vec<shared::Card>>, StatusCode> {
    let column: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if column.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    let cards: Vec<DbCard> = state
        .db
        .query(
            "SELECT * FROM cards WHERE column = type::thing('columns', $id) ORDER BY position ASC",
        )
        .bind(("id", col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(cards.into_iter().map(DbCard::into_api).collect()))
}

pub async fn create_card(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    Json(payload): Json<shared::CreateCardRequest>,
) -> Result<(StatusCode, Json<shared::Card>), StatusCode> {
    let column: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if column.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    let id = ulid::Ulid::new().to_string().to_lowercase();

    // Derive next_position atomically inside the CREATE to avoid TOCTOU races.
    let card: Option<DbCard> = state
        .db
        .query(
            "CREATE type::thing('cards', $id) SET \
             column = type::thing('columns', $col_id), \
             title = $title, \
             description = $description, \
             position = (array::max((SELECT VALUE position FROM cards WHERE column = type::thing('columns', $col_id))) ?? -1) + 1",
        )
        .bind(("id", id))
        .bind(("col_id", col_id))
        .bind(("title", payload.title))
        .bind(("description", payload.description))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => Ok((StatusCode::CREATED, Json(c.into_api()))),
        None => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn update_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    Json(payload): Json<shared::UpdateCardRequest>,
) -> Result<Json<shared::Card>, StatusCode> {
    let existing: Option<DbCard> = state
        .db
        .select(("cards", &card_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let existing = match existing {
        Some(e) => e,
        None => return Err(StatusCode::NOT_FOUND),
    };

    // Validate target column if provided, and guard against cross-board moves.
    if let Some(ref col_id) = payload.column_id {
        let target_col: Option<DbColumn> = state
            .db
            .select(("columns", col_id.as_str()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let target_col = match target_col {
            Some(c) => c,
            None => return Err(StatusCode::NOT_FOUND),
        };

        let current_col: Option<DbColumn> = state
            .db
            .select(("columns", existing.column.id.to_raw()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        if let Some(current_col) = current_col {
            if current_col.board.id.to_raw() != target_col.board.id.to_raw() {
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
        }
    }

    // Build a single atomic UPDATE covering all changed fields.
    let mut set_parts: Vec<String> = Vec::new();

    if payload.column_id.is_some() {
        set_parts.push("column = type::thing('columns', $col_id)".to_string());
    }
    if payload.title.is_some() {
        set_parts.push("title = $title".to_string());
    }
    if let Some(ref desc) = payload.description {
        if desc.is_some() {
            set_parts.push("description = $description".to_string());
        } else {
            set_parts.push("description = NONE".to_string());
        }
    }
    if payload.position.is_some() {
        set_parts.push("position = $position".to_string());
    }

    if set_parts.is_empty() {
        return Ok(Json(existing.into_api()));
    }

    let query_str = format!(
        "UPDATE type::thing('cards', $card_id) SET {}",
        set_parts.join(", ")
    );

    let mut q = state.db.query(query_str).bind(("card_id", card_id));
    if let Some(col_id) = payload.column_id {
        q = q.bind(("col_id", col_id));
    }
    if let Some(title) = payload.title {
        q = q.bind(("title", title));
    }
    if let Some(Some(desc)) = payload.description {
        q = q.bind(("description", desc));
    }
    if let Some(position) = payload.position {
        q = q.bind(("position", position));
    }

    let card: Option<DbCard> = q
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => Ok(Json(c.into_api())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn delete_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    match state
        .db
        .delete::<Option<DbCard>>(("cards", &card_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn move_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    Json(payload): Json<shared::MoveCardRequest>,
) -> Result<Json<shared::Card>, StatusCode> {
    let existing: Option<DbCard> = state
        .db
        .select(("cards", &card_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let existing = match existing {
        Some(e) => e,
        None => return Err(StatusCode::NOT_FOUND),
    };

    let target_col: Option<DbColumn> = state
        .db
        .select(("columns", &payload.column_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let target_col = match target_col {
        Some(c) => c,
        None => return Err(StatusCode::NOT_FOUND),
    };

    // Guard: target column must belong to the same board as the card's current column.
    let current_col: Option<DbColumn> = state
        .db
        .select(("columns", existing.column.id.to_raw()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Some(current_col) = current_col {
        if current_col.board.id.to_raw() != target_col.board.id.to_raw() {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    let card: Option<DbCard> = state
        .db
        .query("UPDATE type::thing('cards', $card_id) SET column = type::thing('columns', $col_id), position = $position")
        .bind(("card_id", card_id))
        .bind(("col_id", payload.column_id))
        .bind(("position", payload.position))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => Ok(Json(c.into_api())),
        None => Err(StatusCode::NOT_FOUND),
    }
}
