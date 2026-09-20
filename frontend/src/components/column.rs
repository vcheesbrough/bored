use std::collections::HashSet;

use leptos::prelude::*;

use crate::components::card::{CardItem, ExpandedCardId};
use crate::events::{BoardSseEvent, DragOverColId, DragPayload};
use crate::links::BoardLinkIndex;
use crate::search::{BoardCardIndex, BoardSearchQuery, card_is_visible, parse_query};

/// Context type provided by `ColumnView` so that `CardItem` children can
/// look up their own current position within the column at drop time.
#[derive(Clone, Copy)]
pub struct ColumnCards(pub RwSignal<Vec<RwSignal<shared::Card>>>);

const COLLAPSED_COLUMNS_STORAGE_PREFIX: &str = "bored:collapsed-columns:";

fn collapsed_columns_storage_key(board_id: &str) -> String {
    format!("{COLLAPSED_COLUMNS_STORAGE_PREFIX}{board_id}")
}

fn load_collapsed_columns(board_id: &str) -> HashSet<String> {
    let key = collapsed_columns_storage_key(board_id);
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|storage| storage.get_item(&key).ok().flatten())
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default()
}

fn persist_column_collapsed(board_id: &str, column_id: &str, collapsed: bool) {
    let Ok(Some(storage)) = window().local_storage() else {
        return;
    };
    let key = collapsed_columns_storage_key(board_id);
    let mut collapsed_columns = load_collapsed_columns(board_id);
    if collapsed {
        collapsed_columns.insert(column_id.to_owned());
    } else {
        collapsed_columns.remove(column_id);
    }

    if collapsed_columns.is_empty() {
        let _ = storage.remove_item(&key);
    } else if let Ok(value) = serde_json::to_string(&collapsed_columns) {
        let _ = storage.set_item(&key, &value);
    }
}

