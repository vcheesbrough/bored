use leptos::prelude::*;

#[component]
pub fn CardItem(
    // `RwSignal<shared::Card>` so that saving edits in the modal can update this
    // card's title in the list without re-fetching the whole column.
    card: RwSignal<shared::Card>,
    // `Callback<shared::Card>` is `Copy`, so it can be captured by the `on:click`
    // closure without needing an explicit `.clone()` at the call site.
    on_click: Callback<shared::Card>,
) -> impl IntoView {
    view! {
        <div class="card-item" on:click=move |_| on_click.run(card.get_untracked())>
            // `move || card.get().title.clone()` is a reactive closure — it re-runs
            // whenever `card` is updated (e.g. after saving from the modal).
            <span class="card-title">{move || card.get().title.clone()}</span>
        </div>
    }
}
