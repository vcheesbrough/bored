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

    let card: Option<DbCard> = state
        .db
        .query("CREATE type::thing('cards', $id) SET column = type::thing('columns', $col_id), title = $title, description = $description, position = 0")
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

    if existing.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    let mut patch = serde_json::Map::new();

    if let Some(title) = payload.title {
        patch.insert("title".to_string(), serde_json::Value::String(title));
    }
    if let Some(desc) = payload.description {
        match desc {
            Some(d) => {
                patch.insert("description".to_string(), serde_json::Value::String(d));
            }
            None => {
                patch.insert("description".to_string(), serde_json::Value::Null);
            }
        }
    }
    if let Some(position) = payload.position {
        patch.insert(
            "position".to_string(),
            serde_json::Value::Number(position.into()),
        );
    }

    if let Some(col_id) = payload.column_id {
        // column is a record-link — must use raw SurrealQL
        let mut result = state
            .db
            .query("UPDATE type::thing('cards', $card_id) SET column = type::thing('columns', $col_id)")
            .bind(("card_id", card_id.clone()))
            .bind(("col_id", col_id))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        // Apply any remaining scalar patches
        if !patch.is_empty() {
            let card: Option<DbCard> = state
                .db
                .update(("cards", &card_id))
                .merge(serde_json::Value::Object(patch))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            return match card {
                Some(c) => Ok(Json(c.into_api())),
                None => Err(StatusCode::NOT_FOUND),
            };
        }

        let card: Option<DbCard> = result
            .take(0)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return match card {
            Some(c) => Ok(Json(c.into_api())),
            None => Err(StatusCode::NOT_FOUND),
        };
    }

    let card: Option<DbCard> = state
        .db
        .update(("cards", &card_id))
        .merge(serde_json::Value::Object(patch))
        .await
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

    if existing.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    let target_col: Option<DbColumn> = state
        .db
        .select(("columns", &payload.column_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if target_col.is_none() {
        return Err(StatusCode::NOT_FOUND);
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
