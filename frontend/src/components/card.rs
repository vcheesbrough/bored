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
                if matches!(drag_payload.get_untracked(), DragPayload::Card { .. }) {
                    // Must prevent default to make this element a valid drop target.
                    e.prevent_default();
                    // Stop bubbling so the parent card-list dragover doesn't
                    // also fire (no harm if it does, but keeps intent clear).
                    e.stop_propagation();
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
                    // Look up this card's current index in the column at the
                    // moment of the drop — avoids stale positional data from
                    // the card signal itself (positions of other cards may have
                    // shifted since the last SSE update).
                    let pos = column_cards.0.with_untracked(|cs| {
                        cs.iter()
                            .position(|s| s.get_untracked().id == target.id)
                            .unwrap_or(0) as i32
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
