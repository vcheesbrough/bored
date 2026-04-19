use leptos::prelude::*;

use crate::components::column::ColumnCards;
use crate::components::markdown::MarkdownPreview;
use crate::events::DragPayload;

#[component]
pub fn CardItem(
    // `RwSignal<shared::Card>` so saving edits in the modal updates the
    // rendered preview in the list without re-fetching the whole column.
    card: RwSignal<shared::Card>,
    // `Callback<shared::Card>` is `Copy` so it can be captured in the click
    // closure without an explicit `.clone()` at each use site.
    on_click: Callback<shared::Card>,
) -> impl IntoView {
    // `DragPayload` context — written on dragstart so drop targets know
    // which card is in flight and which column it came from.
    let drag_payload =
        use_context::<RwSignal<DragPayload>>().expect("drag_payload context missing");

    // `ColumnCards` context — provided by the enclosing `ColumnView` so we
    // can resolve our own current index at drop time without prop-drilling.
    let column_cards = use_context::<ColumnCards>().expect("column_cards context missing");

    // `drag_over_card_id` context — written here on dragover so the column
    // can render a ghost placeholder at the correct insertion point.
    let drag_over_card_id =
        use_context::<RwSignal<Option<String>>>().expect("drag_over_card_id context missing");

    // Derive reactive signals so each derived view re-renders only when its
    // specific field changes, not on every card update.
    let body = Signal::derive(move || card.get().body);
    let number = Signal::derive(move || card.get().number);

    view! {
        <div
            class="card-item"
            // `draggable="true"` makes the browser start a drag operation when
            // the user clicks-and-drags this element.
            draggable="true"
            on:dragstart=move |_: web_sys::DragEvent| {
                // Capture current card data without subscribing reactively.
                let c = card.get_untracked();
                // Write the drag context so any column's drop handler can
                // read the card ID and source column without DataTransfer.
                drag_payload.set(DragPayload::Card {
                    card_id: c.id.clone(),
                    from_column_id: c.column_id.clone(),
                });
            }
            // Allow dropping a dragged card onto this card.
            on:dragover=move |e: web_sys::DragEvent| {
                let payload = drag_payload.get_untracked();
                if let DragPayload::Card { card_id: ref dragged_id, .. } = payload {
                    e.prevent_default();
                    e.stop_propagation();
                    let this_id = card.get_untracked().id;
                    // Skip ghost for the card being dragged — inserting before
                    // itself is a no-op so no placeholder is needed.
                    if dragged_id != &this_id {
                        drag_over_card_id.set(Some(this_id));
                    }
                }
            }
            // Handle a card being dropped directly onto this card, which
            // moves the dragged card to this card's current position.
            on:drop=move |e: web_sys::DragEvent| {
                e.prevent_default();
                // Critical: stop propagation so the card-list drop handler
                // doesn't also fire and append the card to the bottom instead.
                e.stop_propagation();
                if let DragPayload::Card { card_id: dragged_id, .. } =
                    drag_payload.get_untracked()
                {
                    let target = card.get_untracked();
                    let col_id = target.column_id.clone();
                    // Compute the insertion index for the backend's siblings list
                    // (which excludes the dragged card).  The target card's visual
                    // index must be adjusted by -1 when the dragged card is in the
                    // same column AND currently sits before the target — removing it
                    // shifts everything after it left by one slot.
                    let pos = column_cards.0.with_untracked(|cs| {
                        let target_idx = cs
                            .iter()
                            .position(|s| s.get_untracked().id == target.id)
                            .unwrap_or(0);
                        let drag_before_target = cs
                            .iter()
                            .position(|s| s.get_untracked().id == dragged_id)
                            .map(|di| di < target_idx)
                            .unwrap_or(false);
                        if drag_before_target {
                            (target_idx - 1) as i32
                        } else {
                            target_idx as i32
                        }
                    });
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(err) =
                            crate::api::move_card(&dragged_id, col_id, pos).await
                        {
                            leptos::logging::error!("move_card failed: {err}");
                        }
                        // The SSE CardMoved event updates the UI for all clients.
                    });
                    drag_payload.set(DragPayload::None);
                }
            }
            on:click=move |_| on_click.run(card.get_untracked())
        >
            <span class="card-number">{move || format!("#{:03}", number.get())}</span>
            <MarkdownPreview body=body class="card-preview" />
        </div>
    }
}
