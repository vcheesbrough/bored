use leptos::prelude::*;

#[component]
pub fn CardModal(
    card: RwSignal<Option<shared::Card>>,
    on_updated: Callback<shared::Card>,
    on_delete: Callback<String>,
) -> impl IntoView {
    let title_input = RwSignal::new(String::new());
    let desc_input = RwSignal::new(String::new());

    Effect::new(move |_| {
        if let Some(c) = card.get() {
            title_input.set(c.title.clone());
            desc_input.set(c.description.clone().unwrap_or_default());
        }
    });

    let on_save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if let Some(c) = card.get_untracked() {
            let card_id = c.id.clone();
            let title = title_input.get_untracked();
            let desc = desc_input.get_untracked();
            let desc_val = if desc.trim().is_empty() { None } else { Some(desc) };
            wasm_bindgen_futures::spawn_local(async move {
                let req = shared::UpdateCardRequest {
                    title: Some(title),
                    description: Some(desc_val),
                    position: None,
                    column_id: None,
                };
                match crate::api::update_card(&card_id, req).await {
                    Ok(updated) => {
                        on_updated.run(updated);
                        card.set(None);
                    }
                    Err(e) => leptos::logging::error!("failed to update card: {e}"),
                }
            });
        }
    };

    let on_delete_click = move |_| {
        if let Some(c) = card.get_untracked() {
            let card_id = c.id.clone();
            let card_id_cb = card_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match crate::api::delete_card(&card_id).await {
                    Ok(()) => on_delete.run(card_id_cb),
                    Err(e) => leptos::logging::error!("failed to delete card: {e}"),
                }
            });
        }
    };

    view! {
        <Show when=move || card.get().is_some() fallback=|| ()>
            <div class="modal-backdrop" on:click=move |_| card.set(None)>
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
