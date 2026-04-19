use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use surrealdb::{engine::local::Db, Surreal};
use tokio::sync::broadcast;

use crate::events::{BoardEvent, BroadcastEvent, BROADCAST_CAPACITY};
use crate::models::DbBoard;

/// Shared application state injected into every Axum handler via `State<AppState>`.
///
/// `Clone` is required by Axum — it clones the state per-request. Both fields
/// are cheap to clone: `Surreal<Db>` is an `Arc`-backed handle and
/// `broadcast::Sender<T>` is also backed by an `Arc` internally.
#[derive(Clone)]
pub struct AppState {
    /// SurrealDB embedded database handle. Every operation on this handle is
    /// async and goes through SurrealDB's internal connection pool.
    pub db: Surreal<Db>,
    /// Broadcast sender for real-time board events. Calling `.send(event)` delivers
    /// a clone of the event to every currently-subscribed SSE client. We ignore the
    /// error returned when there are no subscribers (that just means nobody is
    /// listening right now, which is fine).
    ///
    /// Each message is a `BroadcastEvent` that bundles the event with the board ID
    /// it originated from, so SSE clients can filter to their own board's stream.
    pub events: broadcast::Sender<BroadcastEvent>,
}

impl AppState {
    /// Create a new `AppState` with a fresh broadcast channel.
    pub fn new(db: Surreal<Db>) -> Self {
        // `broadcast::channel` returns (Sender, Receiver). We keep the Sender
        // in AppState and drop the initial Receiver — new receivers are created
        // by calling `sender.subscribe()` in the SSE handler.
        let (tx, _rx) = broadcast::channel::<BroadcastEvent>(BROADCAST_CAPACITY);
        Self { db, events: tx }
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

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

    let board = board.ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // Seed the board with default columns (Todo, Done) so it's immediately usable.
    for (name, position) in [("Todo", 0i32), ("Done", 1i32)] {
        let col_id = ulid::Ulid::new().to_string().to_lowercase();
        state
            .db
            .query("CREATE type::thing('columns', $id) SET board = type::thing('boards', $board_id), name = $name, position = $position")
            .bind(("id", col_id))
            .bind(("board_id", id.clone()))
            .bind(("name", name.to_string()))
            .bind(("position", position))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let api_board = board.into_api();
    // Fire-and-forget: if no SSE client is connected, the send returns Err
    // (no receivers) — we intentionally ignore it.
    let _ = state.events.send(BroadcastEvent {
        board_id: api_board.id.clone(),
        event: BoardEvent::BoardCreated {
            board: api_board.clone(),
        },
    });

    Ok((StatusCode::CREATED, Json(api_board)))
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
    // Check board exists first so we can return 404 rather than a silent no-op.
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
        Some(b) => {
            let api_board = b.into_api();
            let _ = state.events.send(BroadcastEvent {
                board_id: api_board.id.clone(),
                event: BoardEvent::BoardUpdated {
                    board: api_board.clone(),
                },
            });
            Ok(Json(api_board))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn delete_board(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    // Cascade delete: remove all cards first, then columns, then the board.
    // SurrealDB doesn't enforce FK-style cascades automatically, so we do it
    // explicitly in the correct dependency order.
    state
        .db
        .query("DELETE cards WHERE column.board = type::thing('boards', $id)")
        .bind(("id", id.clone()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    state
        .db
        .query("DELETE columns WHERE board = type::thing('boards', $id)")
        .bind(("id", id.clone()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let _: Option<DbBoard> = state
        .db
        .delete(("boards", &id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let _ = state.events.send(BroadcastEvent {
        board_id: id.clone(),
        event: BoardEvent::BoardDeleted { board_id: id },
    });

    Ok(StatusCode::NO_CONTENT)
}
