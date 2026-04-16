use leptos::prelude::*;

use crate::components::add_card_modal::AddCardModal;
use crate::components::card::CardItem;
use crate::components::card_modal::CardModal;

#[component]
pub fn ColumnView(column: shared::Column) -> impl IntoView {
    let cards: RwSignal<Vec<RwSignal<shared::Card>>> = RwSignal::new(Vec::new());
    let selected_card: RwSignal<Option<shared::Card>> = RwSignal::new(None);
    let show_add = RwSignal::new(false);
    let col_id = column.id.clone();

    Effect::new(move |_| {
        let id = col_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::fetch_cards(&id).await {
                Ok(fetched) => cards.set(fetched.into_iter().map(RwSignal::new).collect()),
                Err(e) => leptos::logging::error!("failed to fetch cards: {e}"),
            }
        });
    });

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

    view! {
        <div class="column-view">
            <div class="column-header">
                <span class="column-name">{column.name.clone()}</span>
                <button
                    class="add-card-btn"
                    title="Add card"
                    on:click=move |_| show_add.set(true)
                >"+"</button>
            </div>

            <div class="card-list">
                <For
                    each=move || cards.get()
                    key=|sig| sig.get_untracked().id.clone()
                    children={
                        let on_card_click = on_card_click.clone();
                        move |sig| view! { <CardItem card=sig on_click=on_card_click.clone() /> }
                    }
                />
            </div>

            <CardModal card=selected_card on_updated=on_card_updated on_delete=on_card_delete />
            <AddCardModal
                column_id=column.id.clone()
                column_name=column.name.clone()
                show=show_add
                on_created=on_card_created
            />
        </div>
    }
}
