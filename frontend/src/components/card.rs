use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map};
use leptos_router::NavigateOptions;

use crate::components::column::ColumnCards;
use crate::components::confirm_modal::ConfirmModal;
use crate::components::markdown::MarkdownPreview;
use crate::events::DragPayload;

/// Newtype wrapping the board-level "which card is currently expanded" signal.
/// Using a newtype avoids type collisions with other `RwSignal<Option<String>>`
/// contexts (e.g. `drag_over_card_id` provided by `ColumnView`).
#[derive(Clone, Copy)]
pub struct ExpandedCardId(pub RwSignal<Option<String>>);

/// Three-state interaction model for a card in the board column.
///
/// `Collapsed` → click anywhere on card → `Expanded` (renders full markdown)
/// `Expanded`  → click on body → `Editing` (shows textarea, auto-saves)
/// `Editing`   → blur / Escape → `Expanded`
///
/// The maximize button (visible in Expanded/Editing) navigates to
/// `/boards/:id?card=:card_id`, which causes `BoardView` to overlay the
/// same card in full-screen mode without remounting the board.
#[derive(Clone, PartialEq)]
enum CardState {
    Collapsed,
    Expanded,
    Editing,
}

/// Auto-save status shown below the textarea during editing.
#[derive(Clone, PartialEq)]
enum SaveStatus {
    Idle,
    Saving,
    Saved,
    Failed,
}

