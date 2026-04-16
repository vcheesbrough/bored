use leptos::prelude::*;

#[component]
pub fn AddCardModal(
    // These are plain `String` values, not signals — they're set once when the
    // column is created and never change while the column exists.
    column_id: String,
    column_name: String,
    // `show` is owned by the parent (`ColumnView`) and shared here so both the
    // `+` button and the modal's Cancel/close can toggle visibility.
    show: RwSignal<bool>,
    on_created: Callback<shared::Card>, // called after the card is successfully created
) -> impl IntoView {
    let title_input = RwSignal::new(String::new());
    let desc_input = RwSignal::new(String::new());

    // A plain closure (not a `Callback`) because it's only called from within
    // this component and doesn't need to be `Copy`.
    let close = move || {
        show.set(false);
        title_input.set(String::new()); // clear fields so they're blank next time the modal opens
        desc_input.set(String::new());
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let title = title_input.get_untracked();
        if title.trim().is_empty() {
            return; // don't create a card with a blank title
        }
        let desc = desc_input.get_untracked();
        // Store `None` for description if the user left it blank.
        let desc_val = if desc.trim().is_empty() { None } else { Some(desc) };
        // `column_id` was moved into this closure from the function argument.
        // `.clone()` here because the closure itself is `move` — each call to
        // `on_submit` would consume `column_id` without the clone.
        let col_id = column_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_card(&col_id, title, desc_val).await {
                Ok(card) => {
                    on_created.run(card); // tell `ColumnView` to append this card
                    // Reset the inputs and hide the modal.
                    title_input.set(String::new());
                    desc_input.set(String::new());
                    show.set(false);
                }
                Err(e) => leptos::logging::error!("failed to create card: {e}"),
            }
        });
    };

    view! {
        // `style:display` reactively sets the CSS `display` property.
        // "flex" shows the backdrop centred; "none" hides it entirely.
        // We use display toggling rather than conditional rendering (<Show>) so
        // the DOM node persists and doesn't reset its state when hidden.
        <div
            class="modal-backdrop"
            style:display=move || if show.get() { "flex" } else { "none" }
            on:click=move |_| close()
        >
            <div class="modal" on:click=|ev| ev.stop_propagation()>
                <button class="modal-close" on:click=move |_| close()>"×"</button>
                // `column_name.clone()` because `column_name` is a `String` captured
                // by value; the `view!` macro needs the value to be valid for the
                // lifetime of the DOM, so we pass a clone.
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
