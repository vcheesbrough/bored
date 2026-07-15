use std::collections::HashSet;

use leptos::prelude::*;

use crate::components::card::CardItem;
use crate::events::{BoardSseEvent, DragOverColId, DragPayload};
use crate::search::{card_matches_query, BoardSearchQuery};

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
pub fn ColumnView(
    column: RwSignal<shared::Column>,
    on_column_drop: Callback<String>,
) -> impl IntoView {
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

    // ── Contexts from BoardView ────────────────────────────────────────────
    let sse_event =
        use_context::<RwSignal<Option<BoardSseEvent>>>().expect("sse_event context missing");
    let drag_payload =
        use_context::<RwSignal<DragPayload>>().expect("drag_payload context missing");
    let search_query = use_context::<BoardSearchQuery>()
        .expect("BoardSearchQuery context missing")
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

    // Called by CardItem when the user confirms a delete; removes from list
    // immediately before the SSE `CardDeleted` event arrives.
    let on_card_delete = Callback::new(move |card_id: String| {
        cards.update(|cs| cs.retain(|s| s.get_untracked().id != card_id));
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
        e.prevent_default();
        e.stop_propagation();
        card_list_drag_over.set(false);
        if let DragPayload::Card { card_id, .. } = drag_payload.get_untracked() {
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
                            cards
                                .get()
                                .into_iter()
                                .filter(|sig| {
                                    let card = sig.get();
                                    card_matches_query(&card, &query)
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
}
