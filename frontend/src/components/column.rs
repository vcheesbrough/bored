use leptos::prelude::*;

use crate::components::card::CardItem;
use crate::components::card_modal::CardModal;

#[component]
pub fn ColumnView(column: shared::Column) -> impl IntoView {
    let cards = RwSignal::new(Vec::<shared::Card>::new());
    let new_card_title = RwSignal::new(String::new());
    let selected_card: RwSignal<Option<shared::Card>> = RwSignal::new(None);
    let col_id = column.id.clone();

    Effect::new(move |_| {
        let id = col_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::fetch_cards(&id).await {
                Ok(fetched) => cards.set(fetched),
                Err(e) => leptos::logging::error!("failed to fetch cards: {e}"),
            }
        });
    });

    let col_id_add = column.id.clone();
    let on_add_card = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let title = new_card_title.get_untracked();
        if title.trim().is_empty() {
            return;
        }
        let id = col_id_add.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_card(&id, title, None).await {
                Ok(card) => {
                    cards.update(|cs| cs.push(card));
                    new_card_title.set(String::new());
                }
                Err(e) => leptos::logging::error!("failed to create card: {e}"),
            }
        });
    };

    let on_card_click = Callback::new(move |card: shared::Card| {
        selected_card.set(Some(card));
    });

    let on_card_delete = Callback::new(move |card_id: String| {
        cards.update(|cs| cs.retain(|c| c.id != card_id));
        selected_card.set(None);
    });

    view! {
        <div class="column-view">
            <div class="column-header">{column.name.clone()}</div>

            <div class="card-list">
                <For
                    each=move || cards.get()
                    key=|c| c.id.clone()
                    children={
                        let on_card_click = on_card_click.clone();
                        move |card| {
                            view! {
                                <CardItem card=card on_click=on_card_click.clone() />
                            }
                        }
                    }
                />
            </div>

            <div class="column-footer">
                <form class="create-form" on:submit=on_add_card>
                    <input
                        type="text"
                        placeholder="Add card…"
                        prop:value=move || new_card_title.get()
                        on:input=move |ev| new_card_title.set(event_target_value(&ev))
                    />
                    <button type="submit">"Add"</button>
                </form>
            </div>

            <CardModal card=selected_card on_delete=on_card_delete />
        </div>
    }
}
