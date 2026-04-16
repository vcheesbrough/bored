use leptos::prelude::*;

#[component]
pub fn CardItem(
    card: RwSignal<shared::Card>,
    on_click: Callback<shared::Card>,
) -> impl IntoView {
    view! {
        <div class="card-item" on:click=move |_| on_click.run(card.get_untracked())>
            <span class="card-title">{move || card.get().title.clone()}</span>
        </div>
    }
}
