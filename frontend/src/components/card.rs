use leptos::prelude::*;

#[component]
pub fn CardItem(card: shared::Card, on_click: Callback<shared::Card>) -> impl IntoView {
    let card_clone = card.clone();
    view! {
        <div class="card-item" on:click=move |_| on_click.run(card_clone.clone())>
            <span class="card-title">{card.title.clone()}</span>
        </div>
    }
}
