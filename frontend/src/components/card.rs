use gloo_timers::future::TimeoutFuture;
use leptos::leptos_dom::helpers::{WindowListenerHandle, window_event_listener};
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_params_map};

use crate::components::column::ColumnCards;
use crate::components::confirm_modal::ConfirmModal;
use crate::components::history_panel::{HistoryDrawer, HistoryIcon, HistoryScope};
use crate::components::link_editor::{LinkBadges, LinkEditor};
use crate::components::markdown::MarkdownPreview;
use crate::components::tag_editor::{TagChips, TagEditor};
use crate::events::DragPayload;
use crate::search::{BoardSearchQuery, query_matches_number};

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
/// `/boards/:slug?card=:number`, which causes `BoardView` to overlay the
/// same card in full-screen mode without remounting the board.
#[derive(Clone, Copy, PartialEq)]
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
) -> AnyView {
    // caps monomorphization at this boundary — see CardVersionActions doc comment in history_panel.rs
    // ── Contexts ─────────────────────────────────────────────────────────
    let drag_payload =
        use_context::<RwSignal<DragPayload>>().expect("drag_payload context missing");
    let column_cards = use_context::<ColumnCards>().expect("column_cards context missing");
    let columns =
        use_context::<RwSignal<Vec<RwSignal<shared::Column>>>>().expect("columns context missing");
    let drag_over_card_id =
        use_context::<RwSignal<Option<String>>>().expect("drag_over_card_id context missing");
    // Board-level exclusive-expand lock: at most one card open at a time.
    let ExpandedCardId(expanded_card_id) =
        use_context::<ExpandedCardId>().expect("ExpandedCardId context missing");
    let history_drawer = use_context::<HistoryDrawer>();

    let params = use_params_map();
    // Reads the board slug from the `:slug` route parameter.
    let board_slug = move || params.with(|p| p.get("slug").unwrap_or_default());
    let navigate = StoredValue::new(use_navigate());

    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();
    let body_rendered_ref = NodeRef::<leptos::html::Div>::new();
    // Where the click that started the edit landed, as (markdown-source offset,
    // viewport Y). Read and cleared by the focus effect below.
    let pending_caret: RwSignal<Option<(u32, Option<f64>)>> = RwSignal::new(None);

    // ── State machine ─────────────────────────────────────────────────────
    let card_state: RwSignal<CardState> = RwSignal::new(CardState::Collapsed);
    // Server merges consecutive body saves that share this token (one editing stretch).
    let edit_audit_session: RwSignal<Option<String>> = RwSignal::new(None);
    let prev_card_state = StoredValue::new(CardState::Collapsed);

    let body: RwSignal<String> = RwSignal::new(card.get_untracked().body.clone());
    let saved_body: RwSignal<String> = RwSignal::new(card.get_untracked().body.clone());
    let save_status: RwSignal<SaveStatus> = RwSignal::new(SaveStatus::Idle);
    // Coordinates are viewport-relative because the menu is fixed above the board.
    let context_menu_position: RwSignal<Option<(i32, i32)>> = RwSignal::new(None);
    let show_move_submenu = RwSignal::new(false);
    let move_submenu_opens_left = RwSignal::new(false);

    let escape_listener = StoredValue::new(None::<WindowListenerHandle>);
    Effect::new(move |_| {
        let menu_open = context_menu_position.get().is_some();
        escape_listener.update_value(|listener| {
            if menu_open && listener.is_none() {
                *listener = Some(window_event_listener(leptos::ev::keydown, move |ev| {
                    if ev.key() == "Escape" {
                        context_menu_position.set(None);
                        show_move_submenu.set(false);
                    }
                }));
            } else if !menu_open && let Some(listener) = listener.take() {
                listener.remove();
            }
        });
    });
    on_cleanup(move || {
        escape_listener.update_value(|listener| {
            if let Some(listener) = listener.take() {
                listener.remove();
            }
        });
    });

    // Sync local body from SSE updates while not actively editing.
    Effect::new(move |_| {
        let c = card.get();
        if card_state.get_untracked() != CardState::Editing {
            body.set(c.body.clone());
            saved_body.set(c.body);
        }
    });

    // Focus textarea whenever the card enters editing mode, placing the caret
    // where the click landed when the edit started from one. A card that enters
    // editing without a click — a freshly created card — has nothing pending
    // and just takes focus, as before. See `crate::caret`.
    Effect::new(move |_| {
        if card_state.get() == CardState::Editing
            && let Some(el) = textarea_ref.get()
        {
            match pending_caret.get_untracked() {
                Some((offset, anchor)) => crate::caret::place_caret(&el, offset, anchor),
                None => crate::caret::focus_only(&el),
            }
            pending_caret.set(None);
        }
    });

    // Set by the textarea's blur handler when focus is leaving the editor *for
    // another element* (the navbar search box, a tag input, any button…). The
    // focus effect below consumes it, so that one Editing → Expanded transition
    // leaves focus where the user put it. A `StoredValue` rather than a signal:
    // nothing should re-run when it changes, it is only read by that effect.
    let keep_focus_elsewhere = StoredValue::new(false);

    // Focus the rendered body div when entering Expanded so keyboard Esc works
    // without the user needing to click first.
    //
    // Except when the editor was left by focusing something else (card #401):
    // this effect runs *after* the blur, so grabbing focus here would snatch it
    // back from the control the user just clicked. Expanding by click and
    // leaving the editor with Escape never set the flag, so they still focus.
    Effect::new(move |_| {
        let state = card_state.get();
        // Consumed on *every* state change, not only on entering Expanded, so a
        // flag set by a blur that did not lead here can never linger and
        // suppress some later, unrelated expand. `try_*`: this effect can run
        // once more as the card is disposed.
        let focus_moved_on = keep_focus_elsewhere.try_get_value().unwrap_or(false);
        let _ = keep_focus_elsewhere.try_set_value(false);
        if state == CardState::Expanded
            && !focus_moved_on
            && let Some(el) = body_rendered_ref.get()
        {
            let _ = el.focus();
        }
    });

    Effect::new(move |_| {
        let st = card_state.get();
        let was = prev_card_state.get_value();
        if st == CardState::Editing && was != CardState::Editing {
            edit_audit_session.set(Some(crate::audit_edit_session::new()));
        }
        prev_card_state.set_value(st);
    });

    // `try_get` rather than `get` throughout: these render closures also
    // subscribe to the board-level search query below, so a card that the
    // search has just filtered out can be asked to re-render once more after
    // its own signals were disposed.  Reading a disposed signal traps the WASM
    // module, and the rendered output is thrown away anyway.
    //
    // The same care is needed **where these signals are read**, not only inside
    // the closures that compute them. `Signal::derive` (and `Memo`) store the
    // derived value in the reactive arena too, so it is disposed along with
    // everything else this component owns; a `number.get()` in a view closure
    // then traps even though the closure above it is `try_get`-safe. That is
    // the mistake cards #304 and #313 were: this comment, and the matching ones
    // in `TagEditor` and `LinkBadges`, each already claimed `try_get` safety
    // while applying it only inside the derives.
    //
    // **The rule these conversions actually follow** is narrower than "every
    // read in this file", so do not read it as a guarantee: it is *the reads
    // reachable from the disposal paths reproduced for #304 and #313* — an
    // expanded card unmounted by the search filter, and a card deleted from the
    // board. Those were found empirically, by re-running the reproductions
    // against a debug build (release strips `defined_at`) until they came back
    // clean, because each panic masks the next one behind it.
    //
    // Deliberately **not** converted, because no reproduction reached them:
    // `card_state` in `is_collapsed`/`is_expanded` and the `class:` closures,
    // `context_menu_position`, `show_move_submenu`, `move_submenu_opens_left`,
    // and `card` in the move-submenu. (`LinkPicker` in `link_editor.rs` was on
    // this list until card #369 reproduced its traps and converted those
    // reads.) If a new trap appears, that is where to look first, and the way
    // to find it is the debug-build loop above, not inspection.
    let number = Signal::derive(move || card.try_get().map_or(0, |c| c.number));
    let body_signal = Signal::derive(move || body.try_get().unwrap_or_default());

    // Live search query, so rendered card bodies can mark the matched text.
    // `CardItem` only ever renders inside `BoardView`, which provides it.
    let search_query = use_context::<BoardSearchQuery>()
        .expect("BoardSearchQuery context missing")
        .0;
    let highlight = Signal::derive(move || search_query.try_get().unwrap_or_default());
    // A `#42`-style query matches the number, not the body — the badge lights
    // up instead so the card still shows why it survived the filter.
    let number_is_hit = Signal::derive(move || {
        query_matches_number(
            number.try_get().unwrap_or(0),
            &highlight.try_get().unwrap_or_default(),
        )
    });

    // Tags are read straight off the card signal rather than mirrored into local
    // state: unlike the body there is no in-progress edit to protect from
    // overwrites, so an SSE update can land the moment it arrives.
    let tags = Signal::derive(move || card.try_get().map(|c| c.tags).unwrap_or_default());
    // Links live in the board-level index, keyed by card id; the editor and the
    // collapsed badge only need to know which card this is.
    let card_id_signal = Signal::derive(move || card.try_get().map(|c| c.id));

    // Persist a complete replacement tag list. Deliberately carries no
    // `audit_edit_session`: a tag change is its own discrete history row, never
    // folded into an in-flight body edit.
    let save_tags = Callback::new(move |next: Vec<String>| {
        let existing = card.get_untracked();
        let card_id = existing.id.clone();
        let previous = existing.tags;
        // Apply locally first. Adding two tags in quick succession is normal, and
        // without this the second edit would be computed from the list the server
        // last echoed back — silently dropping the first.
        card.update(|c| c.tags = next.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let req = shared::UpdateCardRequest {
                tags: Some(next.clone()),
                ..Default::default()
            };
            let result = crate::api::update_card(&card_id, req).await;
            // Only this call's own optimistic write may be acted on. If the tags
            // have moved on since — a second edit was fired while this request
            // was in flight — this response is stale whatever it says, and both
            // applying its echo and rolling it back would undo the newer edit.
            // Responses can also arrive out of order, so this covers success as
            // well as failure.
            if card.get_untracked().tags != next {
                if let Err(e) = result {
                    leptos::logging::error!("superseded tag save failed: {e}");
                }
                return;
            }
            match result {
                Ok(updated) => card.set(updated),
                Err(e) => {
                    // Put the optimistic change back the way it was, so the chips
                    // never claim a tag the server rejected.
                    card.update(|c| c.tags = previous);
                    leptos::logging::error!("tag save failed: {e}");
                }
            }
        });
    });

    // ── Save helpers ──────────────────────────────────────────────────────

    let do_save = move |card_id: String, current_body: String| {
        save_status.set(SaveStatus::Saving);
        wasm_bindgen_futures::spawn_local(async move {
            // `try_get_untracked`, because this future outlives the component in
            // exactly the case that matters: the write that triggered the save
            // can also be the write that unmounts the card. Trapping here was
            // the "it doesn't work" half of card #304 — the panic fired inside
            // the `wasm-bindgen-futures` task queue, so the PUT below never ran
            // *and* the executor was wedged for the rest of the tab's life.
            // Falling back to `None` only forfeits the audit-session grouping,
            // which costs a separate history row rather than a lost edit.
            let audit_edit_session = (card_state.try_get_untracked() == Some(CardState::Editing))
                .then(|| edit_audit_session.try_get_untracked().flatten())
                .flatten();
            let req = shared::UpdateCardRequest {
                body: Some(current_body.clone()),
                audit_edit_session,
                ..Default::default()
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
            // `try_get_untracked`, for the same reason `do_save` below uses it:
            // this future sleeps 500ms and the card can be disposed in the
            // meantime. The expanded-card pin makes that ordinary rather than
            // exotic — a pinned card is unmounted when *another* card claims
            // the board's expanded lock, which is one click away while typing.
            // `None` means the card is gone, so there is nothing to save.
            if card_state.try_get_untracked() == Some(CardState::Editing)
                && body.try_get_untracked().as_deref() == Some(snapshot.as_str())
            {
                do_save(card_id, snapshot);
            }
        });
    };

    // ── Collapse helpers ──────────────────────────────────────────────────

    // Both flush helpers read through `try_get_untracked` and write through
    // `try_set`. `collapse_silent` is called from an Effect on
    // `expanded_card_id`, and since the expanded card is pinned into the
    // filtered list that same lock write is what unmounts it — so the effect
    // and the disposal are in one batch, and this can run either side of it.
    // `None` everywhere means the card is already gone: there is no body left
    // to flush and no state left to set.

    // Flush any unsaved edit and go to Expanded (keeps the card open).
    let exit_editing = move || {
        if let (Some(current), Some(last_saved), Some(this_card)) = (
            body.try_get_untracked(),
            saved_body.try_get_untracked(),
            card.try_get_untracked(),
        ) && current != last_saved
        {
            do_save(this_card.id.clone(), current);
        }
        let _ = card_state.try_set(CardState::Expanded);
    };

    // Collapse without touching `expanded_card_id` — used when the reactive
    // Effect below kicks in because another card claimed the expanded slot.
    let collapse_silent = move || {
        if let (Some(current), Some(last_saved), Some(this_card)) = (
            body.try_get_untracked(),
            saved_body.try_get_untracked(),
            card.try_get_untracked(),
        ) && current != last_saved
        {
            do_save(this_card.id.clone(), current);
        }
        let _ = card_state.try_set(CardState::Collapsed);
    };

    // Full collapse: also clears the board-level expanded-card lock.
    let collapse = move || {
        collapse_silent();
        expanded_card_id.set(None);
    };

    // When the board-level signal points to a different card, collapse this one.
    //
    // This effect is the one the note above `exit_editing` is about: it is
    // subscribed to the very signal whose change unmounts a pinned card, so its
    // own reads have to tolerate having been disposed first.
    Effect::new(move |_| {
        let active = expanded_card_id.get();
        let (Some(this_card), Some(state)) =
            (card.try_get_untracked(), card_state.try_get_untracked())
        else {
            return;
        };
        if active.as_deref() != Some(this_card.id.as_str()) && state != CardState::Collapsed {
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
        if expanded_card_id.get_untracked().as_deref() == Some(card_id.as_str()) {
            expanded_card_id.set(None);
        }
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::delete_card(&card_id).await {
                Ok(()) => on_delete.run(card_id_cb),
                Err(e) => leptos::logging::error!("delete card failed: {e}"),
            }
        });
    });

    let on_maximize_click = move |e: leptos::ev::MouseEvent| {
        e.stop_propagation();
        // Use the card's sequential number (not ULID) in the URL so the link
        // is human-readable and stable across environment resets.
        let card_num = card.get_untracked().number;
        let url = format!("/boards/{}?card={}", board_slug(), card_num);
        navigate.with_value(|nav| nav(&url, NavigateOptions::default()));
    };

    // ── Derived booleans ──────────────────────────────────────────────────
    let is_collapsed = move || card_state.get() == CardState::Collapsed;
    let is_expanded = move || card_state.get() == CardState::Expanded;

    let move_card = move |column_id: String, position: i32| {
        let card_id = card.get_untracked().id.clone();
        context_menu_position.set(None);
        show_move_submenu.set(false);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = crate::api::move_card(&card_id, column_id, position).await {
                leptos::logging::error!("move_card failed: {err}");
            }
        });
    };

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

            on:contextmenu=move |e: web_sys::MouseEvent| {
                if card_state.get_untracked() != CardState::Collapsed {
                    return;
                }
                e.prevent_default();
                e.stop_propagation();
                const MENU_WIDTH: i32 = 176;
                const MENU_HEIGHT: i32 = 138;
                const VIEWPORT_GUTTER: i32 = 4;
                let viewport_width = window()
                    .inner_width()
                    .ok()
                    .and_then(|width| width.as_f64())
                    .map(|width| width as i32)
                    .unwrap_or(e.client_x() + MENU_WIDTH + VIEWPORT_GUTTER);
                let viewport_height = window()
                    .inner_height()
                    .ok()
                    .and_then(|height| height.as_f64())
                    .map(|height| height as i32)
                    .unwrap_or(e.client_y() + MENU_HEIGHT + VIEWPORT_GUTTER);
                let x = e.client_x().clamp(
                    VIEWPORT_GUTTER,
                    (viewport_width - MENU_WIDTH - VIEWPORT_GUTTER).max(VIEWPORT_GUTTER),
                );
                let y = e.client_y().clamp(
                    VIEWPORT_GUTTER,
                    (viewport_height - MENU_HEIGHT - VIEWPORT_GUTTER).max(VIEWPORT_GUTTER),
                );
                show_move_submenu.set(false);
                move_submenu_opens_left.set(
                    x + (MENU_WIDTH * 2) + VIEWPORT_GUTTER > viewport_width,
                );
                context_menu_position.set(Some((x, y)));
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
                <span
                    class="card-number"
                    class:card-number-hit=move || number_is_hit.try_get().unwrap_or(false)
                >{move || format!("#{}", number.try_get().unwrap_or_default())}</span>
                // One metadata line: link counts first, then the tag chips.
                // Each child renders nothing when it has nothing to say, and an
                // empty flex row has no height, so a plain card gets no gap.
                <span class="card-meta-row">
                    <LinkBadges card_id=card_id_signal />
                    <TagChips tags=tags highlight=highlight />
                </span>
                <MarkdownPreview body=body_signal class="card-preview" highlight=highlight />
            </Show>

            // ── Expanded / Editing ────────────────────────────────────────
            <Show when=move || !is_collapsed()>
                // Floating panel: number + Win11-style buttons, absolutely
                // positioned at the top-right so card content flows beneath.
                <div
                    class="card-float-panel"
                    on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()
                >
                    // Save-state icon sits left of the card number — subtle, not a distraction.
                    <span class="card-save-icon">
                        // A disposed card has no status worth drawing, so the
                        // `None` arm renders the same blank as `Idle`.
                        {move || match save_status.try_get() {
                            Some(SaveStatus::Saving)  => "·",
                            Some(SaveStatus::Saved)   => "💾",
                            Some(SaveStatus::Failed)  => "!",
                            Some(SaveStatus::Idle) | None => "",
                        }}
                    </span>
                    <span
                        class="card-number"
                        class:card-number-hit=move || number_is_hit.try_get().unwrap_or(false)
                    >{move || format!("#{}", number.try_get().unwrap_or_default())}</span>
                    <Show when=move || history_drawer.is_some() fallback=|| ()>
                        <button
                            class="card-toolbar-btn"
                            title="Card history"
                            on:click=move |e: leptos::ev::MouseEvent| {
                                e.stop_propagation();
                                if let Some(hd) = history_drawer {
                                    hd.0.set(Some(HistoryScope::Card(card.get_untracked().id.clone())));
                                }
                            }
                        >
                            <HistoryIcon />
                        </button>
                    </Show>
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

                <TagEditor tags=tags on_change=save_tags />
                <LinkEditor card_id=card_id_signal />

                // Grid-stack body: rendered and textarea share one cell.
                <div class="card-body-wrapper">
                    <div
                        node_ref=body_rendered_ref
                        tabindex="-1"
                        class="card-body-rendered"
                        class:card-body-hidden=move || !is_expanded()
                        on:click=move |e: leptos::ev::MouseEvent| {
                            e.stop_propagation();
                            pending_caret.set(crate::caret::event_container(&e).map(|container| {
                                let offset = crate::caret::source_offset_at(
                                    &container,
                                    f64::from(e.client_x()),
                                    f64::from(e.client_y()),
                                );
                                (offset.unwrap_or(0), crate::caret::anchor_of(&e))
                            }));
                            card_state.set(CardState::Editing);
                        }
                    >
                        <Show
                            // `try_get`: `body` is this component's own signal,
                            // and an edit that unmounts the card re-runs this
                            // closure after disposal. `unwrap_or_default` shows
                            // the placeholder branch, which is never seen — the
                            // card is being removed from the DOM regardless.
                            when=move || !body.try_get().unwrap_or_default().is_empty()
                            fallback=|| view! {
                                <p class="card-body-placeholder">"Click to edit…"</p>
                            }
                        >
                            <MarkdownPreview
                                body=body_signal
                                class="card-markdown"
                                highlight=highlight
                                source_positions=true
                            />
                        </Show>
                    </div>

                    <textarea
                        node_ref=textarea_ref
                        class="card-body-textarea"
                        class:card-body-hidden=is_expanded
                        prop:value=move || body.try_get().unwrap_or_default()
                        on:input=on_body_input
                        on:blur=move |ev: web_sys::FocusEvent| {
                            // `relatedTarget` is the element gaining focus, or
                            // `None` when focus is going nowhere in particular
                            // (a click on the board background, the window
                            // losing focus). Only the first case means the user
                            // chose somewhere else to type — see
                            // `keep_focus_elsewhere`.
                            //
                            // This card's own rendered body is excluded: leaving
                            // the editor with Escape focuses it, which blurs the
                            // textarea *with* that div as the related target.
                            use wasm_bindgen::JsCast;
                            let own_body = body_rendered_ref
                                .get_untracked()
                                .map(|el| el.unchecked_into::<web_sys::EventTarget>());
                            let target = ev.related_target();
                            keep_focus_elsewhere
                                .set_value(target.is_some() && target != own_body);
                            exit_editing();
                        }
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

            </Show>

            <ConfirmModal show=show_confirm on_confirm=on_confirmed />

            <Show when=move || context_menu_position.get().is_some()>
                <div
                    class="card-context-menu-backdrop"
                    on:click=move |e: leptos::ev::MouseEvent| {
                        e.stop_propagation();
                        context_menu_position.set(None);
                        show_move_submenu.set(false);
                    }
                    on:contextmenu=move |e: web_sys::MouseEvent| {
                        e.prevent_default();
                        e.stop_propagation();
                        context_menu_position.set(None);
                        show_move_submenu.set(false);
                    }
                ></div>
                <div
                    class="card-context-menu"
                    role="menu"
                    style=move || {
                        context_menu_position
                            .get()
                            .map(|(x, y)| format!("left: {x}px; top: {y}px;"))
                            .unwrap_or_default()
                    }
                    on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()
                >
                    <button
                        type="button"
                        role="menuitem"
                        on:click=move |e: leptos::ev::MouseEvent| {
                            e.stop_propagation();
                            move_card(card.get_untracked().column_id.clone(), 0);
                        }
                    >"Move to top"</button>
                    <button
                        type="button"
                        role="menuitem"
                        on:click=move |e: leptos::ev::MouseEvent| {
                            e.stop_propagation();
                            let column_id = card.get_untracked().column_id.clone();
                            let position = column_cards.0.with_untracked(|cards| cards.len() as i32);
                            move_card(column_id, position);
                        }
                    >"Move to bottom"</button>
                    <div class="card-context-menu-submenu-wrap">
                        <button
                            type="button"
                            role="menuitem"
                            aria-haspopup="menu"
                            aria-expanded=move || show_move_submenu.get().to_string()
                            on:click=move |_| show_move_submenu.update(|open| *open = !*open)
                        >"Move to column"<span aria-hidden="true">"›"</span></button>
                        <Show when=move || show_move_submenu.get()>
                            <div
                                class="card-context-menu card-context-menu-submenu"
                                class:card-context-menu-submenu-left=move || move_submenu_opens_left.get()
                                role="menu"
                            >
                                <For
                                    each=move || {
                                        let current_column_id = card.get().column_id;
                                        columns
                                            .get()
                                            .into_iter()
                                            .filter(|column| {
                                                column.get().id != current_column_id
                                            })
                                            .collect::<Vec<_>>()
                                    }
                                    key=|column| column.get_untracked().id.clone()
                                    children=move |column| {
                                        let target = column.get_untracked();
                                        let target_id = target.id;
                                        let target_name = target.name;
                                        view! {
                                            <button
                                                type="button"
                                                role="menuitem"
                                                on:click=move |e: leptos::ev::MouseEvent| {
                                                    e.stop_propagation();
                                                    move_card(target_id.clone(), 0);
                                                }
                                            >{target_name}</button>
                                        }
                                    }
                                />
                            </div>
                        </Show>
                    </div>
                    <button
                        type="button"
                        class="card-context-menu-danger"
                        role="menuitem"
                        on:click=move |e: leptos::ev::MouseEvent| {
                            e.stop_propagation();
                            context_menu_position.set(None);
                            show_move_submenu.set(false);
                            show_confirm.set(true);
                        }
                    >"Delete"</button>
                </div>
            </Show>
        </div>
    }
    .into_any()
}
