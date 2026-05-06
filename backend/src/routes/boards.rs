use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use surrealdb::{engine::local::Db, Surreal};
use tokio::sync::broadcast;

use crate::auth::{AuthConfig, Claims, JwksCache};
use crate::events::{BoardEvent, BroadcastEvent, BROADCAST_CAPACITY};
use crate::audit;
use crate::models::{DbBoard, DbCard, DbColumn};

/// Shared application state injected into every Axum handler via `State<AppState>`.
///
/// `Clone` is required by Axum — it clones the state per-request. All fields
/// are cheap to clone: `Surreal<Db>` is an `Arc`-backed handle, the broadcast
/// `Sender<T>` is also `Arc`-backed, and the auth/JWKS handles are explicit Arcs.
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
    /// OIDC configuration. `None` when `OIDC_ISSUER_URL` is unset — in that
    /// case the auth middleware injects a synthetic `anonymous` claim so local
    /// dev without an IdP keeps working unchanged. Production always sets this.
    pub auth: Option<Arc<AuthConfig>>,
    /// Cached JWKS public keys for the configured issuer. Always present
    /// alongside `auth` (created together at startup); kept as a separate
    /// field so handlers that only need verification don't pull in the secret.
    pub jwks_cache: Option<Arc<JwksCache>>,
}

impl AppState {
    /// Create a new `AppState` with a fresh broadcast channel.
    /// Auth defaults to disabled — `with_auth()` enables it.
    pub fn new(db: Surreal<Db>) -> Self {
        // `broadcast::channel` returns (Sender, Receiver). We keep the Sender
        // in AppState and drop the initial Receiver — new receivers are created
        // by calling `sender.subscribe()` in the SSE handler.
        let (tx, _rx) = broadcast::channel::<BroadcastEvent>(BROADCAST_CAPACITY);
        Self {
            db,
            events: tx,
            auth: None,
            jwks_cache: None,
        }
    }

    /// Builder-style attach of OIDC configuration + JWKS cache. Called once
    /// from `main.rs` if the OIDC env vars are present.
    pub fn with_auth(mut self, auth: Arc<AuthConfig>, jwks_cache: Arc<JwksCache>) -> Self {
        self.auth = Some(auth);
        self.jwks_cache = Some(jwks_cache);
        self
    }
}

/// Helper used by mutation handlers to populate `last_edited_by` on writes.
/// Pulled into one place so behaviour stays consistent — every mutation path
/// stamps the `sub` claim as the editor identity.
pub(crate) fn editor_sub(claims: &Extension<Claims>) -> String {
    claims.sub.clone()
}

