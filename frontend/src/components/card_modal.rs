use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;

use crate::components::confirm_modal::ConfirmModal;
use crate::components::markdown::MarkdownPreview;

#[derive(Clone, PartialEq)]
enum SaveStatus {
    Idle,
    Saving,
    Saved,
    Failed,
}

/// Full-screen card detail modal — shown when `?card=<id>` is in the URL.
///
/// Interaction model mirrors the inline `CardItem`:
///   rendered view → click body → textarea (auto-save) → blur/Escape → rendered
///
/// The toolbar mirrors the inline card toolbar:
///   [#number]  [🗗 restore]  [Delete]
///
/// `on_close` is called after flushing any pending edits so the caller can
/// navigate away (e.g. remove the `?card=` query parameter from the URL).
#[component]
pub fn CardModal(
    /// `None` = hidden, `Some(card)` = open for that card.
    card: RwSignal<Option<shared::Card>>,
    on_updated: Callback<shared::Card>,
    on_delete: Callback<String>,
    /// Invoked when the user closes or minimises the modal, after any pending
    /// save has been flushed. Use this to pop the route / query parameter.
    on_close: Callback<()>,
) -> impl IntoView {
    let body = RwSignal::new(String::new());
    let editing = RwSignal::new(false);
    let saved_body = RwSignal::new(String::new());
    let save_status = RwSignal::new(SaveStatus::Idle);

    // Reset all local state when a new card is opened.
    Effect::new(move |_| {
        if let Some(c) = card.get() {
            body.set(c.body.clone());
            saved_body.set(c.body.clone());
            editing.set(false);
            save_status.set(SaveStatus::Idle);
        }
    });

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
                    leptos::logging::error!("modal auto-save failed: {e}");
                }
            }
        });
    };

    // Debounced auto-save: 500 ms after the last keystroke.
    let on_body_input = move |ev: leptos::ev::Event| {
        let new_body = event_target_value(&ev);
        body.set(new_body.clone());
        save_status.set(SaveStatus::Idle);

        let snapshot = new_body;
        if let Some(c) = card.get_untracked() {
            let card_id = c.id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                TimeoutFuture::new(500).await;
                if card.get_untracked().is_none() {
                    return;
                }
                let current = body.get_untracked();
                if current == snapshot {
                    do_save(card_id, current);
                }
            });
        }
    };

    // Flush any unsaved body then close. The modal stays open during the flush
    // so the user sees a failure if the request errors out.
    let flush_and_close = move || {
        let current = body.get_untracked();
        let last_saved = saved_body.get_untracked();
        if current != last_saved {
            if let Some(c) = card.get_untracked() {
                let card_id = c.id.clone();
                save_status.set(SaveStatus::Saving);
                wasm_bindgen_futures::spawn_local(async move {
                    let req = shared::UpdateCardRequest {
                        body: Some(current.clone()),
                        position: None,
                        column_id: None,
                    };
                    match crate::api::update_card(&card_id, req).await {
                        Ok(updated) => {
                            on_updated.run(updated);
                            card.set(None);
                            editing.set(false);
                            on_close.run(());
                        }
                        Err(e) => {
                            save_status.set(SaveStatus::Failed);
                            leptos::logging::error!("modal flush save failed: {e}");
                        }
                    }
                });
                return;
            }
        }
        card.set(None);
        editing.set(false);
        on_close.run(());
    };

    let show_confirm = RwSignal::new(false);

    let on_delete_click = move |_| show_confirm.set(true);

    let on_confirmed = Callback::new(move |_: ()| {
        if let Some(c) = card.get_untracked() {
            let card_id = c.id.clone();
            let card_id_cb = card_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match crate::api::delete_card(&card_id).await {
                    Ok(()) => {
                        card.set(None);
                        on_close.run(());
                        on_delete.run(card_id_cb);
                    }
                    Err(e) => leptos::logging::error!("modal delete failed: {e}"),
                }
            });
        }
    });

    let body_signal = Signal::derive(move || body.get());

    view! {
        <Show when=move || card.get().is_some() fallback=|| ()>
            // Full-screen overlay — clicking the backdrop (which is unreachable
            // since the modal fills the screen) would close, but the real close
            // path is the Minimise button in the toolbar.
            <div class="modal-backdrop">
                <div class="modal" on:click=|ev| ev.stop_propagation()>

                    // ── Toolbar: mirrors the inline card-toolbar ──────────────
                    <div class="modal-toolbar">
                        <span class="modal-card-number">
                            {move || card.get().map(|c| format!("#{:03}", c.number)).unwrap_or_default()}
                        </span>
                        <span class="card-save-status">
                            {move || match save_status.get() {
                                SaveStatus::Idle   => "",
                                SaveStatus::Saving => "Saving…",
                                SaveStatus::Saved  => "Saved",
                                SaveStatus::Failed => "Save failed",
                            }}
                        </span>
                        <button
                            class="card-toolbar-btn"
                            title="Restore to board"
                            on:click=move |_| flush_and_close()
                        >"🗗"</button>
                        <button
                            class="card-toolbar-btn card-toolbar-close"
                            title="Delete"
                            on:click=on_delete_click
                        >"✕"</button>
                    </div>

                    // ── Body region: rendered markdown ↔ textarea toggle ──────
                    <div class="modal-body-region">
                        <Show when=move || !editing.get() fallback=|| ()>
                            <div
                                class="modal-body-rendered"
                                on:click=move |_| editing.set(true)
                            >
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

                        <Show when=move || editing.get() fallback=|| ()>
                            <textarea
                                class="modal-body-textarea"
                                prop:value=move || body.get()
                                on:input=on_body_input
                                on:blur=move |_| editing.set(false)
                                on:keydown=move |ev| {
                                    if ev.key() == "Escape" {
                                        editing.set(false);
                                    }
                                }
                                autofocus=true
                            />
                        </Show>
                    </div>
                </div>

                <ConfirmModal show=show_confirm on_confirm=on_confirmed />
            </div>
        </Show>
    }
}
