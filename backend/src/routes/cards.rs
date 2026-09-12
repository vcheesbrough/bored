use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use std::collections::HashSet;

use surrealdb::{Surreal, engine::local::Db};

use crate::audit;
use crate::auth::Claims;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::{DbCard, DbCardCounter, DbColumn};
use crate::routes::boards::{AppState, editor_sub};

/// Gap between adjacent card positions in the sparse ordering scheme.
/// Large enough to allow ~10 bisections between any two cards before a rebalance
/// is needed, while fitting comfortably within i32.
const POSITION_GAP: i32 = 1024;

/// Apply [`shared::tags::normalize`] to a client-supplied tag list, mapping a
/// rejected list onto the HTTP status the handlers return.
///
/// Limit violations are a client mistake (a tag longer than the cap, or an
/// absurd number of them), not a server fault, so they surface as 422 rather
/// than being silently trimmed into something the user did not ask for.
fn normalize_tags(raw: &[String]) -> Result<Vec<String>, StatusCode> {
    shared::tags::normalize(raw).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)
}

/// Given the sorted card list for a column (with the moving card excluded),
/// compute the sparse position value for inserting at `idx`.
/// Uses sentinels: 0 at the top edge, last_pos + 2*GAP at the bottom edge.
fn midpoint_position(col_cards: &[DbCard], idx: usize) -> i32 {
    let left = if idx == 0 {
        0
    } else {
        col_cards[idx - 1].position
    };
    let right = if idx >= col_cards.len() {
        col_cards.last().map(|c| c.position).unwrap_or(0) + 2 * POSITION_GAP
    } else {
        col_cards[idx].position
    };
    (left + right) / 2
}

/// Returns true when the candidate position is not strictly between its
/// left and right neighbours — meaning the gap is exhausted and we must
/// rebalance before inserting.
fn needs_rebalance(col_cards: &[DbCard], idx: usize, new_pos: i32) -> bool {
    let left = if idx == 0 {
        0
    } else {
        col_cards[idx - 1].position
    };
    // Bottom edge: any positive value above `left` is always valid.
    let right = if idx >= col_cards.len() {
        i32::MAX
    } else {
        col_cards[idx].position
    };
    new_pos <= left || new_pos >= right
}

/// Reassign every card in `col_id` to evenly-spaced positions (GAP, 2*GAP, …).
/// Called only when the gap between two neighbouring cards drops to zero,
/// which happens after ~10 consecutive insertions at the same slot.
async fn rebalance_column(db: &Surreal<Db>, col_id: &str) -> Result<(), surrealdb::Error> {
    let cards: Vec<DbCard> = db
        .query(
            "SELECT * FROM cards \
             WHERE column = type::thing('columns', $col_id) \
             ORDER BY position ASC",
        )
        .bind(("col_id", col_id.to_string()))
        .await?
        .take(0)?;

    for (i, card) in cards.iter().enumerate() {
        // Start at GAP (not 0) so there is always room above the first card
        // for a top insert without immediately triggering another rebalance.
        db.query("UPDATE type::thing('cards', $id) SET position = $pos")
            .bind(("id", card.id.id.to_raw()))
            .bind(("pos", (i as i32 + 1) * POSITION_GAP))
            .await?;
    }
    Ok(())
}