/// Return `true` when `name` is a valid board slug:
/// 1–63 lowercase ASCII alphanumerics and hyphens, no leading or trailing hyphen.
pub(crate) fn is_valid_board_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 63 {
        return false;
    }
    let bytes = name.as_bytes();
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Look up a board by its name (slug). Returns `None` when no board has that name.
/// The name is now the stable URL identifier; the internal ULID primary key is
/// only used for DB record operations.
pub(crate) async fn find_board_by_slug(
    db: &Surreal<Db>,
    slug: &str,
) -> Result<Option<DbBoard>, StatusCode> {
    // Bind an owned String so the value outlives the async query chain
    // (SurrealDB's bind requires 'static, which &str does not satisfy).
    db.query("SELECT * FROM boards WHERE name = $slug LIMIT 1")
        .bind(("slug", slug.to_owned()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Map a SurrealDB error to an HTTP status code.
/// Unique-index violations on `board_name_unique` become 409; everything else 500.
fn board_db_err(e: surrealdb::Error) -> StatusCode {
    if e.to_string().contains("board_name_unique") {
        StatusCode::CONFLICT
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
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
    claims: Extension<Claims>,
    Json(payload): Json<shared::CreateBoardRequest>,
) -> Result<(StatusCode, Json<shared::Board>), StatusCode> {
    if !is_valid_board_name(&payload.name) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let id = ulid::Ulid::new().to_string().to_lowercase();
    let editor = editor_sub(&claims);

    let board: Option<DbBoard> = state
        .db
        .create(("boards", &id))
        .content(serde_json::json!({ "name": payload.name, "last_edited_by": editor }))
        .await
        .map_err(board_db_err)?;

    let board = board.ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let api_board = board.into_api();
    let snapshot_after =
        serde_json::to_value(api_board.clone()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        &claims,
        api_board.id.clone(),
        "board",
        &api_board.id,
        "create",
        None,
        Some(snapshot_after),
        None,
        None,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let _ = state.events.send(BroadcastEvent {
        // SSE filtering uses the internal ULID so card/column events can use
        // it too without needing to look up the board name.
        board_id: api_board.id.clone(),
        event: BoardEvent::BoardCreated {
            board: api_board.clone(),
        },
    });

    Ok((StatusCode::CREATED, Json(api_board)))
}

pub async fn get_board(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<shared::Board>, StatusCode> {
    match find_board_by_slug(&state.db, &slug).await? {
        Some(b) => Ok(Json(b.into_api())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn update_board(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::UpdateBoardRequest>,
) -> Result<Json<shared::Board>, StatusCode> {
    if !is_valid_board_name(&payload.name) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let existing = match find_board_by_slug(&state.db, &slug).await? {
        Some(b) => b,
        None => return Err(StatusCode::NOT_FOUND),
    };

    let board_ulid = existing.id.id.to_raw();
    let editor = editor_sub(&claims);
    let snapshot_before =
        serde_json::to_value(existing.clone().into_api()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let board: Option<DbBoard> = state
        .db
        .update(("boards", &board_ulid))
        .merge(serde_json::json!({ "name": payload.name, "last_edited_by": editor }))
        .await
        .map_err(board_db_err)?;

    match board {
        Some(b) => {
            let api_board = b.into_api();
            let snapshot_after =
                serde_json::to_value(api_board.clone()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            audit::record_and_broadcast(
                &state.db,
                &state.events,
                &claims,
                api_board.id.clone(),
                "board",
                &api_board.id,
                "update",
                Some(snapshot_before),
                Some(snapshot_after),
                None,
                None,
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
    Path(slug): Path<String>,
    claims: Extension<Claims>,
) -> Result<StatusCode, StatusCode> {
    let board_record = match find_board_by_slug(&state.db, &slug).await? {
        Some(b) => b,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let id = board_record.id.id.to_raw();
    let batch = audit::new_batch_group();

    let cards: Vec<DbCard> = state
        .db
        .query(
            "SELECT * FROM cards WHERE column.board = type::thing('boards', $bid) \
             ORDER BY column ASC, position ASC",
        )
        .bind(("bid", id.clone()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    for card in cards {
        let entity_id = card.id.id.to_raw();
        let snapshot_before = serde_json::to_value(card.clone().into_api())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        audit::record_and_broadcast(
            &state.db,
            &state.events,
            &claims,
            id.clone(),
            "card",
            &entity_id,
            "delete",
            Some(snapshot_before),
            None,
            None,
            Some(batch.clone()),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let _: Option<DbCard> = state
            .db
            .delete(("cards", &entity_id))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let cols: Vec<DbColumn> = state
        .db
        .query(
            "SELECT * FROM columns WHERE board = type::thing('boards', $bid) ORDER BY position ASC",
        )
        .bind(("bid", id.clone()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    for col in cols {
        let entity_id = col.id.id.to_raw();
        let snapshot_before = serde_json::to_value(col.clone().into_api())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        audit::record_and_broadcast(
            &state.db,
            &state.events,
            &claims,
            id.clone(),
            "column",
            &entity_id,
            "delete",
            Some(snapshot_before),
            None,
            None,
            Some(batch.clone()),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let _: Option<DbColumn> = state
            .db
            .delete(("columns", &entity_id))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let board_snap = serde_json::to_value(board_record.clone().into_api())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        &claims,
        id.clone(),
        "board",
        &id,
        "delete",
        Some(board_snap),
        None,
        None,
        Some(batch),
    )
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
