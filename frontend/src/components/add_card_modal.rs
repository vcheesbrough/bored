use leptos::prelude::*;

#[component]
pub fn AddCardModal(
    column_id: String,
    column_name: String,
    show: RwSignal<bool>,
    on_created: Callback<shared::Card>,
) -> impl IntoView {
    let body_input = RwSignal::new(String::new());

    let close = move || {
        show.set(false);
        body_input.set(String::new());
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let body = body_input.get_untracked();
        // Disable submit while body is empty — no card with no content.
        if body.trim().is_empty() {
            return;
        }
        let col_id = column_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_card(&col_id, body).await {
                Ok(card) => {
                    on_created.run(card);
                    body_input.set(String::new());
                    show.set(false);
                }
                Err(e) => leptos::logging::error!("failed to create card: {e}"),
            }
        });
    };

    // Whether submit should be disabled: body is blank.
    let submit_disabled = move || body_input.get().trim().is_empty();

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
                    // Single markdown body textarea — no separate title field.
                    <textarea
                        class="modal-body-textarea"
                        placeholder="Card content (markdown supported)"
                        prop:value=move || body_input.get()
                        on:input=move |ev| body_input.set(event_target_value(&ev))
                    />
                    <div class="modal-actions">
                        <button type="button" class="btn-ghost" on:click=move |_| close()>
                            "Cancel"
                        </button>
                        // `prop:disabled` binds reactively to the boolean signal.
                        <button type="submit" prop:disabled=submit_disabled>
                            "Add card"
                        </button>
                    </div>
                </form>
            </div>
        </div>
    }
}