#[component]
pub fn ColumnView(column: RwSignal<shared::Column>, on_column_drop: Callback<String>) -> AnyView {
    // caps monomorphization at this boundary — see CardVersionActions doc comment in history_panel.rs
    let cards: RwSignal<Vec<RwSignal<shared::Card>>> = RwSignal::new(Vec::new());
    // Tracks which card ID (if any) should open in editing mode on mount.
    // Set just before inserting the card into `cards` so the matching
    // `CardItem` picks it up as soon as the `For` loop renders it.
    let new_card_id: RwSignal<Option<String>> = RwSignal::new(None);
    // Tracks whether a card drag is currently over this column's card list,
    // driving the dashed `.drag-over` outline.
    let card_list_drag_over = RwSignal::new(false);
    // ID of the card currently being hovered over during a drag; drives
    // the ghost placeholder rendered just above that card.
    let drag_over_card_id: RwSignal<Option<String>> = RwSignal::new(None);

    // Reactive count derived from the cards list; updates automatically whenever
    // cards are added, removed, or moved by SSE events.
    let card_count = Signal::derive(move || cards.get().len());

    provide_context(ColumnCards(cards));
    provide_context(drag_over_card_id);

    // Publish this column's card list to the board-level index so the navbar
    // search can suggest tags and card numbers from the whole board. Entries
    // are keyed by column ID and withdrawn on unmount, so a deleted column
    // stops contributing suggestions immediately.
    if let Some(BoardCardIndex(index)) = use_context::<BoardCardIndex>() {
        let column_id = column.get_untracked().id;
        let registered_id = column_id.clone();
        index.update(|entries| {
            entries.retain(|(id, _)| *id != column_id);
            entries.push((column_id, cards));
        });
        on_cleanup(move || {
            index.try_update(|entries| entries.retain(|(id, _)| *id != registered_id));
        });
    }

    // ── Contexts from BoardView ────────────────────────────────────────────
    let sse_event =
        use_context::<RwSignal<Option<BoardSseEvent>>>().expect("sse_event context missing");
    let drag_payload =
        use_context::<RwSignal<DragPayload>>().expect("drag_payload context missing");
    let search_query = use_context::<BoardSearchQuery>()
        .expect("BoardSearchQuery context missing")
        .0;
    // The board-level lock naming the one expanded card, read by the card list's
    // filter so that card stays mounted whatever the query says — see
    // `card_is_visible`. `CardItem` reads the same context to drive its own
    // expand/collapse.
    let expanded_card_id = use_context::<ExpandedCardId>()
        .expect("ExpandedCardId context missing")
        .0;
    // Tracks which column a dragged column is hovering over (drives ghost).
    // DragOverColId wrapper avoids colliding with the bare RwSignal<Option<String>>
    // that ColumnView itself provides as drag_over_card_id context.
    let drag_over_col_id = use_context::<DragOverColId>()
        .expect("drag_over_col_id context missing")
        .0;
    // ── Static column metadata ─────────────────────────────────────────────
    let initial = column.get_untracked();
    let col_id = initial.id.clone();
    let board_id = initial.board_id.clone();
    let is_collapsed = RwSignal::new(load_collapsed_columns(&board_id).contains(&col_id));
    let col_id_fetch = col_id.clone();
    let col_id_sse = col_id.clone();
    let col_id_card_drop = col_id.clone();
    let col_id_col_drop = col_id.clone();
    let col_id_dragstart = col_id.clone();
    let col_id_for_modal = col_id.clone();
    let col_id_dragover = col_id.clone();
    let col_id_collapse = col_id.clone();
    let col_id_expand = col_id.clone();
    let col_id_attr = col_id.clone();
    let col_id_collapsed_dragstart = col_id.clone();
    let board_id_collapse = board_id.clone();
    let board_id_expand = board_id.clone();
    let col_id_collapsed_drop = col_id.clone();
    let col_id_sort = col_id.clone();

    // ── Initial card fetch ─────────────────────────────────────────────────
    Effect::new(move |_| {
        let id = col_id_fetch.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::fetch_cards(&id).await {
                Ok(fetched) => cards.set(fetched.into_iter().map(RwSignal::new).collect()),
                Err(e) => leptos::logging::error!("failed to fetch cards: {e}"),
            }
        });
    });

    // ── SSE card events ────────────────────────────────────────────────────
    Effect::new(move |_| {
        let Some(event) = sse_event.get() else { return };
        match event {
            BoardSseEvent::CardCreated { card } if card.column_id == col_id_sse => {
                // Guard against double-insert: `on_card_created` (below) may have
                // already inserted this card as an optimistic update.
                // Also insert at the correct sorted position — the backend now
                // assigns top-of-column positions, so `card.position` is small.
                wasm_bindgen_futures::spawn_local(async move {
                    cards.update(|cs| {
                        if cs.iter().any(|s| s.get_untracked().id == card.id) {
                            return;
                        }
                        let insert_at = cs
                            .iter()
                            .position(|s| s.get_untracked().position > card.position)
                            .unwrap_or(cs.len());
                        cs.insert(insert_at, RwSignal::new(card));
                    });
                });
            }
            BoardSseEvent::CardUpdated { card } if card.column_id == col_id_sse => {
                // Update the matching signal in-place so only that card re-renders.
                cards.with_untracked(|cs| {
                    if let Some(sig) = cs.iter().find(|s| s.get_untracked().id == card.id) {
                        sig.set(card);
                    }
                });
            }
            BoardSseEvent::CardDeleted { card_id } => {
                let owned =
                    cards.with_untracked(|cs| cs.iter().any(|s| s.get_untracked().id == card_id));
                if owned {
                    cards.update(|cs| cs.retain(|s| s.get_untracked().id != card_id));
                }
            }
            BoardSseEvent::CardMoved {
                ref card,
                ref from_column_id,
            } => {
                if *from_column_id == col_id_sse && card.column_id == col_id_sse {
                    // Within-column reorder: remove the existing signal from its old
                    // slot, update its data, and re-insert at the correct sorted
                    // position.  Reusing the same RwSignal keeps the `For` component
                    // from remounting the card component.
                    // NOTE: `card.position` is a sparse integer (e.g. 512, 1024),
                    // NOT an array index — find insertion point by comparing positions.
                    let card = card.clone();
                    cards.update(|cs| {
                        if let Some(idx) = cs.iter().position(|s| s.get_untracked().id == card.id) {
                            let sig = cs.remove(idx);
                            sig.set(card.clone());
                            let insert_at = cs
                                .iter()
                                .position(|s| s.get_untracked().position > card.position)
                                .unwrap_or(cs.len());
                            cs.insert(insert_at, sig);
                        }
                    });
                } else if *from_column_id == col_id_sse {
                    // Cross-column move — this column is the source: remove.
                    let id = card.id.clone();
                    cards.update(|cs| cs.retain(|s| s.get_untracked().id != id));
                } else if card.column_id == col_id_sse {
                    // Cross-column move — this column is the destination: insert at
                    // the correct sorted position.
                    let card = card.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        cards.update(|cs| {
                            if cs.iter().any(|s| s.get_untracked().id == card.id) {
                                return;
                            }
                            let insert_at = cs
                                .iter()
                                .position(|s| s.get_untracked().position > card.position)
                                .unwrap_or(cs.len());
                            cs.insert(insert_at, RwSignal::new(card));
                        });
                    });
                }
            }
            _ => {}
        }
    });

    // ── Card callbacks ─────────────────────────────────────────────────────

    // Always present: `ColumnView` only ever renders inside `BoardView`, which
    // provides the board's links before rendering any column. Needed by both
    // the delete callback below and `on_sort_by_links` further down.
    let links_index = expect_context::<BoardLinkIndex>();

    // Called by CardItem when the user confirms a delete; removes from list
    // immediately before the SSE `CardDeleted` event arrives.
    let on_card_delete = Callback::new(move |card_id: String| {
        cards.update(|cs| cs.retain(|s| s.get_untracked().id != card_id));
        // Prune the card's links in the same breath. The card is dropped from
        // the column locally, so waiting for the SSE `CardLinkDeleted` events to
        // prune the index leaves a tab whose stream is down or lagged showing
        // the partner card's link badge and a bare `#N` chip until a reload.
        links_index.remove_touching(&card_id);
    });

    // Called by the + button handler on successful create; inserts at the correct
    // sorted position (the backend assigns a top-of-column sparse position).
    // Guard against double-insert: the SSE CardCreated event can arrive before
    // the API response if the round-trip races, so we check before inserting.
    let on_card_created = Callback::new(move |card: shared::Card| {
        cards.update(|cs| {
            if cs.iter().any(|s| s.get_untracked().id == card.id) {
                return;
            }
            let insert_at = cs
                .iter()
                .position(|s| s.get_untracked().position > card.position)
                .unwrap_or(cs.len());
            cs.insert(insert_at, RwSignal::new(card));
        });
    });

    // ── Drag-and-drop: card drop onto this column ──────────────────────────
    let on_cardlist_dragover = move |e: web_sys::DragEvent| {
        match drag_payload.get_untracked() {
            DragPayload::Card { .. } => {
                e.prevent_default();
                card_list_drag_over.set(true);
                // Do NOT clear drag_over_card_id here: when the card-ghost has
                // pointer-events:none, events over it bubble to this handler,
                // causing a flicker loop (ghost hides → card repositions → card
                // dragover fires → ghost shows → repeat).
            }
            DragPayload::Column { .. } => {
                // Accept the drag so the full column area is a valid drop zone;
                // the drop event bubbles up to .column-view which handles the reorder.
                e.prevent_default();
            }
            DragPayload::None => {}
        }
    };

    // Clear the outline and ghost only when the cursor truly leaves the
    // card-list bounds — not when it enters a child element.
    let on_cardlist_dragleave = move |e: web_sys::DragEvent| {
        use wasm_bindgen::JsCast;
        let still_inside = e
            .related_target()
            .and_then(|rt| rt.dyn_into::<web_sys::Node>().ok())
            .and_then(|rt| {
                e.current_target()
                    .and_then(|ct| ct.dyn_into::<web_sys::Node>().ok())
                    .map(|ct| ct.contains(Some(&rt)))
            })
            .unwrap_or(false);
        if !still_inside {
            card_list_drag_over.set(false);
            drag_over_card_id.set(None);
        }
    };

    let on_cardlist_drop = {
        let col_id = col_id_card_drop.clone();
        move |e: web_sys::DragEvent| {
            e.prevent_default();
            card_list_drag_over.set(false);
            // Snapshot drag_over_card_id before clearing: the ghost shifts the
            // target card down, so the cursor may be over the ghost (not the
            // card) at drop time.  Use the hover ID to recover insertion point.
            let hover_id = drag_over_card_id.get_untracked();
            drag_over_card_id.set(None);
            if let DragPayload::Card {
                card_id,
                from_column_id: _,
            } = drag_payload.get_untracked()
            {
                let target_col = col_id.clone();
                let position = cards.with_untracked(|cs| {
                    if let Some(ref hover_card_id) = hover_id {
                        let target_idx = cs
                            .iter()
                            .position(|s| s.get_untracked().id == *hover_card_id)
                            .unwrap_or(cs.len());
                        let drag_before = cs
                            .iter()
                            .position(|s| s.get_untracked().id == card_id)
                            .map(|di| di < target_idx)
                            .unwrap_or(false);
                        if drag_before {
                            (target_idx - 1) as i32
                        } else {
                            target_idx as i32
                        }
                    } else {
                        cs.len() as i32
                    }
                });
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(err) = crate::api::move_card(&card_id, target_col, position).await {
                        leptos::logging::error!("move_card failed: {err}");
                    }
                });
                drag_payload.set(DragPayload::None);
            }
        }
    };

    // ── Sort this column's cards by their links ────────────────────────────
    // Guards against a double-click firing two overlapping reorders — the
    // second would be computed from a card order the server has already
    // replaced.
    let sorting = RwSignal::new(false);
    let on_sort_by_links = move |_: web_sys::MouseEvent| {
        if sorting.get_untracked() {
            return;
        }
        // The *whole* column, deliberately read from `cards` rather than from
        // the `<For>`'s `each` closure: that closure filters by the search
        // query, so sorting it would quietly sink every card hidden by a
        // search to the bottom of the column.
        let current: Vec<shared::Card> = cards
            .get_untracked()
            .iter()
            .map(|sig| sig.get_untracked())
            .collect();
        let current_ids: Vec<&str> = current.iter().map(|c| c.id.as_str()).collect();

        // Every link on the board; `order_by_dependency` drops the ones that
        // reach outside this column.
        let links = links_index.links.get_untracked();
        let edges = links
            .iter()
            .map(|link| (link.predecessor_id.as_str(), link.successor_id.as_str()));

        let ordered = match shared::links::order_by_dependency(&current_ids, edges) {
            Ok(ordered) => ordered,
            Err(err) => {
                // Only reachable from link rows that predate the cycle check.
                leptos::logging::error!("cannot sort column by links: {err}");
                return;
            }
        };
        if ordered == current_ids {
            // Already satisfies its links — do not spend a request, an audit
            // row, or an SSE burst saying so.
            //
            // This does mean the server's duplicate-position repair is out of
            // reach here: a column whose stored positions collide but whose
            // displayed order already satisfies its links is left alone. That
            // repair is a side effect of applying an order, not a feature of
            // this button, and paying a request on every click to maybe fix a
            // rare state is the worse trade.
            return;
        }

        let order: Vec<String> = ordered.into_iter().map(str::to_string).collect();
        let col_id = col_id_sort.clone();
        sorting.set(true);
        // No optimistic update: the column does not move until the server has
        // spoken. It just does not wait for SSE to relay what the server
        // already said — see the `Ok` arm.
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::reorder_cards(&col_id, order).await {
                // The response body *is* the column's new order, authoritative
                // and already in hand. Applying it directly rather than
                // waiting for the `CardMoved` events to come back round closes
                // two gaps at once: a big reorder spends two broadcast slots
                // per card and can lag the initiating tab out of the channel
                // (`backend/src/events.rs`), where lagged events are dropped
                // with no reconciliation short of a reload; and the sort now
                // works whether or not SSE is connected at all.
                Ok(server_cards) => apply_server_order(cards, server_cards),
                Err(err) => leptos::logging::error!("reorder_cards failed: {err}"),
            }
            sorting.set(false);
        });
    };

    // ── Drag-and-drop: column reorder via drop onto column ─────────────────
    let on_col_dragover = move |e: web_sys::DragEvent| {
        if matches!(drag_payload.get_untracked(), DragPayload::Column { .. }) {
            e.prevent_default();
            // Track which column the dragged column is hovering over so the
            // board can place the ghost on the matching insertion side.
            drag_over_col_id.set(Some(col_id_dragover.clone()));
        }
    };

    let on_col_drop = {
        let target_id = col_id_col_drop.clone();
        move |e: web_sys::DragEvent| {
            e.prevent_default();
            e.stop_propagation();
            if matches!(drag_payload.get_untracked(), DragPayload::Column { .. }) {
                on_column_drop.run(target_id.clone());
            }
        }
    };

    let on_collapsed_dragover = move |e: web_sys::DragEvent| {
        if matches!(drag_payload.get_untracked(), DragPayload::Card { .. }) {
            e.prevent_default();
            e.stop_propagation();
            card_list_drag_over.set(true);
        }
    };

    let on_collapsed_dragleave = move |e: web_sys::DragEvent| {
        use wasm_bindgen::JsCast;
        let still_inside = e
            .related_target()
            .and_then(|rt| rt.dyn_into::<web_sys::Node>().ok())
            .and_then(|rt| {
                e.current_target()
                    .and_then(|ct| ct.dyn_into::<web_sys::Node>().ok())
                    .map(|ct| ct.contains(Some(&rt)))
            })
            .unwrap_or(false);
        if !still_inside {
            card_list_drag_over.set(false);
        }
    };

    let on_collapsed_drop = move |e: web_sys::DragEvent| {
        if let DragPayload::Card { card_id, .. } = drag_payload.get_untracked() {
            e.prevent_default();
            e.stop_propagation();
            card_list_drag_over.set(false);
            let target_col = col_id_collapsed_drop.clone();
            let position = cards.with_untracked(|cs| cs.len() as i32);
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(err) = crate::api::move_card(&card_id, target_col, position).await {
                    leptos::logging::error!("move_card failed: {err}");
                }
            });
            drag_payload.set(DragPayload::None);
        }
    };

    view! {
        <div
            class="column-view"
            class:column-collapsed=move || is_collapsed.get()
            data-column-id=col_id_attr
            data-collapsed=move || is_collapsed.get().to_string()
            on:dragover=on_col_dragover
            on:drop=on_col_drop
        >
            <div
                class="column-expanded-content"
                class:is-hidden=move || is_collapsed.get()
            >
                <div class="column-header">
                    <span
                        class="column-grip"
                        title="Drag to reorder"
                        draggable="true"
                        on:dragstart=move |_: web_sys::DragEvent| {
                            drag_payload.set(DragPayload::Column {
                                column_id: col_id_dragstart.clone(),
                            });
                        }
                        on:dragend=move |_: web_sys::DragEvent| {
                            if drag_payload.get_untracked() != DragPayload::None {
                                drag_payload.set(DragPayload::None);
                            }
                            drag_over_col_id.set(None);
                        }
                    >"⠿"</span>
                    <span class="column-name">{move || column.get().name.clone()}</span>
                    <span class="card-count-badge">{card_count}</span>
                    <button
                        class="card-toolbar-btn column-sort-links-btn"
                        type="button"
                        title="Sort by links"
                        aria-label="Sort by links"
                        // Disabled until the board's links have been fetched as
                        // well as while a sort is in flight: columns paint one
                        // round trip ahead of the link index, and a click in
                        // that window would compute an order from no edges and
                        // silently do nothing.
                        prop:disabled=move || sorting.get() || !links_index.loaded.get()
                        on:click=on_sort_by_links
                    >"⇅"</button>
                    <button
                        class="card-toolbar-btn column-collapse-btn"
                        type="button"
                        title="Collapse column"
                        aria-label="Collapse column"
                        on:click=move |_| {
                            persist_column_collapsed(&board_id_collapse, &col_id_collapse, true);
                            is_collapsed.set(true);
                        }
                    >"─"</button>
                    <button
                        class="add-card-btn"
                        type="button"
                        title="Add card"
                        on:click=move |_| {
                            // Immediately create an empty card at the top of the
                            // column; the new `CardItem` starts in editing mode.
                            let col_id = col_id_for_modal.clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                match crate::api::create_card(&col_id, String::new()).await {
                                    Ok(card) => {
                                        // Signal the matching CardItem to start in
                                        // editing mode before inserting it into
                                        // the list so the For loop picks it up.
                                        new_card_id.set(Some(card.id.clone()));
                                        on_card_created.run(card);
                                    }
                                    Err(e) => leptos::logging::error!("create card failed: {e}"),
                                }
                            });
                        }
                    >"+"</button>
                </div>

                <div
                    class="card-list"
                    class:drag-over=move || {
                        // Only show outline while a drag is actually active; clears
                        // automatically when drag_payload returns to None (dragend).
                        card_list_drag_over.get() && drag_payload.get() != DragPayload::None
                    }
                    on:dragover=on_cardlist_dragover
                    on:dragleave=on_cardlist_dragleave
                    on:drop=on_cardlist_drop
                >
                    <For
                        each=move || {
                            let query = search_query.get();
                            // Read the lock once, outside the filter: it is the
                            // same for every card, and tracking it here is what
                            // makes the column re-filter when the user collapses
                            // a pinned card that no longer matches.
                            let expanded = expanded_card_id.get();
                            // The board's links, likewise once — but only for a
                            // query that has a `#N` term, because that is the
                            // only kind `card_matches_query` consults them for
                            // (card #305). Reading conditionally keeps the
                            // *subscription* conditional: a `#42` search
                            // re-filters when a link is added or removed, from
                            // this tab or over SSE, while a plain text search
                            // is not woken by link traffic anywhere on the
                            // board — and does not clone the list on every
                            // keystroke either.
                            //
                            // The cost is that `parse_query` runs once here and
                            // again inside the filter. It is a split over
                            // whitespace on a search box's worth of text, next
                            // to a per-card body match that runs for every card
                            // in the column. The coupling is the part to keep
                            // an eye on: if another kind of term ever reads
                            // `links`, this condition has to learn about it, or
                            // the column will quietly stop re-filtering for it.
                            let links = if parse_query(query.trim()).numbers.is_empty() {
                                Vec::new()
                            } else {
                                links_index.links.get()
                            };
                            cards
                                .get()
                                .into_iter()
                                .filter(|sig| {
                                    let card = sig.get();
                                    card_is_visible(&card, &query, expanded.as_deref(), &links)
                                })
                                .collect::<Vec<_>>()
                        }
                        key=|sig| sig.get_untracked().id.clone()
                        children={
                            move |sig| {
                                let card_id = sig.get_untracked().id.clone();
                                view! {
                                    // Ghost placeholder shown immediately before the hovered card.
                                    <Show when=move || {
                                        drag_over_card_id.get().as_deref() == Some(card_id.as_str())
                                            && matches!(drag_payload.get(), DragPayload::Card { .. })
                                    }>
                                        <div class="card-ghost" />
                                    </Show>
                                    <CardItem card=sig on_delete=on_card_delete new_card_id=new_card_id />
                                }
                            }
                        }
                    />
                    // End-zone: fills remaining space; acts as "append to bottom" drop target.
                    // Its own dragover clears drag_over_card_id (moving ghost here) without
                    // touching card-level state, avoiding the pointer-events flicker loop.
                    <div
                        class="card-list-end"
                        on:dragover=move |e: web_sys::DragEvent| {
                            if matches!(drag_payload.get_untracked(), DragPayload::Card { .. }) {
                                e.prevent_default();
                                drag_over_card_id.set(None);
                            }
                        }
                    >
                        <Show when=move || {
                            drag_over_card_id.get().is_none()
                                && card_list_drag_over.get()
                                && matches!(drag_payload.get(), DragPayload::Card { .. })
                        }>
                            <div class="card-ghost" />
                        </Show>
                    </div>
                </div>
            </div>

            <div
                class="collapsed-column-drop-target"
                class:is-hidden=move || !is_collapsed.get()
                class:drag-over=move || {
                    card_list_drag_over.get()
                        && matches!(drag_payload.get(), DragPayload::Card { .. })
                }
                on:dragover=on_collapsed_dragover
                on:dragleave=on_collapsed_dragleave
                on:drop=on_collapsed_drop
            >
                <span
                    class="collapsed-column-grip"
                    title="Drag to reorder"
                    draggable="true"
                    on:dragstart=move |_: web_sys::DragEvent| {
                        drag_payload.set(DragPayload::Column {
                            column_id: col_id_collapsed_dragstart.clone(),
                        });
                    }
                    on:dragend=move |_: web_sys::DragEvent| {
                        if drag_payload.get_untracked() != DragPayload::None {
                            drag_payload.set(DragPayload::None);
                        }
                        drag_over_col_id.set(None);
                        card_list_drag_over.set(false);
                    }
                >"⠿"</span>
                <span class="collapsed-column-name">{move || column.get().name.clone()}</span>
                <span class="collapsed-card-count">{card_count}</span>
                <button
                    class="card-toolbar-btn column-expand-btn"
                    type="button"
                    title="Expand column"
                    aria-label="Expand column"
                    on:click=move |_| {
                        persist_column_collapsed(&board_id_expand, &col_id_expand, false);
                        is_collapsed.set(false);
                    }
                >"🗖"</button>
            </div>

        </div>
    }
    .into_any()
}

