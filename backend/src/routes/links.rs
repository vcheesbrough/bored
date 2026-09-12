//! Predecessor/successor links between cards (iteration 42 — card #76).
//!
//! A link is one stored fact — `predecessor` comes before `successor` — that
//! is created from either card and addressed by its own id afterwards. The
//! rules a link must satisfy (trimmed, capped reason; no loops) live in
//! `shared::links` so the browser can apply the same ones before it asks.
//!
//! Responses here carry a short plain-text body on the client-error statuses,
//! unlike the other routes which return a bare status. Four different things
//! are 422 for a link (self-link, cross-board, over-long reason, cycle) and
//! the editor needs to tell the user which one it hit.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use surrealdb::{Surreal, engine::local::Db};

use crate::audit;
use crate::auth::Claims;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::{DbCard, DbCardLink, DbColumn};
use crate::routes::boards::{AppState, editor_sub, find_board_by_slug};

/// Status plus a human-readable reason. Axum renders the tuple as a plain-text
/// response with that status; an empty string is a bodiless response.
type ApiError = (StatusCode, &'static str);

/// Any database or serialisation failure: the client did nothing wrong, so no
/// message is offered. The concrete error is discarded the same way the other
/// route modules discard theirs.
fn internal<E>(_: E) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, "")
}

const NOT_FOUND: ApiError = (StatusCode::NOT_FOUND, "");
const SELF_LINK: ApiError = (
    StatusCode::UNPROCESSABLE_ENTITY,
    "a card cannot be linked to itself",
);
const CROSS_BOARD: ApiError = (
    StatusCode::UNPROCESSABLE_ENTITY,
    "linked cards must be on the same board",
);
const REASON_TOO_LONG: ApiError = (
    StatusCode::UNPROCESSABLE_ENTITY,
    "reason is longer than 200 characters",
);
const CYCLE: ApiError = (
    StatusCode::UNPROCESSABLE_ENTITY,
    "linking these cards would create a loop",
);
const ALREADY_LINKED: ApiError = (StatusCode::CONFLICT, "these cards are already linked");

/// Field list every link read uses. The card numbers are not stored on the
/// link row; they are pulled through the record links at read time so the API
/// type can carry them without a second query per link.
const LINK_FIELDS: &str =
    "*, predecessor.number AS predecessor_number, successor.number AS successor_number";

/// One link by id, with the card numbers projected in.
async fn load_link(
    db: &Surreal<Db>,
    link_id: &str,
) -> Result<Option<DbCardLink>, surrealdb::Error> {
    db.query(format!(
        "SELECT {LINK_FIELDS} FROM card_links WHERE id = type::thing('card_links', $id) LIMIT 1"
    ))
    .bind(("id", link_id.to_string()))
    .await?
    .take(0)
}

/// Every link on a board, oldest first. A link's board is the board of the
/// cards it joins; both ends are always on the same one, so the predecessor
/// alone is enough to scope the query.
async fn board_links(
    db: &Surreal<Db>,
    board_ulid: &str,
) -> Result<Vec<DbCardLink>, surrealdb::Error> {
    db.query(format!(
        "SELECT {LINK_FIELDS} FROM card_links \
         WHERE predecessor.column.board = type::thing('boards', $bid) \
         ORDER BY created_at ASC"
    ))
    .bind(("bid", board_ulid.to_string()))
    .await?
    .take(0)
}

/// Every link with `card_id` at either end.
async fn links_touching_card(
    db: &Surreal<Db>,
    card_id: &str,
) -> Result<Vec<DbCardLink>, surrealdb::Error> {
    db.query(format!(
        "SELECT {LINK_FIELDS} FROM card_links \
         WHERE predecessor = type::thing('cards', $cid) \
            OR successor = type::thing('cards', $cid) \
         ORDER BY created_at ASC"
    ))
    .bind(("cid", card_id.to_string()))
    .await?
    .take(0)
}

/// The board a card sits on, via its column. `None` when the column has gone
/// — possible mid-cascade, when the caller should treat the board as unknown
/// rather than fail.
async fn board_of_card(
    db: &Surreal<Db>,
    card: &DbCard,
) -> Result<Option<String>, surrealdb::Error> {
    let column: Option<DbColumn> = db.select(("columns", card.column.id.to_raw())).await?;
    Ok(column.map(|c| c.board.id.to_raw()))
}

/// The board a link belongs to, via its predecessor card, falling back to the
/// successor when the predecessor (or its column) is already gone — possible
/// when the link outlives one of its ends mid-cascade. `None` only when
/// neither end resolves to a board.
async fn board_of_link(
    db: &Surreal<Db>,
    link: &DbCardLink,
) -> Result<Option<String>, surrealdb::Error> {
    let predecessor: Option<DbCard> = db.select(("cards", link.predecessor.id.to_raw())).await?;
    if let Some(card) = predecessor
        && let Some(board_id) = board_of_card(db, &card).await?
    {
        return Ok(Some(board_id));
    }
    let successor: Option<DbCard> = db.select(("cards", link.successor.id.to_raw())).await?;
    match successor {
        Some(card) => board_of_card(db, &card).await,
        None => Ok(None),
    }
}

