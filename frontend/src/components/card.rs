use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map};
use leptos_router::NavigateOptions;

use crate::components::column::ColumnCards;
use crate::components::markdown::MarkdownPreview;
use crate::events::DragPayload;

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
) -> impl IntoView {
    // ── Contexts ─────────────────────────────────────────────────────────
    // All three are provided by `ColumnView`; accessed here without prop drilling.
    let drag_payload =
        use_context::<RwSignal<DragPayload>>().expect("drag_payload context missing");
    let column_cards = use_context::<ColumnCards>().expect("column_cards context missing");
    let drag_over_card_id =
        use_context::<RwSignal<Option<String>>>().expect("drag_over_card_id context missing");

    // Board ID from the URL — needed to build the maximise navigation URL.
    let params = use_params_map();
    let board_id = move || params.with(|p| p.get("id").unwrap_or_default());
    // `NavigateFn` is not `Copy`, so wrap it in `StoredValue` (which IS Copy)
    // so that `on_maximize_click` can be captured by the `view!` macro's `Fn`
    // closure without making it `FnOnce`.
    let navigate = StoredValue::new(use_navigate());

    // ── State machine ─────────────────────────────────────────────────────
    let card_state: RwSignal<CardState> = RwSignal::new(CardState::Collapsed);

    // Local copy of the body — edited without reactively reading `card`,
    // so saves don't cause the whole card to re-render mid-keystroke.
    let body: RwSignal<String> = RwSignal::new(card.get_untracked().body.clone());
    // The last body value confirmed saved to the server, used for dirty detection.
    let saved_body: RwSignal<String> = RwSignal::new(card.get_untracked().body.clone());
    let save_status: RwSignal<SaveStatus> = RwSignal::new(SaveStatus::Idle);

    // Sync local body from the card signal when an external SSE update arrives,
    // but skip the sync while the user is actively typing to avoid clobbering edits.
    Effect::new(move |_| {
        let c = card.get();
        if card_state.get_untracked() != CardState::Editing {
            body.set(c.body.clone());
            saved_body.set(c.body);
        }
    });

    let number = Signal::derive(move || card.get().number);
    // Derive a read-only signal so MarkdownPreview only re-renders on body changes.
    let body_signal = Signal::derive(move || body.get());

    // ── Save helpers ──────────────────────────────────────────────────────

    // Fires a PUT request with the given body and updates save state on completion.
    // All captured values are `Copy` signals, so this closure is `Copy` and can be
    // shared across multiple event handlers without explicit cloning.
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
                    // Propagate the server-confirmed data back to the parent signal
                    // so the column list stays in sync immediately (SSE also follows).
                    card.set(updated);
                }
                Err(e) => {
                    save_status.set(SaveStatus::Failed);
                    leptos::logging::error!("card save failed: {e}");
                }
            }
        });
    };

    // Debounced auto-save: fires 500 ms after the user stops typing.
    // Each keystroke schedules a future; the future bails out if the body
    // changed again during the sleep (indicating more typing happened).
    let on_body_input = move |ev: leptos::ev::Event| {
        let new_body = event_target_value(&ev);
        body.set(new_body.clone());
        save_status.set(SaveStatus::Idle);

        let snapshot = new_body;
        let card_id = card.get_untracked().id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            TimeoutFuture::new(500).await;
            // Only save if still in Editing state and no further edits occurred.
            if card_state.get_untracked() == CardState::Editing && body.get_untracked() == snapshot
            {
                do_save(card_id, body.get_untracked());
            }
        });
    };

    // Flush any unsaved edit to the server and transition back to Expanded.
    // Used on textarea blur and Escape keydown.
    let exit_editing = move || {
        let current = body.get_untracked();
        let last_saved = saved_body.get_untracked();
        if current != last_saved {
            do_save(card.get_untracked().id.clone(), current);
        }
        card_state.set(CardState::Expanded);
    };

    // Flush any unsaved edit and collapse the card back to preview mode.
    let collapse = move || {
        let current = body.get_untracked();
        let last_saved = saved_body.get_untracked();
        if current != last_saved {
            do_save(card.get_untracked().id.clone(), current);
        }
        card_state.set(CardState::Collapsed);
    };

    // ── Delete / maximize ─────────────────────────────────────────────────
    let on_delete_click = move |e: leptos::ev::MouseEvent| {
        e.stop_propagation();
        let card_id = card.get_untracked().id.clone();
        let card_id_cb = card_id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::delete_card(&card_id).await {
                Ok(()) => on_delete.run(card_id_cb),
                Err(e) => leptos::logging::error!("delete card failed: {e}"),
            }
        });
    };

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
            // CSS class drives styling for expanded/editing states.
            class:card-expanded=move || card_state.get() != CardState::Collapsed
            class:card-editing=move || card_state.get() == CardState::Editing
            // Disable native drag-and-drop while the card is open, so the user
            // can select text without accidentally dragging the card.
            draggable=move || if is_collapsed() { "true" } else { "false" }

            // ── Drag-and-drop handlers (mirrors previous card.rs) ───────
            on:dragstart=move |_: web_sys::DragEvent| {
                // Guard: only start a drag from collapsed state.
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
                    // Skip ghost for the card being dragged — no-op insertion.
                    if dragged_id != &this_id {
                        drag_over_card_id.set(Some(this_id));
                    }
                }
            }
            on:drop=move |e: web_sys::DragEvent| {
                e.prevent_default();
                // Stop propagation so the card-list drop handler doesn't also
                // fire and append the card to the bottom instead.
                e.stop_propagation();
                if let DragPayload::Card { card_id: dragged_id, .. } =
                    drag_payload.get_untracked()
                {
                    let target = card.get_untracked();
                    let col_id = target.column_id.clone();
                    // Compute the sibling-adjusted insertion index.  The target
                    // card's visual index must be decremented when the dragged
                    // card is in the same column and currently sits before it,
                    // because removing it shifts all subsequent cards left by one.
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

            // Outer click: advance Collapsed → Expanded.  Clicks within the
            // expanded content stop propagation so they don't re-trigger this.
            on:click=move |_| {
                if card_state.get_untracked() == CardState::Collapsed {
                    card_state.set(CardState::Expanded);
                }
            }
        >
            <span class="card-number">{move || format!("#{:03}", number.get())}</span>

            // ── Collapsed: clamped preview with fade ──────────────────────
            <Show when=is_collapsed>
                <MarkdownPreview body=body_signal class="card-preview" />
            </Show>

            // ── Expanded or Editing ───────────────────────────────────────
            <Show when=move || !is_collapsed()>
                // Toolbar visible in both Expanded and Editing states.
                // stop_propagation prevents the outer card click from firing.
                <div
                    class="card-toolbar"
                    on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()
                >
                    <button
                        class="card-toolbar-btn"
                        title="Collapse"
                        on:click=move |e: leptos::ev::MouseEvent| {
                            e.stop_propagation();
                            collapse();
                        }
                    >"↑ Collapse"</button>
                    <button
                        class="card-toolbar-btn"
                        title="Maximise"
                        on:click=on_maximize_click
                    >"🗖"</button>
                    <button
                        class="card-toolbar-btn card-toolbar-delete btn-danger"
                        on:click=on_delete_click
                    >"Delete"</button>
                </div>

                // Expanded: rendered markdown body; click enters Editing.
                // Editing: textarea with debounced auto-save.
                <Show
                    when=is_expanded
                    fallback=move || view! {
                        // ── Editing state ──────────────────────────────────
                        <textarea
                            class="card-body-textarea"
                            prop:value=move || body.get()
                            on:input=on_body_input
                            on:blur=move |_| exit_editing()
                            on:keydown=move |ev: web_sys::KeyboardEvent| {
                                if ev.key() == "Escape" {
                                    exit_editing();
                                }
                            }
                            // Prevent the outer click handler from treating
                            // clicks inside the textarea as a state transition.
                            on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()
                            autofocus=true
                        />
                        <span class="card-save-status">
                            {move || match save_status.get() {
                                SaveStatus::Idle    => "",
                                SaveStatus::Saving  => "Saving…",
                                SaveStatus::Saved   => "Saved",
                                SaveStatus::Failed  => "Save failed",
                            }}
                        </span>
                    }
                >
                    // ── Expanded state ─────────────────────────────────────
                    <div
                        class="card-body-rendered"
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
                </Show>
            </Show>
        </div>
    }
}
