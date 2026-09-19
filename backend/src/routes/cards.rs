// Sparse-position arithmetic and the column rebalance live in their own file
// (`cards/position.rs`); the handlers below only ask it "where does this card
// go?".
mod position;
// Likewise, "what does this PUT actually change?" is a pure decision with no
// database in it, so it lives in `cards/update.rs` where it can be unit tested.
mod update;

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use std::collections::HashSet;
use surrealdb::{Surreal, engine::local::Db};

use crate::audit;
use crate::auth::Claims;
use crate::error::ApiError;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::{DbCard, DbCardCounter, DbColumn};
use crate::routes::boards::{AppState, editor_sub};

use position::{POSITION_GAP, compute_sparse_position, compute_top_position};
use update::{CardUpdate, CardWrite};

/// Apply [`shared::tags::normalize`] to a client-supplied tag list, mapping a
/// rejected list onto the HTTP status the handlers return.
///
/// Limit violations are a client mistake (a tag longer than the cap, or an
/// absurd number of them), not a server fault, so they surface as 422 rather
/// than being silently trimmed into something the user did not ask for.
fn normalize_tags(raw: &[String]) -> Result<Vec<String>, ApiError> {
    shared::tags::normalize(raw).map_err(|_| ApiError::UNPROCESSABLE)
}

/// Load a card, mapping "no such card" onto 404 and a database fault onto 500.
async fn load_card(db: &Surreal<Db>, card_id: &str) -> Result<DbCard, ApiError> {
    let card: Option<DbCard> = db.select(("cards", card_id)).await?;
    card.ok_or(ApiError::NotFound)
}

/// Load a column the request named, mapping "no such column" onto 404.
///
/// Use this for a column the caller asked for; use [`find_column`] for one the
/// server looks up on its own behalf.
async fn load_column(db: &Surreal<Db>, col_id: &str) -> Result<DbColumn, ApiError> {
    let column: Option<DbColumn> = db.select(("columns", col_id)).await?;
    column.ok_or(ApiError::NotFound)
}

/// Look up a column that may legitimately be gone.
///
/// A card's own column is read to find its board, but a cascade delete can
/// remove the column while the card row is still around. That is not the
/// caller's fault, so it is `Ok(None)` rather than a 404 — the callers fall
/// back to an empty board id, which no connected client is scoped to.
async fn find_column(db: &Surreal<Db>, col_id: &str) -> Result<Option<DbColumn>, ApiError> {
    db.select(("columns", col_id)).await.map_err(ApiError::from)
}

/// The JSON snapshot an audit row stores for a card: the card exactly as the
/// API renders it, so restoring a row replays a real API shape.
fn snapshot(card: &shared::Card) -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(card).map_err(ApiError::from)
}

/// Run a planned edit ([`CardUpdate::plan`]) and return the stored row.
///
/// The card was loaded moments ago, so a zero-row result means it was deleted
/// in between — 404, the same answer the caller would have given had the delete
/// landed first.
async fn persist_update(
    db: &Surreal<Db>,
    card_id: String,
    editor: String,
    write: CardWrite,
) -> Result<DbCard, ApiError> {
    let statement = write.statement();
    let card: Option<DbCard> = write
        .bind(
            db.query(statement)
                .bind(("card_id", card_id))
                .bind(("editor", editor)),
        )
        .await?
        .take(0)?;
    card.ok_or(ApiError::NotFound)
}

/// Write a card's new column and position, and return the stored row.
///
/// Unlike [`persist_update`] the fields are fixed, so the statement is a
/// constant rather than something assembled per request.
///
/// Takes its ids by value, as [`persist_update`] does: `.bind` needs owned
/// values, and the caller is finished with both by this point.
async fn persist_move(
    db: &Surreal<Db>,
    card_id: String,
    col_id: String,
    position: i32,
    editor: String,
) -> Result<DbCard, ApiError> {
    let card: Option<DbCard> = db
        .query(
            "UPDATE type::thing('cards', $card_id) \
             SET column = type::thing('columns', $col_id), position = $position, last_edited_by = $editor",
        )
        .bind(("card_id", card_id))
        .bind(("col_id", col_id))
        .bind(("position", position))
        .bind(("editor", editor))
        .await
        ?
        .take(0)
        ?;
    card.ok_or(ApiError::NotFound)
}