/// Map a reason the shared rules rejected onto the 422 the client sees.
fn normalize_reason(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    shared::links::normalize_reason(raw).map_err(|_| REASON_TOO_LONG)
}

/// `GET /api/boards/:slug/links` — every link on the board.
pub async fn list_board_links(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Vec<shared::CardLink>>, StatusCode> {
    let board = match find_board_by_slug(&state.db, &slug).await? {
        Some(b) => b,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let links = board_links(&state.db, &board.id.id.to_raw())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(links.into_iter().map(DbCardLink::into_api).collect()))
}

/// `POST /api/cards/:id/links` — link this card to another one.
///
/// `direction` says which role the *other* card takes, so the same row comes
/// out whether the user started from the predecessor or the successor.
///
/// 404 unknown card · 409 already linked · 422 self-link, cross-board, reason
/// too long, or cycle.
pub async fn create_card_link(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::CreateCardLinkRequest>,
) -> Result<(StatusCode, Json<shared::CardLink>), ApiError> {
    let card: Option<DbCard> = state
        .db
        .select(("cards", &card_id))
        .await
        .map_err(internal)?;
    let card = card.ok_or(NOT_FOUND)?;

    // Checked before the other card is even loaded: a self-link is not a
    // missing card, and 404 would send the client looking for one.
    if payload.other_card_id == card_id {
        return Err(SELF_LINK);
    }

    let other: Option<DbCard> = state
        .db
        .select(("cards", &payload.other_card_id))
        .await
        .map_err(internal)?;
    let other = other.ok_or(NOT_FOUND)?;

    let board_id = board_of_card(&state.db, &card)
        .await
        .map_err(internal)?
        .ok_or(NOT_FOUND)?;
    let other_board_id = board_of_card(&state.db, &other)
        .await
        .map_err(internal)?
        .ok_or(NOT_FOUND)?;
    if board_id != other_board_id {
        return Err(CROSS_BOARD);
    }

    // Validate the reason before touching the graph so a bad reason costs
    // nothing more than the lookups above.
    let reason = normalize_reason(payload.reason.as_deref())?;

    let (predecessor_id, successor_id) = match payload.direction {
        shared::LinkDirection::Predecessor => (payload.other_card_id.clone(), card_id.clone()),
        shared::LinkDirection::Successor => (card_id.clone(), payload.other_card_id.clone()),
    };

    // From here to the CREATE the board's link graph must not change under
    // us — see `AppState::link_lock`. The guard drops at the end of the block.
    let created_id = {
        let _guard = state.link_lock.lock().await;

        let existing = board_links(&state.db, &board_id).await.map_err(internal)?;
        let pairs: Vec<(String, String)> = existing
            .iter()
            .map(|l| (l.predecessor.id.to_raw(), l.successor.id.to_raw()))
            .collect();

        // An exact duplicate is a conflict, not a cycle — checked first so the
        // client gets the more specific answer.
        if pairs
            .iter()
            .any(|(p, s)| *p == predecessor_id && *s == successor_id)
        {
            return Err(ALREADY_LINKED);
        }
        let edges = pairs.iter().map(|(p, s)| (p.as_str(), s.as_str()));
        if shared::links::would_create_cycle(edges, &predecessor_id, &successor_id) {
            return Err(CYCLE);
        }

        // The card lookups above ran before this lock was taken, so a
        // concurrent delete could have removed either card in the meantime —
        // cascade deletes do not hold `link_lock`. Re-check right before the
        // write so the CREATE below never targets a card that is already
        // gone (an orphan link, invisible or showing as `#0` on the board).
        let predecessor_still_exists: Option<DbCard> = state
            .db
            .select(("cards", predecessor_id.as_str()))
            .await
            .map_err(internal)?;
        let successor_still_exists: Option<DbCard> = state
            .db
            .select(("cards", successor_id.as_str()))
            .await
            .map_err(internal)?;
        if predecessor_still_exists.is_none() || successor_still_exists.is_none() {
            return Err(NOT_FOUND);
        }

        let id = ulid::Ulid::new().to_string().to_lowercase();
        state
            .db
            .query(
                "CREATE type::thing('card_links', $id) SET \
                 predecessor = type::thing('cards', $pred), \
                 successor = type::thing('cards', $succ), \
                 reason = $reason, \
                 last_edited_by = $editor",
            )
            .bind(("id", id.clone()))
            .bind(("pred", predecessor_id))
            .bind(("succ", successor_id))
            .bind(("reason", reason))
            .bind(("editor", editor_sub(&claims)))
            .await
            .map_err(internal)?
            // Under the lock the duplicate check above should make the unique
            // index unreachable; if it fires anyway, the right answer is still
            // "already linked", not a server fault.
            .check()
            .map_err(|e| {
                if e.to_string().contains("card_links_pair") {
                    ALREADY_LINKED
                } else {
                    internal(e)
                }
            })?;
        id
    };

    // Re-read rather than use the CREATE's return: the projected card numbers
    // only come from a SELECT.
    let link = load_link(&state.db, &created_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("link vanished after create"))?
        .into_api();

    let snapshot_after = serde_json::to_value(&link).map_err(internal)?;
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        audit::AuditRecord {
            claims: &claims,
            board_id: board_id.clone(),
            entity_type: "card_link",
            entity_id: &link.id,
            action: "create",
            snapshot_before: None,
            snapshot_after: Some(snapshot_after),
            restored_from: None,
            batch_group: None,
            audit_edit_session: None,
        },
    )
    .await
    .map_err(internal)?;

    let _ = state.events.send(BroadcastEvent {
        board_id,
        event: BoardEvent::CardLinkCreated { link: link.clone() },
    });
    Ok((StatusCode::CREATED, Json(link)))
}

