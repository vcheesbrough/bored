use leptos::prelude::*;

use crate::components::add_card_modal::AddCardModal;
use crate::components::card::CardItem;
use crate::components::card_modal::CardModal;
use crate::events::{BoardSseEvent, DragPayload};

/// Context type provided by `ColumnView` so that `CardItem` children can
/// look up their own current position within the column at drop time.
/// Using a dedicated newtype avoids colliding with any other
/// `RwSignal<Vec<…>>` that might exist in the context chain.
#[derive(Clone, Copy)]
pub struct ColumnCards(pub RwSignal<Vec<RwSignal<shared::Card>>>);

#[component]
pub fn ColumnView(
    // `RwSignal<shared::Column>` lets `BoardChooser` rename this column and
    // have the updated name appear in the header instantly.
    column: RwSignal<shared::Column>,
) -> impl IntoView {
    let cards: RwSignal<Vec<RwSignal<shared::Card>>> = RwSignal::new(Vec::new());
    let selected_card: RwSignal<Option<shared::Card>> = RwSignal::new(None);
    let show_add = RwSignal::new(false);
    // Tracks whether a card is currently dragged over *this* column's card list.
    // Drives the CSS `.drag-over` class so the outline only appears during an
    // actual drag, not on ordinary mouse hover.
    let card_list_drag_over = RwSignal::new(false);
    // The card ID currently being hovered over during a drag.  `CardItem`
    // children write to this so the column can render a ghost placeholder
    // in the right position without any prop drilling.
    let drag_over_card_id: RwSignal<Option<String>> = RwSignal::new(None);

    // Expose this column's cards signal to `CardItem` children so they can
    // resolve their own index at drop time without needing it as a prop.
    provide_context(ColumnCards(cards));
    provide_context(drag_over_card_id);

    // ── Context ────────────────────────────────────────────────────────────
    // These signals are provided by `BoardView` via `provide_context`.
    let sse_event =
        use_context::<RwSignal<Option<BoardSseEvent>>>().expect("sse_event context missing");
    let drag_payload =
        use_context::<RwSignal<DragPayload>>().expect("drag_payload context missing");
    let columns_ctx =
        use_context::<RwSignal<Vec<RwSignal<shared::Column>>>>().expect("columns context missing");

    // ── Static column metadata ─────────────────────────────────────────────
    // Read once, untracked — the column ID and board ID don't change over the
    // lifetime of this component instance.
    let initial = column.get_untracked();
    let col_id = initial.id.clone();
    let board_id = initial.board_id.clone();
    // Extra clones for closures that each need ownership of `col_id`.
    let col_id_fetch = col_id.clone();
    let col_id_sse = col_id.clone();
    let col_id_card_drop = col_id.clone();
    let col_id_col_drop = col_id.clone();
    let col_id_dragstart = col_id.clone();
    let col_id_for_modal = col_id.clone();

    let col_name_for_modal = initial.name.clone();

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
    // This effect re-runs every time `sse_event` changes. We filter for events
    // that affect this column and ignore the rest.
    Effect::new(move |_| {
        let Some(event) = sse_event.get() else { return };
        match event {
            BoardSseEvent::CardCreated { card } if card.column_id == col_id_sse => {
                // Spawn outside the effect's reactive scope so the new signal
                // is owned by the global arena (not the per-run effect scope).
                // Without this, the signal would be disposed the next time the
                // effect re-runs, causing get_untracked() panics in `retain`.
                // Also guards against the on_card_created callback having
                // already inserted this card (optimistic local update).
                wasm_bindgen_futures::spawn_local(async move {
                    cards.update(|cs| {
                        if cs.iter().any(|s| s.get_untracked().id == card.id) {
                            return;
                        }
                        cs.push(RwSignal::new(card));
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
                // We don't know the column from the event; check if the card is ours.
                let owned =
                    cards.with_untracked(|cs| cs.iter().any(|s| s.get_untracked().id == card_id));
                if owned {
                    cards.update(|cs| cs.retain(|s| s.get_untracked().id != card_id));
                    // Close modal if the deleted card was open.
                    if selected_card
                        .with_untracked(|sc| sc.as_ref().map(|c| c.id == card_id).unwrap_or(false))
                    {
                        selected_card.set(None);
                    }
                }
            }
            BoardSseEvent::CardMoved {
                ref card,
                ref from_column_id,
            } => {
                if *from_column_id == col_id_sse && card.column_id == col_id_sse {
                    // Within-column reorder: remove the existing signal from
                    // its old slot, update its data, and re-insert at the new
                    // position.  Reusing the same RwSignal keeps the `For`
                    // component from remounting the card component.
                    let card = card.clone();
                    cards.update(|cs| {
                        if let Some(idx) = cs.iter().position(|s| s.get_untracked().id == card.id) {
                            let sig = cs.remove(idx);
                            sig.set(card.clone());
                            let pos = (card.position as usize).min(cs.len());
                            cs.insert(pos, sig);
                        }
                    });
                } else if *from_column_id == col_id_sse {
                    // Cross-column move — this column is the source: remove.
                    let id = card.id.clone();
                    cards.update(|cs| cs.retain(|s| s.get_untracked().id != id));
                } else if card.column_id == col_id_sse {
                    // Cross-column move — this column is the destination: insert.
                    // Use spawn_local to create the signal outside the effect's
                    // per-run reactive scope.  If created synchronously here the
                    // signal would be disposed on the next SSE event, causing
                    // get_untracked() panics in subsequent retain() calls.
                    let card = card.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        cards.update(|cs| {
                            if cs.iter().any(|s| s.get_untracked().id == card.id) {
                                return;
                            }
                            let pos = (card.position as usize).min(cs.len());
                            cs.insert(pos, RwSignal::new(card));
                        });
                    });
                }
            }
            _ => {}
        }
    });

    // ── Manual card callbacks (from modal) ─────────────────────────────────
    let on_card_click = Callback::new(move |card: shared::Card| {
        selected_card.set(Some(card));
    });

    let on_card_updated = Callback::new(move |updated: shared::Card| {
        cards.with_untracked(|cs| {
            if let Some(sig) = cs.iter().find(|s| s.get_untracked().id == updated.id) {
                sig.set(updated);
            }
        });
    });

    let on_card_delete = Callback::new(move |card_id: String| {
        cards.update(|cs| cs.retain(|s| s.get_untracked().id != card_id));
        selected_card.set(None);
    });

    let on_card_created = Callback::new(move |card: shared::Card| {
        cards.update(|cs| cs.push(RwSignal::new(card)));
    });

    // ── Drag-and-drop: card drop onto this column ──────────────────────────
    // When a card is dragged over this card list, prevent the browser's default
    // "no-drop" behavior so the drop event can fire.
    let on_cardlist_dragover = move |e: web_sys::DragEvent| {
        if matches!(drag_payload.get_untracked(), DragPayload::Card { .. }) {
            // `prevent_default()` on dragover is required to enable dropping.
            e.prevent_default();
            // Show the drop-zone outline only while a card drag is active.
            card_list_drag_over.set(true);
        }
    };

    // Clear the outline and ghost only when the cursor truly leaves the
    // card-list bounds.  Without the relatedTarget check, dragleave fires
    // when the cursor enters any child element (e.g. a card), causing the
    // ghost to flicker on every movement across cards.
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
            // Clear outline and ghost regardless of whether the payload is valid.
            card_list_drag_over.set(false);
            drag_over_card_id.set(None);
            // Move is only valid when a card is in flight.
            if let DragPayload::Card {
                card_id,
                from_column_id: _,
            } = drag_payload.get_untracked()
            {
                // Append to the bottom of the column (position = current length).
                let target_col = col_id.clone();
                let position = cards.with_untracked(|cs| cs.len() as i32);
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(err) = crate::api::move_card(&card_id, target_col, position).await {
                        leptos::logging::error!("move_card failed: {err}");
                    }
                    // The SSE CardMoved event updates the UI for all clients.
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
                // Dropping a column onto itself is a no-op.
                if dragged_id == target_id {
                    drag_payload.set(DragPayload::None);
                    return;
                }

                // Compute new order: move `dragged_id` to just before `target_id`.
                let new_order: Vec<String> = columns_ctx.with_untracked(|cs| {
                    let mut ids: Vec<String> =
                        cs.iter().map(|s| s.get_untracked().id.clone()).collect();
                    if let Some(drag_idx) = ids.iter().position(|id| *id == dragged_id) {
                        if let Some(tgt_idx) = ids.iter().position(|id| *id == target_id) {
                            ids.remove(drag_idx);
                            // After removal, the target may have shifted left by 1.
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

                // Optimistically reorder the local signals so the UI updates
                // immediately, before the round-trip and SSE event arrive.
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
                    // The SSE ColumnsReordered event will sync any other clients.
                });
                drag_payload.set(DragPayload::None);
            }
        }
    };

    view! {
        // The outer div handles column-level drops (for column reordering).
        <div
            class="column-view"
            on:dragover=on_col_dragover
            on:drop=on_col_drop
        >
            <div class="column-header">
                // Grip icon — draggable to trigger column reordering.
                // Only the grip is draggable, not the whole header, so that
                // clicking the column name or the add-card button still works.
                <span
                    class="column-grip"
                    title="Drag to reorder"
                    draggable="true"
                    on:dragstart=move |_: web_sys::DragEvent| {
                        drag_payload.set(DragPayload::Column {
                            column_id: col_id_dragstart.clone(),
                        });
                    }
                    // Stop propagation so the column-level dragover doesn't
                    // fire while the user is initiating a column drag.
                    on:dragend=move |_: web_sys::DragEvent| {
                        // Reset drag state if the user drops outside a valid target.
                        if drag_payload.get_untracked() != DragPayload::None {
                            drag_payload.set(DragPayload::None);
                        }
                    }
                >"⠿"</span>

                <span class="column-name">{move || column.get().name.clone()}</span>
                <button
                    class="add-card-btn"
                    title="Add card"
                    on:click=move |_| show_add.set(true)
                >"+"</button>
            </div>

            // Card list — accepts card drops from any column.
            <div
                class="card-list"
                // `.drag-over` is toggled reactively so the dashed outline only
                // appears when a card is actually in flight over this list.
                class:drag-over=move || card_list_drag_over.get()
                on:dragover=on_cardlist_dragover
                on:dragleave=on_cardlist_dragleave
                on:drop=on_cardlist_drop
            >
                <For
                    each=move || cards.get()
                    key=|sig| sig.get_untracked().id.clone()
                    children={
                        let on_card_click = on_card_click.clone();
                        move |sig| {
                            // Capture the card ID at render time for the reactive ghost check.
                            let card_id = sig.get_untracked().id.clone();
                            view! {
                                // Ghost placeholder shown immediately before the card being
                                // hovered over, giving a live preview of the drop position.
                                <Show when=move || {
                                    drag_over_card_id.get().as_deref() == Some(card_id.as_str())
                                        && matches!(drag_payload.get(), DragPayload::Card { .. })
                                }>
                                    <div class="card-ghost" />
                                </Show>
                                <CardItem card=sig on_click=on_card_click.clone() />
                            }
                        }
                    }
                />
                // Ghost at the bottom of the list: shown when hovering over empty
                // column space (no card hovered) while a card drag is active.
                <Show when=move || {
                    drag_over_card_id.get().is_none()
                        && card_list_drag_over.get()
                        && matches!(drag_payload.get(), DragPayload::Card { .. })
                }>
                    <div class="card-ghost" />
                </Show>
            </div>

            <CardModal
                card=selected_card
                on_updated=on_card_updated
                on_delete=on_card_delete
            />
            <AddCardModal
                column_id=col_id_for_modal
                column_name=col_name_for_modal
                show=show_add
                on_created=on_card_created
            />
        </div>
    }
}
