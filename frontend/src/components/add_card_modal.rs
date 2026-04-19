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

    let submit_disabled = move || body_input.get().trim().is_empty();

    view! {
        <div
            class="modal-backdrop"
            style:display=move || if show.get() { "flex" } else { "none" }
            on:click=move |_| close()
        >
            <div class="modal" on:click=|ev| ev.stop_propagation()>
                <button class="modal-close" on:click=move |_| close()>"×"</button>
                // Column name interpolated correctly — previous code had a
                // quoting bug that rendered the Rust expression literally.
                <p class="modal-section-label">
                    "New card in " {column_name.clone()}
                </p>
                <form on:submit=on_submit>
                    <textarea
                        class="modal-body-textarea"
                        placeholder="Card content (markdown supported)"
                        prop:value=move || body_input.get()
                        on:input=move |ev| body_input.set(event_target_value(&ev))
                        autofocus=true
                    />
                    <div class="modal-actions">
                        <button type="button" class="btn-ghost" on:click=move |_| close()>
                            "Cancel"
                        </button>
                        <button type="submit" prop:disabled=submit_disabled>
                            "Add card"
                        </button>
                    </div>
                </form>
            </div>
        </div>
    }
}