/// One completed card mutation, ready to be recorded and announced.
///
/// Grouped into a struct rather than passed as eight arguments to
/// [`audit_and_emit`] — the same shape `audit::AuditRecord` uses, minus the
/// fields these two handlers never vary.
struct CardMutation<'a> {
    claims: &'a Claims,
    board_id: String,
    /// The card's id, as the audit log's `entity_id`.
    card_id: &'a str,
    /// `"move"` or `"update"` — a closed vocabulary, never request-derived,
    /// which `&'static str` enforces at the type level.
    action: &'static str,
    snapshot_before: serde_json::Value,
    snapshot_after: serde_json::Value,
    audit_edit_session: Option<&'a str>,
    /// The board event to broadcast once the audit row is committed.
    event: BoardEvent,
}

/// Record a card mutation in the audit log, then announce it to subscribers.
///
/// The order is deliberate and matches every other handler in this file: the
/// audit row is committed first (and emits its own `AuditAppended`), and only
/// then does the event that actually moves the card in connected browsers go
/// out. A failed audit write is a 500 and no event is sent, so no browser ever
/// shows a change that history does not record.
///
/// Broadcast failure is ignored on purpose — `send` only errors when nobody is
/// listening, which is the normal state of a server with no open tabs.
async fn audit_and_emit(state: &AppState, mutation: CardMutation<'_>) -> Result<(), ApiError> {
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        audit::AuditRecord {
            claims: mutation.claims,
            board_id: mutation.board_id.clone(),
            entity_type: "card",
            entity_id: mutation.card_id,
            action: mutation.action,
            snapshot_before: Some(mutation.snapshot_before),
            snapshot_after: Some(mutation.snapshot_after),
            restored_from: None,
            batch_group: None,
            audit_edit_session: mutation.audit_edit_session,
        },
    )
    .await?;

    let _ = state.events.send(BroadcastEvent {
        board_id: mutation.board_id,
        event: mutation.event,
    });
    Ok(())
}

pub async fn list_cards(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
) -> Result<Json<Vec<shared::Card>>, ApiError> {
    // Verify the column exists before returning its cards.
    let column: Option<DbColumn> = state.db.select(("columns", &col_id)).await?;

    if column.is_none() {
        return Err(ApiError::NotFound);
    }

    let cards: Vec<DbCard> = state
        .db
        .query(
            "SELECT * FROM cards WHERE column = type::thing('columns', $id) ORDER BY position ASC",
        )
        .bind(("id", col_id))
        .await?
        .take(0)?;

    Ok(Json(cards.into_iter().map(DbCard::into_api).collect()))
}

pub async fn get_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
) -> Result<Json<shared::Card>, ApiError> {
    let card: Option<DbCard> = state.db.select(("cards", &card_id)).await?;

    match card {
        Some(c) => Ok(Json(c.into_api())),
        None => Err(ApiError::NotFound),
    }
}

/// `GET /api/cards/by-number/:number` — fetch a card by its human-readable
/// sequential number. Card numbers are globally unique (single counter), so no
/// board scoping is needed. Used by the frontend when the URL carries
/// `?card=<number>` instead of the internal ULID.
pub async fn get_card_by_number(
    State(state): State<AppState>,
    Path(number): Path<u32>,
) -> Result<Json<shared::Card>, ApiError> {
    let card: Option<DbCard> = state
        .db
        .query("SELECT * FROM cards WHERE number = $number LIMIT 1")
        .bind(("number", number as i64))
        .await?
        .take(0)?;

    match card {
        Some(c) => Ok(Json(c.into_api())),
        None => Err(ApiError::NotFound),
    }
}