/// Indices into `current_ids`, reordered to follow `server_ids`.
///
/// Splitting this out from [`apply_server_order`] keeps the interesting part —
/// what happens when the two lists disagree — testable without a reactive
/// runtime.
///
/// Both kinds of disagreement are tolerated rather than trusted:
///
/// * An id the server reports that this browser does not hold is **skipped**.
///   The card exists, but this tab has not been told about it yet; its own
///   `CardCreated` / `CardMoved` event will place it in the normal way.
/// * An id this browser holds that the server does not report **keeps its
///   relative order at the end**. Usually it left the column, and its
///   `CardMoved` will remove it shortly. But it can also be a card created here
///   between the server's read and this response landing, and dropping that one
///   would make a card the user just created vanish until a reload.
///
/// A repeated id in `server_ids` is honoured once; the repeat finds an
/// already-claimed slot and is skipped.
fn server_order(current_ids: &[String], server_ids: &[String]) -> Vec<usize> {
    let mut claimed = vec![false; current_ids.len()];
    let mut out = Vec::with_capacity(current_ids.len());
    for id in server_ids {
        if let Some(i) = current_ids.iter().position(|c| c == id)
            && !claimed[i]
        {
            claimed[i] = true;
            out.push(i);
        }
    }
    // Everything the server did not mention, in the order it is already in.
    out.extend(
        claimed
            .iter()
            .enumerate()
            .filter(|(_, taken)| !**taken)
            .map(|(i, _)| i),
    );
    out
}

