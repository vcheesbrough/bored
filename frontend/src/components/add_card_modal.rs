use leptos::prelude::*;

#[component]
pub fn AddCardModal(
    column_id: String,
    column_name: String,
    show: RwSignal<bool>,
    on_created: Callback<shared::Card>,
) -> impl IntoView {
    let title_input = RwSignal::new(String::new());
    let desc_input = RwSignal::new(String::new());

    let close = move || {
        show.set(false);
        title_input.set(String::new());
        desc_input.set(String::new());
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let title = title_input.get_untracked();
        if title.trim().is_empty() {
            return;
        }
        let desc = desc_input.get_untracked();
        let desc_val = if desc.trim().is_empty() { None } else { Some(desc) };
        let col_id = column_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_card(&col_id, title, desc_val).await {
                Ok(card) => {
                    on_created.run(card);
                    title_input.set(String::new());
                    desc_input.set(String::new());
                    show.set(false);
                }
                Err(e) => leptos::logging::error!("failed to create card: {e}"),
            }
        });
    };

    view! {
        <div
            class="modal-backdrop"
            style:display=move || if show.get() { "flex" } else { "none" }
            on:click=move |_| close()
        >
            <div class="modal" on:click=|ev| ev.stop_propagation()>
                <button class="modal-close" on:click=move |_| close()>"×"</button>
                <p class="modal-section-label">"New card in "" {column_name.clone()} """</p>
                <form on:submit=on_submit>
                    <input
                        type="text"
                        class="modal-title-input"
                        placeholder="Card title"
                        prop:value=move || title_input.get()
                        on:input=move |ev| title_input.set(event_target_value(&ev))
                    />
                    <textarea
                        placeholder="Description (optional)"
                        prop:value=move || desc_input.get()
                        on:input=move |ev| desc_input.set(event_target_value(&ev))
                    />
                    <div class="modal-actions">
                        <button type="button" class="btn-ghost" on:click=move |_| close()>"Cancel"</button>
                        <button type="submit">"Add card"</button>
                    </div>
                </form>
            </div>
        </div>
    }
}