pub async fn create_card(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::CreateCardRequest>,
) -> Result<(StatusCode, Json<shared::Card>), ApiError> {
    let column: Option<DbColumn> = state.db.select(("columns", &col_id)).await?;

    // Destructure early to capture the board ID for the SSE event.
    let column = match column {
        Some(c) => c,
        None => return Err(ApiError::NotFound),
    };
    let board_id = column.board.id.to_raw();

    // Normalize before claiming a card number so a rejected tag list cannot
    // burn a number from the global counter.
    let tags = normalize_tags(&payload.tags)?;

    let id = ulid::Ulid::new().to_string().to_lowercase();
    let editor = editor_sub(&claims);

    // Claim the next card number by atomically incrementing the global counter.
    // SurrealDB record-level mutations are atomic, so concurrent creates cannot
    // receive the same count value.
    let counter: Option<DbCardCounter> = state
        .db
        .query("UPDATE card_counter:global SET count += 1 RETURN AFTER")
        .await?
        .take(0)?;
    let card_number = counter.map(|c| c.count).unwrap_or(1);

    // Compute the sparse position for inserting at the TOP of the column.
    // This is done before the CREATE so the position is known up front;
    // the two-step approach is safe because card IDs are ULIDs and the
    // counter increment above already serialises concurrent creates.
    let top_pos = compute_top_position(&state.db, &col_id).await?;

    let card: Option<DbCard> = state
        .db
        .query(
            "CREATE type::thing('cards', $id) SET \
             column = type::thing('columns', $col_id), \
             body = $body, \
             number = $number, \
             position = $position, \
             tags = $tags, \
             last_edited_by = $editor",
        )
        .bind(("id", id))
        .bind(("col_id", col_id))
        .bind(("body", payload.body))
        .bind(("number", card_number))
        .bind(("position", top_pos))
        .bind(("tags", tags))
        .bind(("editor", editor))
        .await?
        .take(0)?;

    match card {
        Some(c) => {
            let api_card = c.into_api();
            let snapshot_after = serde_json::to_value(api_card.clone())?;
            audit::record_and_broadcast(
                &state.db,
                &state.events,
                audit::AuditRecord {
                    claims: &claims,
                    board_id: board_id.clone(),
                    entity_type: "card",
                    entity_id: &api_card.id,
                    action: "create",
                    snapshot_before: None,
                    snapshot_after: Some(snapshot_after),
                    restored_from: None,
                    batch_group: None,
                    audit_edit_session: None,
                },
            )
            .await?;

            let _ = state.events.send(BroadcastEvent {
                board_id,
                event: BoardEvent::CardCreated {
                    card: api_card.clone(),
                },
            });
            Ok((StatusCode::CREATED, Json(api_card)))
        }
        // A create that reports success always returns its row.
        None => Err(ApiError::internal("create returned no card row")),
    }
}

/// `PUT /api/cards/:id` — apply a partial edit to a card.
///
/// Reads as the sequence it is: load the card, resolve its board, validate a
/// target column if one was named, work out what actually changes
/// ([`CardUpdate::plan`]), write it, then record and announce it.
///
/// Two orderings here are load-bearing and deliberately preserved:
///
/// * the column checks run **before** tag normalization, so a request that is
///   wrong about both its column and its tags gets the 404, not the 422;
/// * the no-op check runs **after** the column checks, so naming a nonexistent
///   column is still a 404 even when nothing would have been written.
pub async fn update_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::UpdateCardRequest>,
) -> Result<Json<shared::Card>, ApiError> {
    let existing = load_card(&state.db, &card_id).await?;
    let snapshot_before = snapshot(&existing.clone().into_api())?;

    // Always look up the current column so we have the board ID for the SSE event.
    let current_col = find_column(&state.db, &existing.column.id.to_raw()).await?;
    let board_id = current_col
        .as_ref()
        .map(|c| c.board.id.to_raw())
        .unwrap_or_default();

    // Validate target column if provided, and guard against cross-board moves.
    if let Some(col_id) = payload.column_id.as_deref() {
        let target_col = load_column(&state.db, col_id).await?;
        if let Some(ref current_col) = current_col
            && current_col.board.id.to_raw() != target_col.board.id.to_raw()
        {
            return Err(ApiError::UNPROCESSABLE);
        }
    }

    // Nothing changed — return the existing card unchanged, with no write, no
    // audit row and no event.
    let Some(planned) = CardUpdate::plan(payload, &existing)? else {
        return Ok(Json(existing.into_api()));
    };
    // Destructured so the write half can be consumed by `persist_update` while
    // the audit half stays available after the query has run.
    let CardUpdate { write, audit } = planned;

    let api_card = persist_update(&state.db, card_id, editor_sub(&claims), write)
        .await?
        .into_api();
    let snapshot_after = snapshot(&api_card)?;

    audit_and_emit(
        &state,
        CardMutation {
            claims: &claims,
            board_id,
            card_id: &api_card.id,
            action: audit.action,
            snapshot_before,
            snapshot_after,
            audit_edit_session: audit.edit_session.as_deref(),
            event: BoardEvent::CardUpdated {
                card: api_card.clone(),
            },
        },
    )
    .await?;

    Ok(Json(api_card))
}

