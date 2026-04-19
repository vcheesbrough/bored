use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use crate::events::BoardEvent;
use crate::models::{DbCard, DbColumn};
use crate::routes::boards::AppState;

pub async fn list_cards(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
) -> Result<Json<Vec<shared::Card>>, StatusCode> {
    // Verify the column exists before returning its cards.
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

pub async fn get_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> Result<Json<shared::Card>, StatusCode> {
    // Direct lookup by primary key — SurrealDB returns None if the record
    // doesn't exist, which we surface as 404 rather than an internal error.
    let card: Option<DbCard> = state
        .db
        .select(("cards", &card_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => Ok(Json(c.into_api())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn create_card(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    Json(payload): Json<shared::CreateCardRequest>,
) -> Result<(StatusCode, Json<shared::Card>), StatusCode> {
    // Reject empty bodies — a card with no content is not useful.
    if payload.body.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

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
             body = $body, \
             position = (array::max((SELECT VALUE position FROM cards WHERE column = type::thing('columns', $col_id))) ?? -1) + 1",
        )
        .bind(("id", id))
        .bind(("col_id", col_id))
        .bind(("body", payload.body))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => {
            let api_card = c.into_api();
            let _ = state.events.send(BoardEvent::CardCreated {
                card: api_card.clone(),
            });
            Ok((StatusCode::CREATED, Json(api_card)))
        }
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

    // Reject an explicitly-supplied body that is empty or whitespace-only,
    // consistent with the same guard on create_card.
    if let Some(ref body) = payload.body {
        if body.trim().is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

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

    if payload.body.is_some() {
        set_parts.push("body = $body".to_string());
    }
    if payload.column_id.is_some() {
        set_parts.push("column = type::thing('columns', $col_id)".to_string());
    }
    if payload.position.is_some() {
        set_parts.push("position = $position".to_string());
    }

    // Nothing changed — return the existing card unchanged.
    if set_parts.is_empty() {
        return Ok(Json(existing.into_api()));
    }

    let query_str = format!(
        "UPDATE type::thing('cards', $card_id) SET {}",
        set_parts.join(", ")
    );

    let mut q = state.db.query(query_str).bind(("card_id", card_id));
    if let Some(body) = payload.body {
        q = q.bind(("body", body));
    }
    if let Some(col_id) = payload.column_id {
        q = q.bind(("col_id", col_id));
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
        Some(c) => {
            let api_card = c.into_api();
            let _ = state.events.send(BoardEvent::CardUpdated {
                card: api_card.clone(),
            });
            Ok(Json(api_card))
        }
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
        Some(_) => {
            let _ = state.events.send(BoardEvent::CardDeleted {
                card_id: card_id.clone(),
            });
            Ok(StatusCode::NO_CONTENT)
        }
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

    // Capture the source column ID before the update so the event tells
    // subscribers which column to remove the card from.
    let from_column_id = existing.column.id.to_raw();

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
        Some(c) => {
            let api_card = c.into_api();
            let _ = state.events.send(BoardEvent::CardMoved {
                card: api_card.clone(),
                from_column_id,
            });
            Ok(Json(api_card))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}