/// Compute a sparse position for inserting a brand-new card at the TOP of
/// `col_id` (index 0 in the sorted sibling list).  Unlike
/// `compute_sparse_position` there is no card to exclude, so we query all
/// existing cards in the column.
async fn compute_top_position(db: &Surreal<Db>, col_id: &str) -> Result<i32, surrealdb::Error> {
    let col_cards: Vec<DbCard> = db
        .query(
            "SELECT * FROM cards \
             WHERE column = type::thing('columns', $col_id) \
             ORDER BY position ASC",
        )
        .bind(("col_id", col_id.to_string()))
        .await?
        .take(0)?;

    let new_pos = midpoint_position(&col_cards, 0);

    // If the gap between the sentinel (0) and the current first card has been
    // exhausted, rebalance the whole column before computing the new position.
    if !col_cards.is_empty() && needs_rebalance(&col_cards, 0, new_pos) {
        rebalance_column(db, col_id).await?;

        let col_cards: Vec<DbCard> = db
            .query(
                "SELECT * FROM cards \
                 WHERE column = type::thing('columns', $col_id) \
                 ORDER BY position ASC",
            )
            .bind(("col_id", col_id.to_string()))
            .await?
            .take(0)?;

        return Ok(midpoint_position(&col_cards, 0));
    }

    Ok(new_pos)
}

/// Compute a single sparse position value for moving `card_id` to index
/// `target_index` within `col_id`.  Only the moved card is ever written;
/// no other cards are modified in the happy path.
async fn compute_sparse_position(
    db: &Surreal<Db>,
    card_id: &str,
    col_id: &str,
    target_index: i32,
) -> Result<i32, surrealdb::Error> {
    // Fetch sibling cards (the moving card excluded) so we see the column
    // as it will look after the move.
    let col_cards: Vec<DbCard> = db
        .query(
            "SELECT * FROM cards \
             WHERE column = type::thing('columns', $col_id) \
               AND id != type::thing('cards', $card_id) \
             ORDER BY position ASC",
        )
        .bind(("col_id", col_id.to_string()))
        .bind(("card_id", card_id.to_string()))
        .await?
        .take(0)?;

    let idx = (target_index as usize).min(col_cards.len());
    let new_pos = midpoint_position(&col_cards, idx);

    if needs_rebalance(&col_cards, idx, new_pos) {
        // Gap exhausted — renumber the column then recompute.  After a
        // rebalance every gap is exactly POSITION_GAP, so the second
        // midpoint_position call is guaranteed to succeed.
        rebalance_column(db, col_id).await?;

        let col_cards: Vec<DbCard> = db
            .query(
                "SELECT * FROM cards \
                 WHERE column = type::thing('columns', $col_id) \
                   AND id != type::thing('cards', $card_id) \
                 ORDER BY position ASC",
            )
            .bind(("col_id", col_id.to_string()))
            .bind(("card_id", card_id.to_string()))
            .await?
            .take(0)?;

        let idx = (target_index as usize).min(col_cards.len());
        return Ok(midpoint_position(&col_cards, idx));
    }

    Ok(new_pos)
}

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

