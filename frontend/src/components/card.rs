use leptos::prelude::*;

use crate::components::markdown::MarkdownPreview;

#[component]
pub fn CardItem(
    // `RwSignal<shared::Card>` so that saving edits in the modal can update the
    // rendered preview in the list without re-fetching the whole column.
    card: RwSignal<shared::Card>,
    // `Callback<shared::Card>` is `Copy`, so it can be captured by the `on:click`
    // closure without needing an explicit `.clone()` at the call site.
    on_click: Callback<shared::Card>,
) -> impl IntoView {
    // Derive a reactive `Signal<String>` from the card signal so `MarkdownPreview`
    // gets a fresh body whenever the card is updated after a modal save.
    let body = Signal::derive(move || card.get().body);

    view! {
        <div class="card-item" on:click=move |_| on_click.run(card.get_untracked())>
            // `card-preview` applies the CSS line-clamp so only ~3 lines show.
            <MarkdownPreview body=body class="card-preview" />
        </div>
    }
}