pub async fn delete_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    claims: Extension<Claims>,
) -> Result<StatusCode, ApiError> {
    let existing: Option<DbCard> = state.db.select(("cards", &card_id)).await?;

    let existing = match existing {
        Some(e) => e,
        None => return Err(ApiError::NotFound),
    };

    let board_id = state
        .db
        .select::<Option<DbColumn>>(("columns", existing.column.id.to_raw()))
        .await
        .ok()
        .flatten()
        .map(|c| c.board.id.to_raw())
        .unwrap_or_default();

    // A link cannot outlive either of its cards. Remove this card's links —
    // and record each removal — before the card itself goes, so the link
    // delete rows precede the card delete row in history. They share no batch
    // group: restoring the card would not bring its links back (another link
    // could have closed a loop meanwhile), so there is nothing to replay.
    crate::routes::links::cascade_delete_card_links(&state, &claims, &board_id, &card_id, None)
        .await?;

    let snapshot_before = serde_json::to_value(existing.clone().into_api())?;
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        audit::AuditRecord {
            claims: &claims,
            board_id: board_id.clone(),
            entity_type: "card",
            entity_id: &card_id,
            action: "delete",
            snapshot_before: Some(snapshot_before),
            snapshot_after: None,
            restored_from: None,
            batch_group: None,
            audit_edit_session: None,
        },
    )
    .await?;

    match state
        .db
        .delete::<Option<DbCard>>(("cards", &card_id))
        .await?
    {
        Some(deleted) => {
            // Look up the column to find the board ID for the SSE event.
            // The column may have been cascade-deleted with its board, so
            // fall back to an empty string if it's gone — SSE delivery is
            // best-effort and no connected client will be scoped to a
            // non-existent board anyway.
            let board_id = state
                .db
                .select::<Option<DbColumn>>(("columns", deleted.column.id.to_raw()))
                .await
                .unwrap_or(None)
                .map(|c| c.board.id.to_raw())
                .unwrap_or_default();
            let _ = state.events.send(BroadcastEvent {
                board_id,
                event: BoardEvent::CardDeleted {
                    card_id: card_id.clone(),
                },
            });
            Ok(StatusCode::NO_CONTENT)
        }
        None => Err(ApiError::NotFound),
    }
}

/// `PUT /api/cards/:id/move` — move a card to a position in a column.
///
/// Unlike [`update_card`] there is nothing to decide: every field of the
/// request is written. The handler is the I/O sequence — load, validate,
/// compute the position, write, record, announce.
///
/// The two failure modes are ordered: an unknown target column is a 404 and is
/// checked before the cross-board 422, which is the order the API has always
/// answered them in.
pub async fn move_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::MoveCardRequest>,
) -> Result<Json<shared::Card>, ApiError> {
    let existing = load_card(&state.db, &card_id).await?;
    let snapshot_before = snapshot(&existing.clone().into_api())?;

    let target_col = load_column(&state.db, &payload.column_id).await?;

    // Capture the source column ID before the update so the event tells
    // subscribers which column to remove the card from.
    let from_column_id = existing.column.id.to_raw();

    // Board ID for the SSE event — always available from the target column.
    let board_id = target_col.board.id.to_raw();

    // Guard: target column must belong to the same board as the card's current
    // column. A card whose own column has vanished has nothing to compare
    // against, so it is let through rather than wedged.
    if let Some(current_col) = find_column(&state.db, &from_column_id).await?
        && current_col.board.id.to_raw() != board_id
    {
        return Err(ApiError::UNPROCESSABLE);
    }

    // Compute a sparse position so only this one card needs to be written.
    // Other cards in the column are unchanged in the happy path; a rebalance
    // is triggered automatically when the gap between neighbours is exhausted.
    let new_pos =
        compute_sparse_position(&state.db, &card_id, &payload.column_id, payload.position).await?;

    let api_card = persist_move(
        &state.db,
        card_id,
        payload.column_id,
        new_pos,
        editor_sub(&claims),
    )
    .await?
    .into_api();
    let snapshot_after = snapshot(&api_card)?;

    audit_and_emit(
        &state,
        CardMutation {
            claims: &claims,
            board_id,
            card_id: &api_card.id,
            action: "move",
            snapshot_before,
            snapshot_after,
            audit_edit_session: None,
            event: BoardEvent::CardMoved {
                card: api_card.clone(),
                from_column_id,
            },
        },
    )
    .await?;

    Ok(Json(api_card))
}

