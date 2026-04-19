use leptos::prelude::*;

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
    let drag_payload = use_context::<RwSignal<DragPayload>>()
        .expect("drag_payload context missing");

    // Derive a reactive body signal so `MarkdownPreview` re-renders after
    // the card is edited in the modal.
    let body = Signal::derive(move || card.get().body);

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
            on:click=move |_| on_click.run(card.get_untracked())
        >
            <MarkdownPreview body=body class="card-preview" />
        </div>
    }
}
