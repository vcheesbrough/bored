use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

use crate::components::markdown::MarkdownPreview;

// Status of the background auto-save, shown in the modal footer.
#[derive(Clone, PartialEq)]
enum SaveStatus {
    Idle,
    Saving,
    Saved,
    Failed,
}

#[component]
pub fn CardModal(
    // `None` = closed, `Some(card)` = open for that card.
    card: RwSignal<Option<shared::Card>>,
    on_updated: Callback<shared::Card>,
    on_delete: Callback<String>,
) -> impl IntoView {
    // The markdown body being edited. Kept in sync with the open card on open.
    let body = RwSignal::new(String::new());
    // Whether the body region is in edit (textarea) or rendered (preview) mode.
    let editing = RwSignal::new(false);
    // Last body value successfully persisted to the server, used to detect unsaved changes.
    let saved_body = RwSignal::new(String::new());
    let save_status = RwSignal::new(SaveStatus::Idle);

    // When a different card is opened, reset all state to match the new card.
    Effect::new(move |_| {
        if let Some(c) = card.get() {
            body.set(c.body.clone());
            saved_body.set(c.body.clone());
            editing.set(false);
            save_status.set(SaveStatus::Idle);
        }
    });

    // Performs a PUT to the server with the current body and updates state accordingly.
    // Returns the card ID that was saved, for the on_updated callback.
    let do_save = move |card_id: String, current_body: String| {
        save_status.set(SaveStatus::Saving);
        wasm_bindgen_futures::spawn_local(async move {
            let req = shared::UpdateCardRequest {
                body: Some(current_body.clone()),
                position: None,
                column_id: None,
            };
            match crate::api::update_card(&card_id, req).await {
                Ok(updated) => {
                    saved_body.set(current_body);
                    save_status.set(SaveStatus::Saved);
                    on_updated.run(updated);
                }
                Err(e) => {
                    save_status.set(SaveStatus::Failed);
                    leptos::logging::error!("auto-save failed: {e}");
                }
            }
        });
    };

    // Debounced save: fires 500 ms after the user stops typing.
    // Each keystroke spawns a future that sleeps, then checks if the body has
    // changed again during the sleep — if yes, the sleep winner bails out to let
    // the newer future handle the save. This avoids multiple concurrent saves.
    let on_body_input = move |ev: leptos::ev::Event| {
        let new_body = event_target_value(&ev);
        body.set(new_body.clone());
        save_status.set(SaveStatus::Idle);

        // Capture what we intend to save *now* so the async block can compare
        // after the timeout to see if more typing happened in the meantime.
        let body_at_schedule = new_body;
        if let Some(c) = card.get_untracked() {
            let card_id = c.id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                TimeoutFuture::new(500).await;
                // Check that no further edits happened during the sleep.
                let current = body.get_untracked();
                if current == body_at_schedule {
                    do_save(card_id, current);
                }
            });
        }
    };

    // Flushes any unsaved body immediately — called when the modal is about to close.
    let flush_and_close = move || {
        let current = body.get_untracked();
        let last_saved = saved_body.get_untracked();
        if current != last_saved {
            if let Some(c) = card.get_untracked() {
                do_save(c.id.clone(), current);
            }
        }
        card.set(None);
        editing.set(false);
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

    // Derive a Signal<String> for MarkdownPreview from the mutable body signal.
    let body_signal = Signal::derive(move || body.get());

    view! {
        <Show when=move || card.get().is_some() fallback=|| ()>
            <div class="modal-backdrop" on:click=move |_| flush_and_close()>
                <div class="modal" on:click=|ev| ev.stop_propagation()>
                    <button class="modal-close" on:click=move |_| flush_and_close()>"×"</button>

                    // Body region: switches between rendered markdown and a raw textarea.
                    // Both occupy the same layout box so the modal doesn't jump.
                    <div class="modal-body-region">
                        // Rendered view — visible when not editing.
                        <Show when=move || !editing.get() fallback=|| ()>
                            <div
                                class="modal-body-rendered"
                                // Clicking the rendered view enters edit mode.
                                on:click=move |_| editing.set(true)
                            >
                                // Show a muted placeholder when the body is empty.
                                <Show
                                    when=move || !body.get().is_empty()
                                    fallback=move || view! {
                                        <p class="modal-body-placeholder">"Click to edit…"</p>
                                    }
                                >
                                    <MarkdownPreview body=body_signal class="modal-markdown" />
                                </Show>
                            </div>
                        </Show>

                        // Edit view — visible when editing.
                        <Show when=move || editing.get() fallback=|| ()>
                            <textarea
                                class="modal-body-textarea"
                                prop:value=move || body.get()
                                on:input=on_body_input
                                // Blurring exits edit mode and returns to the rendered view.
                                on:blur=move |_| editing.set(false)
                                // Escape also exits edit mode.
                                on:keydown=move |ev| {
                                    if ev.key() == "Escape" {
                                        editing.set(false);
                                    }
                                }
                                // `node_ref` + autofocus would be ideal but auto:focus on
                                // <Show> mount is sufficient here via the CSS :focus style.
                                autofocus=true
                            />
                        </Show>
                    </div>

                    // Footer: save status indicator + delete action.
                    <div class="modal-footer">
                        <span class="save-status">
                            {move || match save_status.get() {
                                SaveStatus::Idle => "",
                                SaveStatus::Saving => "Saving…",
                                SaveStatus::Saved => "Saved",
                                SaveStatus::Failed => "Save failed",
                            }}
                        </span>
                        <button type="button" class="btn-danger" on:click=on_delete_click>
                            "Delete"
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