/// `PUT /api/links/:id` — change a link's reason.
///
/// Sending the reason the link already has is a no-op: the link comes back
/// unchanged and no history row is written.
pub async fn update_card_link(
    State(state): State<AppState>,
    Path(link_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::UpdateCardLinkRequest>,
) -> Result<Json<shared::CardLink>, ApiError> {
    let existing = load_link(&state.db, &link_id)
        .await
        .map_err(internal)?
        .ok_or(NOT_FOUND)?;

    let reason = normalize_reason(payload.reason.as_deref())?;
    if reason == existing.reason {
        return Ok(Json(existing.into_api()));
    }

    let board_id = board_of_link(&state.db, &existing)
        .await
        .map_err(internal)?
        .unwrap_or_default();
    let snapshot_before = serde_json::to_value(existing.into_api()).map_err(internal)?;

    state
        .db
        .query(
            "UPDATE type::thing('card_links', $id) SET \
             reason = $reason, last_edited_by = $editor",
        )
        .bind(("id", link_id.clone()))
        .bind(("reason", reason))
        .bind(("editor", editor_sub(&claims)))
        .await
        .map_err(internal)?
        .check()
        .map_err(internal)?;

    let link = load_link(&state.db, &link_id)
        .await
        .map_err(internal)?
        .ok_or(NOT_FOUND)?
        .into_api();

    let snapshot_after = serde_json::to_value(&link).map_err(internal)?;
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        audit::AuditRecord {
            claims: &claims,
            board_id: board_id.clone(),
            entity_type: "card_link",
            entity_id: &link.id,
            action: "update",
            snapshot_before: Some(snapshot_before),
            snapshot_after: Some(snapshot_after),
            restored_from: None,
            batch_group: None,
            audit_edit_session: None,
        },
    )
    .await
    .map_err(internal)?;

    let _ = state.events.send(BroadcastEvent {
        board_id,
        event: BoardEvent::CardLinkUpdated { link: link.clone() },
    });
    Ok(Json(link))
}

/// `DELETE /api/links/:id` — remove a link.
pub async fn delete_card_link(
    State(state): State<AppState>,
    Path(link_id): Path<String>,
    claims: Extension<Claims>,
) -> Result<StatusCode, ApiError> {
    let existing = load_link(&state.db, &link_id)
        .await
        .map_err(internal)?
        .ok_or(NOT_FOUND)?;
    let board_id = board_of_link(&state.db, &existing)
        .await
        .map_err(internal)?
        .unwrap_or_default();

    delete_one_link(&state, &claims, &board_id, existing, None)
        .await
        .map_err(|status| (status, ""))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Record, delete, and broadcast the removal of one link. Shared by the
/// explicit delete route and the cascades below so every removal — however it
/// came about — leaves the same history row and the same SSE event.
async fn delete_one_link(
    state: &AppState,
    claims: &Claims,
    board_id: &str,
    link: DbCardLink,
    batch_group: Option<&str>,
) -> Result<(), StatusCode> {
    let link_id = link.id.id.to_raw();
    let snapshot_before =
        serde_json::to_value(link.into_api()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        audit::AuditRecord {
            claims,
            board_id: board_id.to_string(),
            entity_type: "card_link",
            entity_id: &link_id,
            action: "delete",
            snapshot_before: Some(snapshot_before),
            snapshot_after: None,
            restored_from: None,
            batch_group: batch_group.map(str::to_string),
            audit_edit_session: None,
        },
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let _: Option<DbCardLink> = state
        .db
        .delete(("card_links", &link_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let _ = state.events.send(BroadcastEvent {
        board_id: board_id.to_string(),
        event: BoardEvent::CardLinkDeleted { link_id },
    });
    Ok(())
}

/// Remove every link touching `card_id`, recording each removal. Called by
/// the card, column, and board delete routes before the card itself goes,
/// because a link cannot outlive either of its ends.
///
/// `batch_group` ties the link rows to the cascade that caused them. They are
/// never replayed by a batch restore — a deleted link may no longer be legal
/// to re-add — but grouping them keeps the history honest about why they went.
pub(crate) async fn cascade_delete_card_links(
    state: &AppState,
    claims: &Claims,
    board_id: &str,
    card_id: &str,
    batch_group: Option<&str>,
) -> Result<(), StatusCode> {
    let links = links_touching_card(&state.db, card_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    for link in links {
        delete_one_link(state, claims, board_id, link, batch_group).await?;
    }
    Ok(())
}