/// `GET /api/cards/by-number/:number` — fetch a card by its human-readable
/// sequential number. Card numbers are globally unique (single counter), so no
/// board scoping is needed. Used by the frontend when the URL carries
/// `?card=<number>` instead of the internal ULID.
pub async fn get_card_by_number(
    State(state): State<AppState>,
    Path(number): Path<u32>,
) -> Result<Json<shared::Card>, StatusCode> {
    let card: Option<DbCard> = state
        .db
        .query("SELECT * FROM cards WHERE number = $number LIMIT 1")
        .bind(("number", number as i64))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => Ok(Json(c.into_api())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn create_card(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::CreateCardRequest>,
) -> Result<(StatusCode, Json<shared::Card>), StatusCode> {
    let column: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Destructure early to capture the board ID for the SSE event.
    let column = match column {
        Some(c) => c,
        None => return Err(StatusCode::NOT_FOUND),
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
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let card_number = counter.map(|c| c.count).unwrap_or(1);

    // Compute the sparse position for inserting at the TOP of the column.
    // This is done before the CREATE so the position is known up front;
    // the two-step approach is safe because card IDs are ULIDs and the
    // counter increment above already serialises concurrent creates.
    let top_pos = compute_top_position(&state.db, &col_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => {
            let api_card = c.into_api();
            let snapshot_after = serde_json::to_value(api_card.clone())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let _ = state.events.send(BroadcastEvent {
                board_id,
                event: BoardEvent::CardCreated {
                    card: api_card.clone(),
                },
            });
            Ok((StatusCode::CREATED, Json(api_card)))
        }
        None => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn update_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    claims: Extension<Claims>,
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

    let snapshot_before = serde_json::to_value(existing.clone().into_api())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Always look up the current column so we have the board ID for the SSE event.
    let current_col: Option<DbColumn> = state
        .db
        .select(("columns", existing.column.id.to_raw()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let board_id = current_col
        .as_ref()
        .map(|c| c.board.id.to_raw())
        .unwrap_or_default();

    // Validate target column if provided, and guard against cross-board moves.
    if let Some(col_id) = payload.column_id.as_ref() {
        let target_col: Option<DbColumn> = state
            .db
            .select(("columns", col_id.as_str()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let target_col = match target_col {
            Some(c) => c,
            None => return Err(StatusCode::NOT_FOUND),
        };

        if let Some(ref current_col) = current_col
            && current_col.board.id.to_raw() != target_col.board.id.to_raw()
        {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    // Tags arrive as a full replacement list; normalize before deciding whether
    // this request changes anything, so a request that only re-sends the tags a
    // card already has is treated as a no-op rather than an edit.
    let tags = payload.tags.as_deref().map(normalize_tags).transpose()?;
    let tags_changed = tags
        .as_ref()
        .is_some_and(|new_tags| *new_tags != existing.tags);

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
    if tags_changed {
        set_parts.push("tags = $tags".to_string());
    }

    // Nothing changed — return the existing card unchanged.
    if set_parts.is_empty() {
        return Ok(Json(existing.into_api()));
    }

    // Always stamp the editor — every successful mutation records who did it.
    set_parts.push("last_edited_by = $editor".to_string());

    let query_str = format!(
        "UPDATE type::thing('cards', $card_id) SET {}",
        set_parts.join(", ")
    );

    // Layout-only changes are "move" (history toggles / filters). Body edits — alone or
    // combined with position/column in one PUT — stay "update" so audit_edit_session merge works.
    // A tag change is content, not layout, so it keeps the row out of "move" too: tagging a
    // card must never be something the history drawer's "show moves" toggle hides.
    let has_layout_change = payload.column_id.is_some() || payload.position.is_some();
    let has_body_change = payload.body.is_some();
    let is_move_audit = has_layout_change && !has_body_change && !tags_changed;

    let mut q = state
        .db
        .query(query_str)
        .bind(("card_id", card_id))
        .bind(("editor", editor_sub(&claims)));
    if let Some(body) = payload.body {
        q = q.bind(("body", body));
    }
    if let Some(col_id) = payload.column_id {
        q = q.bind(("col_id", col_id));
    }
    if let Some(position) = payload.position {
        q = q.bind(("position", position));
    }
    if tags_changed {
        // `tags_changed` is only true when `tags` is `Some`; the fallback keeps
        // the bind total without an unwrap that could panic.
        q = q.bind(("tags", tags.unwrap_or_default()));
    }

    let card: Option<DbCard> = q
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => {
            let api_card = c.into_api();
            let snapshot_after = serde_json::to_value(api_card.clone())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let action = if is_move_audit { "move" } else { "update" };
            // Every tag change gets its own discrete audit row. Merging one into
            // an in-flight body-edit session would hide it behind a "+12 chars"
            // summary and make the tag delta unrecoverable from history, so the
            // session token is deliberately dropped whenever tags moved.
            let audit_edit_session = if action == "update" && !tags_changed {
                payload
                    .audit_edit_session
                    .as_deref()
                    .filter(|s| !s.is_empty())
            } else {
                None
            };

            audit::record_and_broadcast(
                &state.db,
                &state.events,
                audit::AuditRecord {
                    claims: &claims,
                    board_id: board_id.clone(),
                    entity_type: "card",
                    entity_id: &api_card.id,
                    action,
                    snapshot_before: Some(snapshot_before),
                    snapshot_after: Some(snapshot_after),
                    restored_from: None,
                    batch_group: None,
                    audit_edit_session,
                },
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let _ = state.events.send(BroadcastEvent {
                board_id,
                event: BoardEvent::CardUpdated {
                    card: api_card.clone(),
                },
            });
            Ok(Json(api_card))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn delete_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    claims: Extension<Claims>,
) -> Result<StatusCode, StatusCode> {
    let existing: Option<DbCard> = state
        .db
        .select(("cards", &card_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let existing = match existing {
        Some(e) => e,
        None => return Err(StatusCode::NOT_FOUND),
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

    let snapshot_before = serde_json::to_value(existing.clone().into_api())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match state
        .db
        .delete::<Option<DbCard>>(("cards", &card_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
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
        None => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn move_card(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    claims: Extension<Claims>,
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

    let snapshot_before = serde_json::to_value(existing.clone().into_api())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    // Board ID for the SSE event — always available from the target column.
    let board_id = target_col.board.id.to_raw();

    if let Some(current_col) = current_col
        && current_col.board.id.to_raw() != board_id
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Compute a sparse position so only this one card needs to be written.
    // Other cards in the column are unchanged in the happy path; a rebalance
    // is triggered automatically when the gap between neighbours is exhausted.
    let new_pos =
        compute_sparse_position(&state.db, &card_id, &payload.column_id, payload.position)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let card: Option<DbCard> = state
        .db
        .query(
            "UPDATE type::thing('cards', $card_id) \
             SET column = type::thing('columns', $col_id), position = $position, last_edited_by = $editor",
        )
        .bind(("card_id", card_id.clone()))
        .bind(("col_id", payload.column_id.clone()))
        .bind(("position", new_pos))
        .bind(("editor", editor_sub(&claims)))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match card {
        Some(c) => {
            let api_card = c.into_api();
            let snapshot_after = serde_json::to_value(api_card.clone())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            audit::record_and_broadcast(
                &state.db,
                &state.events,
                audit::AuditRecord {
                    claims: &claims,
                    board_id: board_id.clone(),
                    entity_type: "card",
                    entity_id: &api_card.id,
                    action: "move",
                    snapshot_before: Some(snapshot_before),
                    snapshot_after: Some(snapshot_after),
                    restored_from: None,
                    batch_group: None,
                    audit_edit_session: None,
                },
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let _ = state.events.send(BroadcastEvent {
                board_id,
                event: BoardEvent::CardMoved {
                    card: api_card.clone(),
                    from_column_id,
                },
            });
            Ok(Json(api_card))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
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
) -> Result<Json<Vec<shared::Card>>, StatusCode> {
    let column: Option<DbColumn> = state
        .db
        .select(("columns", &col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let Some(column) = column else {
        return Err(StatusCode::NOT_FOUND);
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
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    for (card, &position) in target.iter().zip(slots.iter()) {
        if card.position == position {
            continue;
        }
        let card_id = card.id.id.to_raw();
        let snapshot_before = serde_json::to_value((*card).clone().into_api())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .take(0)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }

        let api_card = card_after.into_api();
        let snapshot_after = serde_json::to_value(api_card.clone())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        moved.push(api_card);
    }

    // Broadcast only once every position has landed, so no `CardMoved` — the
    // event that actually repositions a card — describes a column that is
    // still mid-write. (The loop above is not silent: `record_and_broadcast`
    // emits an `AuditAppended` per row as it goes. Those do not move cards, so
    // a listener acting only on `CardMoved` never sees the transient state.)
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

    let ordered: Vec<DbCard> = state
        .db
        .query(
            "SELECT * FROM cards WHERE column = type::thing('columns', $id) ORDER BY position ASC",
        )
        .bind(("id", col_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .take(0)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ordered.into_iter().map(DbCard::into_api).collect()))
}