#[component]
pub fn CardItem(
    /// Reactive card signal shared with `ColumnView` so SSE updates propagate
    /// to the rendered preview without re-mounting this component.
    card: RwSignal<shared::Card>,
    /// Called when the card is deleted so the parent column can remove it
    /// from its list immediately (before the SSE `CardDeleted` event arrives).
    on_delete: Callback<String>,
    /// Shared signal set to a card's ID by `ColumnView` when a brand-new card
    /// should start in editing mode.  The matching `CardItem` claims the
    /// board-level expanded-card lock, enters `Editing`, then clears the signal.
    new_card_id: RwSignal<Option<String>>,
) -> impl IntoView {
    // ── Contexts ─────────────────────────────────────────────────────────
    let drag_payload =
        use_context::<RwSignal<DragPayload>>().expect("drag_payload context missing");
    let column_cards = use_context::<ColumnCards>().expect("column_cards context missing");
    let drag_over_card_id =
        use_context::<RwSignal<Option<String>>>().expect("drag_over_card_id context missing");
    // Board-level exclusive-expand lock: at most one card open at a time.
    let ExpandedCardId(expanded_card_id) =
        use_context::<ExpandedCardId>().expect("ExpandedCardId context missing");

    let params = use_params_map();
    let board_id = move || params.with(|p| p.get("id").unwrap_or_default());
    let navigate = StoredValue::new(use_navigate());

    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();
    let body_rendered_ref = NodeRef::<leptos::html::Div>::new();

    // ── State machine ─────────────────────────────────────────────────────
    let card_state: RwSignal<CardState> = RwSignal::new(CardState::Collapsed);

    let body: RwSignal<String> = RwSignal::new(card.get_untracked().body.clone());
    let saved_body: RwSignal<String> = RwSignal::new(card.get_untracked().body.clone());
    let save_status: RwSignal<SaveStatus> = RwSignal::new(SaveStatus::Idle);

    // Sync local body from SSE updates while not actively editing.
    Effect::new(move |_| {
        let c = card.get();
        if card_state.get_untracked() != CardState::Editing {
            body.set(c.body.clone());
            saved_body.set(c.body);
        }
    });

    // Focus textarea whenever the card enters editing mode.
    Effect::new(move |_| {
        if card_state.get() == CardState::Editing {
            if let Some(el) = textarea_ref.get() {
                let _ = el.focus();
            }
        }
    });

    // Focus the rendered body div when entering Expanded so keyboard Esc works
    // without the user needing to click first.
    Effect::new(move |_| {
        if card_state.get() == CardState::Expanded {
            if let Some(el) = body_rendered_ref.get() {
                let _ = el.focus();
            }
        }
    });

    let number = Signal::derive(move || card.get().number);
    let body_signal = Signal::derive(move || body.get());

    // ── Save helpers ──────────────────────────────────────────────────────

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
                    card.set(updated);
                }
                Err(e) => {
                    save_status.set(SaveStatus::Failed);
                    leptos::logging::error!("card save failed: {e}");
                }
            }
        });
    };

    let on_body_input = move |ev: leptos::ev::Event| {
        let new_body = event_target_value(&ev);
        body.set(new_body.clone());
        save_status.set(SaveStatus::Idle);

        let snapshot = new_body;
        let card_id = card.get_untracked().id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            TimeoutFuture::new(500).await;
            if card_state.get_untracked() == CardState::Editing && body.get_untracked() == snapshot
            {
                do_save(card_id, body.get_untracked());
            }
        });
    };

    // ── Collapse helpers ──────────────────────────────────────────────────

    // Flush any unsaved edit and go to Expanded (keeps the card open).
    let exit_editing = move || {
        let current = body.get_untracked();
        let last_saved = saved_body.get_untracked();
        if current != last_saved {
            do_save(card.get_untracked().id.clone(), current);
        }
        card_state.set(CardState::Expanded);
    };

    // Collapse without touching `expanded_card_id` — used when the reactive
    // Effect below kicks in because another card claimed the expanded slot.
    let collapse_silent = move || {
        let current = body.get_untracked();
        let last_saved = saved_body.get_untracked();
        if current != last_saved {
            do_save(card.get_untracked().id.clone(), current);
        }
        card_state.set(CardState::Collapsed);
    };

    // Full collapse: also clears the board-level expanded-card lock.
    let collapse = move || {
        collapse_silent();
        expanded_card_id.set(None);
    };

    // When the board-level signal points to a different card, collapse this one.
    Effect::new(move |_| {
        let active = expanded_card_id.get();
        let my_id = card.get_untracked().id.clone();
        if active.as_deref() != Some(&my_id) && card_state.get_untracked() != CardState::Collapsed {
            collapse_silent();
        }
    });

    // When `ColumnView` sets `new_card_id` to this card's ID, immediately
    // enter editing mode and claim the board-level expanded-card lock.
    // Works whether the card was mounted before or after the signal was set
    // (handles the SSE-vs-optimistic-insert race).
    Effect::new(move |_| {
        let target = new_card_id.get();
        let my_id = card.get_untracked().id.clone();
        if target.as_deref() == Some(&my_id) {
            expanded_card_id.set(Some(my_id));
            card_state.set(CardState::Editing);
            new_card_id.set(None);
        }
    });

    // ── Delete / maximize ─────────────────────────────────────────────────
    let show_confirm = RwSignal::new(false);

    let on_delete_click = move |e: leptos::ev::MouseEvent| {
        e.stop_propagation();
        show_confirm.set(true);
    };

    let on_confirmed = Callback::new(move |_: ()| {
        let card_id = card.get_untracked().id.clone();
        let card_id_cb = card_id.clone();
        expanded_card_id.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::delete_card(&card_id).await {
                Ok(()) => on_delete.run(card_id_cb),
                Err(e) => leptos::logging::error!("delete card failed: {e}"),
            }
        });
    });

    let on_maximize_click = move |e: leptos::ev::MouseEvent| {
        e.stop_propagation();
        let card_id = card.get_untracked().id.clone();
        let url = format!("/boards/{}?card={}", board_id(), card_id);
        navigate.with_value(|nav| nav(&url, NavigateOptions::default()));
    };

    // ── Derived booleans ──────────────────────────────────────────────────
    let is_collapsed = move || card_state.get() == CardState::Collapsed;
    let is_expanded = move || card_state.get() == CardState::Expanded;

    view! {
        <div
            class="card-item"
            class:card-expanded=move || card_state.get() != CardState::Collapsed
            class:card-editing=move || card_state.get() == CardState::Editing
            draggable=move || if is_collapsed() { "true" } else { "false" }

            on:dragstart=move |_: web_sys::DragEvent| {
                if card_state.get_untracked() != CardState::Collapsed {
                    return;
                }
                let c = card.get_untracked();
                drag_payload.set(DragPayload::Card {
                    card_id: c.id.clone(),
                    from_column_id: c.column_id.clone(),
                });
            }
            on:dragover=move |e: web_sys::DragEvent| {
                let payload = drag_payload.get_untracked();
                if let DragPayload::Card { card_id: ref dragged_id, .. } = payload {
                    e.prevent_default();
                    e.stop_propagation();
                    let this_id = card.get_untracked().id;
                    if dragged_id != &this_id {
                        drag_over_card_id.set(Some(this_id));
                    }
                }
            }
            on:drop=move |e: web_sys::DragEvent| {
                e.prevent_default();
                e.stop_propagation();
                if let DragPayload::Card { card_id: dragged_id, .. } =
                    drag_payload.get_untracked()
                {
                    let target = card.get_untracked();
                    let col_id = target.column_id.clone();
                    let pos = column_cards.0.with_untracked(|cs| {
                        let target_idx = cs
                            .iter()
                            .position(|s| s.get_untracked().id == target.id)
                            .unwrap_or(0);
                        let drag_before_target = cs
                            .iter()
                            .position(|s| s.get_untracked().id == dragged_id)
                            .map(|di| di < target_idx)
                            .unwrap_or(false);
                        if drag_before_target {
                            (target_idx - 1) as i32
                        } else {
                            target_idx as i32
                        }
                    });
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(err) =
                            crate::api::move_card(&dragged_id, col_id, pos).await
                        {
                            leptos::logging::error!("move_card failed: {err}");
                        }
                    });
                    drag_payload.set(DragPayload::None);
                }
            }

            // Advance Collapsed → Expanded and claim the board-level lock.
            on:click=move |_| {
                if card_state.get_untracked() == CardState::Collapsed {
                    expanded_card_id.set(Some(card.get_untracked().id.clone()));
                    card_state.set(CardState::Expanded);
                }
            }

            // Esc while Expanded collapses the card.  Editing mode stops
            // propagation on its own Esc so this only fires from Expanded.
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Escape" && card_state.get_untracked() == CardState::Expanded {
                    collapse();
                }
            }
        >
            // ── Collapsed: absolute number badge + clamped preview ────────
            <Show when=is_collapsed>
                <span class="card-number">{move || format!("#{:03}", number.get())}</span>
                <MarkdownPreview body=body_signal class="card-preview" />
            </Show>

            // ── Expanded / Editing ────────────────────────────────────────
            <Show when=move || !is_collapsed()>
                // Floating panel: number + Win11-style buttons, absolutely
                // positioned at the top-right so card content flows beneath.
                <div
                    class="card-float-panel"
                    on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()
                >
                    <span class="card-number">{move || format!("#{:03}", number.get())}</span>
                    <button
                        class="card-toolbar-btn"
                        title="Collapse"
                        on:click=move |e: leptos::ev::MouseEvent| {
                            e.stop_propagation();
                            collapse();
                        }
                    >"─"</button>
                    <button
                        class="card-toolbar-btn"
                        title="Maximise"
                        on:click=on_maximize_click
                    >"🗖"</button>
                    <button
                        class="card-toolbar-btn card-toolbar-close"
                        title="Delete"
                        on:click=on_delete_click
                    >"✕"</button>
                </div>

                // Grid-stack body: rendered and textarea share one cell.
                <div class="card-body-wrapper">
                    <div
                        node_ref=body_rendered_ref
                        tabindex="-1"
                        class="card-body-rendered"
                        class:card-body-hidden=move || !is_expanded()
                        on:click=move |e: leptos::ev::MouseEvent| {
                            e.stop_propagation();
                            card_state.set(CardState::Editing);
                        }
                    >
                        <Show
                            when=move || !body.get().is_empty()
                            fallback=|| view! {
                                <p class="card-body-placeholder">"Click to edit…"</p>
                            }
                        >
                            <MarkdownPreview body=body_signal class="card-markdown" />
                        </Show>
                    </div>

                    <textarea
                        node_ref=textarea_ref
                        class="card-body-textarea"
                        class:card-body-hidden=is_expanded
                        prop:value=move || body.get()
                        on:input=on_body_input
                        on:blur=move |_| exit_editing()
                        on:keydown=move |ev: web_sys::KeyboardEvent| {
                            if ev.key() == "Escape" {
                                // Stop propagation so the card-item keydown
                                // handler does not also collapse immediately.
                                ev.stop_propagation();
                                exit_editing();
                            }
                        }
                        on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()
                    />
                </div>

                <span class="card-save-status">
                    {move || match save_status.get() {
                        SaveStatus::Idle    => "",
                        SaveStatus::Saving  => "Saving…",
                        SaveStatus::Saved   => "Saved",
                        SaveStatus::Failed  => "Save failed",
                    }}
                </span>
            </Show>

            <ConfirmModal show=show_confirm on_confirm=on_confirmed />
        </div>
    }
}