/// Rewrites `cards` to the order the server returned, reusing the existing
/// signals.
///
/// Reuse is the point: the column renders through a keyed `<For>`, so building
/// fresh signals would remount every card component — losing focus, collapsing
/// anything expanded, and flashing the whole column. Moving the existing
/// signals mirrors what the `CardMoved` SSE handler does for a single card.
fn apply_server_order(
    cards: RwSignal<Vec<RwSignal<shared::Card>>>,
    server_cards: Vec<shared::Card>,
) {
    cards.update(|cs| {
        let current_ids: Vec<String> = cs.iter().map(|s| s.get_untracked().id).collect();
        let server_ids: Vec<String> = server_cards.iter().map(|c| c.id.clone()).collect();
        let order = server_order(&current_ids, &server_ids);

        let mut slots: Vec<Option<RwSignal<shared::Card>>> = cs.drain(..).map(Some).collect();
        *cs = order.into_iter().filter_map(|i| slots[i].take()).collect();

        // Refresh each card's data from the response as well as its slot. The
        // SSE `CardMoved` handler inserts by comparing `position` values, so
        // leaving the stale pre-sort positions in place would make the next
        // event land a card in the wrong slot.
        for card in server_cards {
            if let Some(sig) = cs.iter().find(|s| s.get_untracked().id == card.id) {
                sig.set(card);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::server_order;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn follows_the_server_order() {
        assert_eq!(
            server_order(&ids(&["a", "b", "c"]), &ids(&["c", "a", "b"])),
            vec![2, 0, 1]
        );
    }

    #[test]
    fn an_unchanged_order_is_the_identity() {
        assert_eq!(
            server_order(&ids(&["a", "b", "c"]), &ids(&["a", "b", "c"])),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn skips_server_ids_this_browser_does_not_hold() {
        // `b` was created in another tab and its SSE event has not arrived yet.
        assert_eq!(
            server_order(&ids(&["a", "c"]), &ids(&["c", "b", "a"])),
            vec![1, 0]
        );
    }

    #[test]
    fn keeps_locally_known_cards_the_server_did_not_report() {
        // `d` was created here after the server read the column; it must not
        // disappear just because the response predates it.
        assert_eq!(
            server_order(&ids(&["a", "d", "b"]), &ids(&["b", "a"])),
            vec![2, 0, 1]
        );
    }

    #[test]
    fn honours_a_repeated_server_id_once() {
        assert_eq!(
            server_order(&ids(&["a", "b"]), &ids(&["b", "b", "a"])),
            vec![1, 0]
        );
    }

    #[test]
    fn empty_lists_are_no_ops() {
        assert_eq!(server_order(&[], &[]), Vec::<usize>::new());
        assert_eq!(server_order(&ids(&["a"]), &[]), vec![0]);
        assert_eq!(server_order(&[], &ids(&["a"])), Vec::<usize>::new());
    }
}
