use leptos::prelude::*;

use crate::components::card::CardItem;
use crate::events::{BoardSseEvent, DragPayload};

/// Context type provided by `ColumnView` so that `CardItem` children can
/// look up their own current position within the column at drop time.
#[derive(Clone, Copy)]
pub struct ColumnCards(pub RwSignal<Vec<RwSignal<shared::Card>>>);

#[component]
pub fn ColumnView(column: RwSignal<shared::Column>) -> impl IntoView {
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
    let columns_ctx =
        use_context::<RwSignal<Vec<RwSignal<shared::Column>>>>().expect("columns context missing");

    // ── Static column metadata ─────────────────────────────────────────────
    let initial = column.get_untracked();
    let col_id = initial.id.clone();
    let board_id = initial.board_id.clone();
    let col_id_fetch = col_id.clone();
    let col_id_sse = col_id.clone();
    let col_id_card_drop = col_id.clone();
    let col_id_col_drop = col_id.clone();
    let col_id_dragstart = col_id.clone();
    let col_id_for_modal = col_id.clone();

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
    let on_card_created = Callback::new(move |card: shared::Card| {
        cards.update(|cs| {
            let insert_at = cs
                .iter()
                .position(|s| s.get_untracked().position > card.position)
                .unwrap_or(cs.len());
            cs.insert(insert_at, RwSignal::new(card));
        });
    });

    // ── Drag-and-drop: card drop onto this column ──────────────────────────
    let on_cardlist_dragover = move |e: web_sys::DragEvent| {
        if matches!(drag_payload.get_untracked(), DragPayload::Card { .. }) {
            e.prevent_default();
            card_list_drag_over.set(true);
            // Do NOT clear drag_over_card_id here: when the card-ghost has
            // pointer-events:none, events over it bubble to this handler,
            // causing a flicker loop (ghost hides → card repositions → card
            // dragover fires → ghost shows → repeat).
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
        }
    };

    let on_col_drop = {
        let target_id = col_id_col_drop.clone();
        let board_id = board_id.clone();
        move |e: web_sys::DragEvent| {
            e.prevent_default();
            if let DragPayload::Column {
                column_id: dragged_id,
            } = drag_payload.get_untracked()
            {
                if dragged_id == target_id {
                    drag_payload.set(DragPayload::None);
                    return;
                }
                let new_order: Vec<String> = columns_ctx.with_untracked(|cs| {
                    let mut ids: Vec<String> =
                        cs.iter().map(|s| s.get_untracked().id.clone()).collect();
                    if let Some(drag_idx) = ids.iter().position(|id| *id == dragged_id) {
                        if let Some(tgt_idx) = ids.iter().position(|id| *id == target_id) {
                            ids.remove(drag_idx);
                            let insert_at = if drag_idx < tgt_idx {
                                tgt_idx - 1
                            } else {
                                tgt_idx
                            };
                            ids.insert(insert_at, dragged_id.clone());
                        }
                    }
                    ids
                });
                columns_ctx.update(|cs| {
                    if let Some(drag_idx) =
                        cs.iter().position(|s| s.get_untracked().id == dragged_id)
                    {
                        if let Some(tgt_idx) =
                            cs.iter().position(|s| s.get_untracked().id == target_id)
                        {
                            let removed = cs.remove(drag_idx);
                            let insert_at = if drag_idx < tgt_idx {
                                tgt_idx - 1
                            } else {
                                tgt_idx
                            };
                            cs.insert(insert_at, removed);
                        }
                    }
                });
                let bid = board_id.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(err) = crate::api::reorder_columns(&bid, new_order).await {
                        leptos::logging::error!("reorder_columns failed: {err}");
                    }
                });
                drag_payload.set(DragPayload::None);
            }
        }
    };

    view! {
        <div
            class="column-view"
            on:dragover=on_col_dragover
            on:drop=on_col_drop
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
                    }
                >"⠿"</span>
                <span class="column-name">{move || column.get().name.clone()}</span>
                <span class="card-count-badge">{card_count}</span>
                <button
                    class="add-card-btn"
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
                class:drag-over=move || card_list_drag_over.get()
                on:dragover=on_cardlist_dragover
                on:dragleave=on_cardlist_dragleave
                on:drop=on_cardlist_drop
            >
                <For
                    each=move || cards.get()
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
    }
}
