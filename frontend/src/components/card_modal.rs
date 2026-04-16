use leptos::prelude::*;

#[component]
pub fn CardModal(
    // `Option<shared::Card>` — `None` = modal closed, `Some(card)` = modal open for that card.
    // The parent owns the signal; this component reads and writes it.
    card: RwSignal<Option<shared::Card>>,
    on_updated: Callback<shared::Card>, // called after a successful save
    on_delete: Callback<String>,        // called after a successful delete (passes the card ID)
) -> impl IntoView {
    let title_input = RwSignal::new(String::new());
    let desc_input = RwSignal::new(String::new());

    // Whenever the selected card changes (a different card is opened), sync the
    // input fields to that card's current data.
    Effect::new(move |_| {
        if let Some(c) = card.get() {
            title_input.set(c.title.clone());
            // `unwrap_or_default()` converts `None` → empty string.
            desc_input.set(c.description.clone().unwrap_or_default());
        }
    });

    let on_save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default(); // stop the browser doing a full-page form submit
        // `get_untracked` — we only need the current value, not a reactive subscription.
        if let Some(c) = card.get_untracked() {
            let card_id = c.id.clone();
            let title = title_input.get_untracked();
            let desc = desc_input.get_untracked();
            // Treat a blank description the same as no description — store `None` in the DB.
            let desc_val = if desc.trim().is_empty() { None } else { Some(desc) };
            wasm_bindgen_futures::spawn_local(async move {
                let req = shared::UpdateCardRequest {
                    title: Some(title),
                    // `Some(desc_val)` wraps the inner Option in the outer Option.
                    // The backend interprets `Some(None)` as "clear the field",
                    // and `Some(Some(s))` as "set it to s".
                    description: Some(desc_val),
                    position: None,   // not changing position
                    column_id: None,  // not moving columns
                };
                match crate::api::update_card(&card_id, req).await {
                    Ok(updated) => {
                        on_updated.run(updated); // tell the parent list to refresh this card
                        card.set(None);          // close the modal
                    }
                    Err(e) => leptos::logging::error!("failed to update card: {e}"),
                }
            });
        }
    };

    let on_delete_click = move |_| {
        if let Some(c) = card.get_untracked() {
            let card_id = c.id.clone();
            // We need two separate owned strings because both the `async` block
            // and the `on_delete.run(...)` call need to own the ID.
            let card_id_cb = card_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match crate::api::delete_card(&card_id).await {
                    Ok(()) => on_delete.run(card_id_cb), // remove from parent list
                    Err(e) => leptos::logging::error!("failed to delete card: {e}"),
                }
            });
        }
    };

    view! {
        // `<Show>` conditionally renders its children. The `when` closure is reactive —
        // it re-evaluates whenever `card` changes.
        <Show when=move || card.get().is_some() fallback=|| ()>
            // Clicking the backdrop (outside the modal box) closes the modal.
            <div class="modal-backdrop" on:click=move |_| card.set(None)>
                // `ev.stop_propagation()` prevents clicks inside the modal from
                // bubbling up to the backdrop and accidentally closing it.
                <div class="modal" on:click=|ev| ev.stop_propagation()>
                    <button class="modal-close" on:click=move |_| card.set(None)>"×"</button>
                    <form on:submit=on_save>
                        <input
                            type="text"
                            class="modal-title-input"
                            prop:value=move || title_input.get()
                            on:input=move |ev| title_input.set(event_target_value(&ev))
                        />
                        <textarea
                            class="card-desc-input"
                            placeholder="Description…"
                            prop:value=move || desc_input.get()
                            on:input=move |ev| desc_input.set(event_target_value(&ev))
                        />
                        <div class="modal-actions">
                            <button type="button" class="btn-danger" on:click=on_delete_click>
                                "Delete"
                            </button>
                            <button type="submit">"Save"</button>
                        </div>
                    </form>
                </div>
            </div>
        </Show>
    }
}
