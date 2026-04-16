use leptos::prelude::*;

use crate::components::add_card_modal::AddCardModal;
use crate::components::card::CardItem;
use crate::components::card_modal::CardModal;

#[component]
pub fn ColumnView(
    // `RwSignal<shared::Column>` lets `BoardChooser` rename this column and have
    // the new name appear here instantly — both components share the same signal.
    column: RwSignal<shared::Column>,
) -> impl IntoView {
    // Each card is also wrapped in an `RwSignal` so that saving edits in the
    // modal updates the card title in the list without a full re-fetch.
    let cards: RwSignal<Vec<RwSignal<shared::Card>>> = RwSignal::new(Vec::new());

    // `None` means no card is open in the modal; `Some(card)` opens the modal for that card.
    let selected_card: RwSignal<Option<shared::Card>> = RwSignal::new(None);
    let show_add = RwSignal::new(false); // controls whether the add-card modal is visible

    // Read the column's initial data once, outside of any reactive context.
    // `get_untracked` avoids subscribing — the column ID and name are stable.
    let initial = column.get_untracked();
    let col_id = initial.id.clone();
    // Clone the ID a second time for the fetch closure — the first clone is moved
    // into the effect, and the second is passed to `AddCardModal`.
    let col_id_for_fetch = col_id.clone();
    let col_name_for_modal = initial.name.clone();

    // Fetch this column's cards once on mount. The `|_|` effect argument is the
    // previous run's return value; we ignore it here because this effect only
    // runs once (none of the signals inside it are read, so no re-runs).
    Effect::new(move |_| {
        let id = col_id_for_fetch.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::fetch_cards(&id).await {
                Ok(fetched) => cards.set(fetched.into_iter().map(RwSignal::new).collect()),
                Err(e) => leptos::logging::error!("failed to fetch cards: {e}"),
            }
        });
    });

    // `Callback<T>` is a wrapper around a closure that is `Copy` (cheaply cloneable).
    // Unlike a plain `move` closure, a `Callback` can be stored in multiple places
    // without needing separate clones for each event handler.
    let on_card_click = Callback::new(move |card: shared::Card| {
        selected_card.set(Some(card)); // open the modal with this card
    });

    // Called by `CardModal` after a successful save — updates the card in the list.
    let on_card_updated = Callback::new(move |updated: shared::Card| {
        // `with_untracked` reads the signal without subscribing.
        // We iterate to find the matching signal and update it in place.
        cards.with_untracked(|cs| {
            if let Some(sig) = cs.iter().find(|s| s.get_untracked().id == updated.id) {
                sig.set(updated); // updating this inner signal re-renders just that card
            }
        });
    });

    // Called by `CardModal` after a successful delete — removes the card from the list.
    let on_card_delete = Callback::new(move |card_id: String| {
        // `.retain(|s| ...)` keeps only elements for which the closure returns `true`.
        cards.update(|cs| cs.retain(|s| s.get_untracked().id != card_id));
        selected_card.set(None); // close the modal
    });

    // Called by `AddCardModal` after a successful create — appends the new card.
    let on_card_created = Callback::new(move |card: shared::Card| {
        cards.update(|cs| cs.push(RwSignal::new(card)));
    });

    view! {
        <div class="column-view">
            <div class="column-header">
                // `move || column.get().name.clone()` reads `column` reactively —
                // if the chooser renames the column, this text updates automatically.
                <span class="column-name">{move || column.get().name.clone()}</span>
                <button
                    class="add-card-btn"
                    title="Add card"
                    on:click=move |_| show_add.set(true)
                >"+"</button>
            </div>

            <div class="card-list">
                // Keyed list — each card is identified by its ID so Leptos can
                // add/remove/reorder individual cards efficiently.
                <For
                    each=move || cards.get()
                    key=|sig| sig.get_untracked().id.clone()
                    children={
                        let on_card_click = on_card_click.clone();
                        move |sig| view! { <CardItem card=sig on_click=on_card_click.clone() /> }
                    }
                />
            </div>

            // Both modals are always mounted but hidden when not in use.
            // `CardModal` watches `selected_card` and shows itself when it's `Some`.
            <CardModal card=selected_card on_updated=on_card_updated on_delete=on_card_delete />
            <AddCardModal
                column_id=col_id
                column_name=col_name_for_modal
                show=show_add
                on_created=on_card_created
            />
        </div>
    }
}