/// `PUT /api/columns/:id/cards/reorder`
///
/// Applies a complete desired card order to one column. This endpoint is
/// deliberately **dumb**: it knows nothing about card links or about why the
/// caller wants this order. The client computes the order (the column header's
/// "sort by links" button does it with `shared::links::order_by_dependency`)
/// and sends the result, exactly as `columns::reorder_columns` works for
/// columns.
///
/// The request is tolerant, per [`shared::CardsReorderRequest`]: ids that are
/// not cards of this column are dropped — which is also the IDOR guard, since
/// another column's card simply is not in the loaded set — and cards the caller
/// omitted keep their relative order at the bottom.
///
/// # Positions
///
/// Card positions are *sparse* (`POSITION_GAP`-spaced, bisected on insert), so
/// they are not array indices and must not be treated as such. Two rules apply:
///
/// * when the values in use are strictly increasing they are reused as the
///   slots, so a card that does not move is never written and keeps its exact
///   value — this is what makes "sort an already-sorted column" a true no-op;
/// * otherwise the column is renumbered to `(i + 1) * POSITION_GAP`, the same
///   scheme `rebalance_column` uses. `cards.position` has no unique index and
///   both `PUT /api/cards/:id` and audit restore can write an arbitrary value,
///   so duplicates are possible, and `ORDER BY position ASC` has no tiebreak.
///   A column holding duplicates therefore has no defined order to be "already
///   sorted" in: it is rewritten even when the requested order matches what the
///   database happened to return.
///
/// Distinct final positions are what lets the browser reposition cards from
/// `CardMoved` alone, without a dedicated bulk SSE event.
///
/// One audit row per written card, all sharing a batch group. The group is
/// informational: batch restore only fans out `delete` rows, so restoring one
/// of these `move` rows restores that card's position alone.
pub async fn reorder_cards(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::CardsReorderRequest>,
) -> Result<Json<Vec<shared::Card>>, ApiError> {
    let column: Option<DbColumn> = state.db.select(("columns", &col_id)).await?;

    let Some(column) = column else {
        return Err(ApiError::NotFound);
    };
    let board_id = column.board.id.to_raw();

    // Current contents of the column, top first — the same query `list_cards`
    // uses, so the caller is ordering exactly what it was shown.
    let current: Vec<DbCard> = state
        .db
        .query(
            "SELECT * FROM cards WHERE column = type::thing('columns', $id) ORDER BY position ASC",
        )
        .bind(("id", col_id.clone()))
        .await?
        .take(0)?;

    // Requested ids that really are in this column, in the order asked for and
    // without repeats, followed by everything the caller left out in its
    // current order.
    let mut seen: HashSet<String> = HashSet::new();
    let mut target: Vec<&DbCard> = Vec::with_capacity(current.len());
    for wanted in &payload.order {
        if let Some(card) = current.iter().find(|c| &c.id.id.to_raw() == wanted)
            && seen.insert(wanted.clone())
        {
            target.push(card);
        }
    }
    for card in &current {
        if !seen.contains(&card.id.id.to_raw()) {
            target.push(card);
        }
    }

    // Are the current positions unambiguous? `ORDER BY position ASC` has no
    // tiebreak, so as soon as two cards share a value the order above is
    // whatever the database happened to return — not something to preserve or
    // to compare against.
    let existing: Vec<i32> = current.iter().map(|c| c.position).collect();
    let unambiguous = existing.windows(2).all(|w| w[0] < w[1]);

    // Nothing to do — every card is already where the caller wants it, in an
    // order the database will reproduce. Bail out before writing anything so
    // the operation is genuinely idempotent: no writes, no audit, no SSE.
    let already_ordered = target
        .iter()
        .zip(current.iter())
        .all(|(wanted, present)| wanted.id == present.id);
    if unambiguous && already_ordered {
        return Ok(Json(current.into_iter().map(DbCard::into_api).collect()));
    }

    // The slots to place cards into: reuse the position values already in use
    // when they are unambiguous, so cards that do not move are never written;
    // otherwise renumber the column onto the `POSITION_GAP` grid, repairing the
    // duplicates on the way past (the same scheme `rebalance_column` uses).
    let slots: Vec<i32> = if unambiguous {
        existing
    } else {
        (0..current.len())
            .map(|i| (i as i32 + 1) * POSITION_GAP)
            .collect()
    };

    let editor = editor_sub(&claims);
    let batch = audit::new_batch_group();
    let mut moved: Vec<shared::Card> = Vec::new();

    // The write loop is wrapped rather than run inline so its error exits do
    // not skip the fan-out below. Every `?` in here returns `Err(500)` after
    // some cards have already had their `position` committed and audited; if
    // that error went straight out of the handler, `moved` would be dropped
    // and not one of those committed writes would ever be announced. Every
    // other open browser would keep rendering the pre-sort order indefinitely
    // — `sse_handler` drops lagged and missed events with no reconciliation
    // (events.rs), so nothing short of a manual reload would repair it.
    //
    // Holding the outcome instead lets the broadcast run on both paths: what
    // was stored is what gets announced, even when the batch aborts part way.
    let outcome: Result<(), ApiError> = async {
        for (card, &position) in target.iter().zip(slots.iter()) {
            if card.position == position {
                continue;
            }
            let card_id = card.id.id.to_raw();
            let snapshot_before = serde_json::to_value((*card).clone().into_api())
                ?;

            // RETURN AFTER in the same statement: never write a position without a
            // confirmed row to audit, and never audit a write that did not land.
            // The WHERE clause re-asserts the column so a card moved out from under
            // this request matches zero rows rather than being dragged back.
            let updated: Vec<DbCard> = state
                .db
                .query(
                    "UPDATE type::thing('cards', $id) SET position = $pos, last_edited_by = $editor \
                     WHERE column = type::thing('columns', $col_id) RETURN AFTER",
                )
                .bind(("id", card_id.clone()))
                .bind(("pos", position))
                .bind(("col_id", col_id.clone()))
                .bind(("editor", editor.clone()))
                .await
                ?
                .take(0)
                ?;
            let mut it = updated.into_iter();
            let Some(card_after) = it.next() else {
                // Zero rows means the card left this column between the SELECT
                // above and this UPDATE. It is no longer part of the order being
                // applied, so skip it rather than failing a batch that has already
                // written rows — the same policy `reorder_columns` uses for a
                // column that no longer qualifies.
                continue;
            };
            if it.next().is_some() {
                return Err(ApiError::internal(
                    "reorder update matched more than one card",
                ));
            }

            let api_card = card_after.into_api();
            let snapshot_after = serde_json::to_value(api_card.clone())
                ?;

            audit::record_and_broadcast(
                &state.db,
                &state.events,
                audit::AuditRecord {
                    claims: &claims,
                    board_id: board_id.clone(),
                    entity_type: "card",
                    entity_id: &card_id,
                    action: "move",
                    snapshot_before: Some(snapshot_before),
                    snapshot_after: Some(snapshot_after),
                    restored_from: None,
                    batch_group: Some(batch.clone()),
                    audit_edit_session: None,
                },
            )
            .await
            ?;

            moved.push(api_card);
        }
        Ok(())
    }
    .await;

    // Broadcast only once the loop has finished, so no `CardMoved` — the event
    // that actually repositions a card — describes a column that is still
    // mid-write. (The loop above is not silent: `record_and_broadcast` emits an
    // `AuditAppended` per row as it goes. Those do not move cards, so a
    // listener acting only on `CardMoved` never sees the transient state.)
    //
    // This runs whether the loop succeeded or failed. On the failure path
    // `moved` holds exactly the cards whose `UPDATE` was confirmed, so what is
    // announced still matches what is stored; the column is left part-sorted,
    // but every listener agrees on that same part-sorted state instead of
    // diverging from it silently.
    //
    // Cost of doing it this way: `BROADCAST_CAPACITY` is 128 (events.rs:25)
    // and each moved card spends two slots (`AuditAppended` + `CardMoved`), so
    // a very large column reordered twice in quick succession could lag a slow
    // tab into `Lagged`. The escape hatch is an aggregate `CardsReordered`
    // variant, deliberately not built until something needs it.
    for card in moved {
        let _ = state.events.send(BroadcastEvent {
            board_id: board_id.clone(),
            event: BoardEvent::CardMoved {
                card,
                // A reorder never leaves the column, so source and destination
                // are the same — the browser treats that as an in-place move.
                from_column_id: col_id.clone(),
            },
        });
    }

    // Now that the committed writes have been announced, a failed batch can
    // surface as the 500 it is.
    outcome?;

    let ordered: Vec<DbCard> = state
        .db
        .query(
            "SELECT * FROM cards WHERE column = type::thing('columns', $id) ORDER BY position ASC",
        )
        .bind(("id", col_id))
        .await?
        .take(0)?;

    Ok(Json(ordered.into_iter().map(DbCard::into_api).collect()))
}
