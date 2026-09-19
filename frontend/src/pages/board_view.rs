use leptos::leptos_dom::helpers::window_event_listener;
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use wasm_bindgen::prelude::*;

use crate::components::board_chooser::BoardChooser;
use crate::components::card::ExpandedCardId;
use crate::components::card_modal::CardModal;
use crate::components::column::ColumnView;
use crate::components::history_panel::{HistoryDrawer, HistoryIcon, HistoryPanel, HistoryScope};
use crate::components::search_suggestions::SearchSuggestions;
use crate::components::user_badge::UserBadge;
use crate::events::{BoardSseEvent, DragOverColId, DragPayload};
use crate::links::BoardLinkIndex;
use crate::recent::RecentPicks;
use crate::search::{
    BoardCardIndex, BoardSearchQuery, ColumnCardsEntry, HashSuggestion, active_hash_prefix,
    apply_hash_suggestion, hash_suggestions,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnGhostSide {
    Before,
    After,
}

fn column_ghost_side(
    columns: &[RwSignal<shared::Column>],
    drag_payload: &DragPayload,
    target_id: &str,
) -> Option<ColumnGhostSide> {
    let DragPayload::Column { column_id } = drag_payload else {
        return None;
    };
    let dragged_index = columns
        .iter()
        .position(|column| column.get_untracked().id == *column_id)?;
    let target_index = columns
        .iter()
        .position(|column| column.get_untracked().id == target_id)?;

    match dragged_index.cmp(&target_index) {
        std::cmp::Ordering::Less => Some(ColumnGhostSide::After),
        std::cmp::Ordering::Greater => Some(ColumnGhostSide::Before),
        std::cmp::Ordering::Equal => None,
    }
}

#[component]
fn ColumnGhost(
    columns: RwSignal<Vec<RwSignal<shared::Column>>>,
    drag_payload: RwSignal<DragPayload>,
    on_drop: Callback<()>,
) -> AnyView {
    // caps monomorphization at this boundary — see CardVersionActions doc comment in history_panel.rs
    view! {
        <div
            class="column-ghost"
            on:dragover=move |event: web_sys::DragEvent| {
                event.prevent_default();
                event.stop_propagation();
            }
            on:drop=move |event: web_sys::DragEvent| {
                event.prevent_default();
                event.stop_propagation();
                on_drop.run(());
            }
        >
            <span class="column-ghost-name">
                {move || {
                    if let DragPayload::Column { column_id: ref id } = drag_payload.get() {
                        columns
                            .get()
                            .iter()
                            .find(|column| column.get_untracked().id == *id)
                            .map(|column| column.get_untracked().name.clone())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    }
                }}
            </span>
        </div>
    }
    .into_any()
}

#[component]
pub fn BoardView() -> AnyView {
    // caps monomorphization at this boundary — see CardVersionActions doc comment in history_panel.rs
    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();

    // Board slug (name) from the route path parameter `:slug`.
    let board_slug = move || params.with(|p| p.get("slug").unwrap_or_default());
    // The board's internal ULID, resolved after the initial fetch. SSE
    // filtering and column comparisons use this rather than the slug because
    // all server-side events carry the ULID.
    let board_ulid: RwSignal<String> = RwSignal::new(String::new());

    // Optional card number from `?card=<number>` — drives the maximised overlay.
    // Card numbers are globally unique (single counter), so no board scope is needed.
    let maximised_card_number =
        move || query.with(|q| q.get("card").and_then(|v| v.parse::<u32>().ok()));

    let board_name = RwSignal::new(String::new());
    let columns: RwSignal<Vec<RwSignal<shared::Column>>> = RwSignal::new(Vec::new());
    // Owner for the per-column signals inside `columns`. They have to be created
    // under an owner that lives as long as the list, which rules out creating
    // them in an `Effect` — see `crate::columns::insert_absent`. A component
    // body always has an owner, but falling back to a standalone one keeps a
    // broken assumption from blanking the whole board: an owner held here for
    // the life of the view satisfies the same requirement.
    let view_owner = Owner::current().unwrap_or_default();
    let loading = RwSignal::new(true);
    let search_query = RwSignal::new(String::new());

    let watermark = RwSignal::new(format!("v{}", shared::app_version()));

    // ── Context signals ────────────────────────────────────────────────────
    let sse_event: RwSignal<Option<BoardSseEvent>> = RwSignal::new(None);
    let drag_payload: RwSignal<DragPayload> = RwSignal::new(DragPayload::None);
    let expanded_card_id: RwSignal<Option<String>> = RwSignal::new(None);
    let drag_over_col_id: RwSignal<Option<String>> = RwSignal::new(None);

    provide_context(sse_event);
    provide_context(drag_payload);
    provide_context(columns);
    provide_context(ExpandedCardId(expanded_card_id));
    provide_context(DragOverColId(drag_over_col_id));
    provide_context(BoardSearchQuery(search_query));

    // Handle on the navbar search `<input>`, used to focus it programmatically
    // (Enter-to-focus shortcut below and the clear button's refocus).
    let search_input_ref = NodeRef::<leptos::html::Input>::new();
    // Every column registers its card list here on mount; the `#` search popup
    // reads the aggregate to suggest tags and card numbers.
    let board_card_index: RwSignal<Vec<ColumnCardsEntry>> = RwSignal::new(Vec::new());
    let card_index = BoardCardIndex(board_card_index);
    provide_context(card_index);
    // Every link on the board, fetched once with the columns and then kept
    // current over SSE. Cards read it through the context so the inline card
    // and the modal show the same links.
    let board_links: RwSignal<Vec<shared::CardLink>> = RwSignal::new(Vec::new());
    // Flipped true when the initial link fetch resolves; see `BoardLinkIndex`
    // for why an empty `board_links` is not a usable stand-in for "loaded".
    let board_links_loaded = RwSignal::new(false);
    let link_index = BoardLinkIndex {
        links: board_links,
        loaded: board_links_loaded,
    };
    provide_context(link_index);
    // What the user has picked from this board's combo boxes before, so the
    // link picker and the `#` popup can lead with it. Seeded from
    // `localStorage` whenever the board's ULID changes — including the blank
    // it passes through on navigation, which clears the previous board's
    // picks.
    let recent = RecentPicks::new(board_ulid);
    Effect::new(move |_| recent.load());
    provide_context(recent);

    // ── `#` search suggestions ─────────────────────────────────────────────
    // Typing `#` opens a helper listing the tags and card numbers that could
    // follow it. Purely client-side: the board's cards are already in memory.
    let suggestions = Signal::derive(move || match active_hash_prefix(&search_query.get()) {
        Some(prefix) => hash_suggestions(
            prefix,
            &card_index.all_tags(),
            &card_index.all_cards(),
            &recent.tags.get(),
            &recent.cards.get(),
        ),
        None => Vec::new(),
    });
    // Index of the arrow-key-highlighted row; `None` means nothing is picked and
    // Enter falls through to its normal "focus the search box" behaviour.
    let active_suggestion: RwSignal<Option<usize>> = RwSignal::new(None);
    // Set when the user dismisses the popup with Escape, so it stays shut until
    // the token changes rather than reopening on the next keystroke.
    let suggestions_dismissed = RwSignal::new(false);
    let suggestions_open =
        Signal::derive(move || !suggestions_dismissed.get() && !suggestions.get().is_empty());

    let accept_suggestion = move |suggestion: &HashSuggestion| {
        // Remember the pick before the query changes: the popup's next opening
        // should offer this row first, whatever the search goes on to match.
        match suggestion {
            HashSuggestion::Tag(tag) => recent.record_tag(tag),
            HashSuggestion::Card { number, .. } => {
                // The row carries a number, not an ID, but the history is
                // keyed by ID so it survives a card being renumbered — look
                // the card up on the board to record it.
                if let Some(card) = card_index
                    .all_cards()
                    .into_iter()
                    .find(|card| card.number == *number)
                {
                    recent.record_card(&card.id);
                }
            }
        }
        search_query.update(|q| *q = apply_hash_suggestion(q, &suggestion.value()));
        active_suggestion.set(None);
        if let Some(input) = search_input_ref.get_untracked() {
            let _ = input.focus();
        }
    };
    // Callback form for `SearchSuggestions`, which needs to accept a row by
    // value from its own click handler.
    let accept_suggestion_cb =
        Callback::new(move |suggestion: HashSuggestion| accept_suggestion(&suggestion));

    // Enter on the "bare" board — when nothing interactive is focused — jumps
    // straight into the search input so the whole search flow is mouse-free.
    // Enter is a normal activation key, so we must *not* steal it from focused
    // controls, text fields, or open overlays (a card modal, for instance,
    // parks focus on its container `<div>`). Rather than enumerate every
    // interactive element, we act only when focus rests on the page `<body>`,
    // which is exactly the "nothing is focused" state. Registered once for the
    // life of the view and torn down in `on_cleanup`.
    //
    // Some overlays (the board chooser, a card's right-click context menu) sit
    // above the board without moving DOM focus onto themselves, so the
    // `<body>` check alone would let Enter reach through them. They each mount
    // a full-viewport backdrop element while open, so checking for a *visible*
    // backdrop closes that gap without plumbing each overlay's local
    // open-state signal into this component. Presence alone isn't enough:
    // `.chooser-backdrop` stays mounted at all times and is toggled purely via
    // inline `display`, so each selector is checked independently for that
    // rather than matched as a single combined query.
    fn backdrop_visible(selector: &str) -> bool {
        use wasm_bindgen::JsCast;
        document()
            .query_selector(selector)
            .ok()
            .flatten()
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
            .is_some_and(|el| {
                el.style().get_property_value("display").unwrap_or_default() != "none"
            })
    }

    let enter_focus_listener = StoredValue::new(Some(window_event_listener(
        leptos::ev::keydown,
        move |ev| {
            if ev.key() != "Enter" {
                return;
            }
            let on_bare_board = match document().active_element() {
                None => true,
                Some(active) => active.tag_name().eq_ignore_ascii_case("body"),
            };
            if !on_bare_board {
                return;
            }
            let overlay_open = backdrop_visible(".chooser-backdrop")
                || backdrop_visible(".card-context-menu-backdrop");
            if overlay_open {
                return;
            }
            // Prevent default so the keypress doesn't also trigger any latent
            // form submission before we move focus into the search box.
            ev.prevent_default();
            if let Some(input) = search_input_ref.get_untracked() {
                let _ = input.focus();
            }
        },
    )));
    on_cleanup(move || {
        enter_focus_listener.update_value(|listener| {
            if let Some(listener) = listener.take() {
                listener.remove();
            }
        });
    });

    let on_column_drop = Callback::new(move |target_id: String| {
        let DragPayload::Column {
            column_id: dragged_id,
        } = drag_payload.get_untracked()
        else {
            return;
        };

        if dragged_id == target_id {
            drag_payload.set(DragPayload::None);
            drag_over_col_id.set(None);
            return;
        }

        let mut reordered = false;
        columns.update(|current| {
            let Some(dragged_index) = current
                .iter()
                .position(|column| column.get_untracked().id == dragged_id)
            else {
                return;
            };
            let Some(target_index) = current
                .iter()
                .position(|column| column.get_untracked().id == target_id)
            else {
                return;
            };

            let dragged = current.remove(dragged_index);
            // `target_index` is captured before removal. Moving right shifts
            // the target left, so its old index inserts after it; moving left
            // inserts before it.
            current.insert(target_index, dragged);
            reordered = true;
        });

        if reordered {
            let order = columns.with_untracked(|current| {
                current
                    .iter()
                    .map(|column| column.get_untracked().id.clone())
                    .collect()
            });
            let slug = board_name.get_untracked();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(err) = crate::api::reorder_columns(&slug, order).await {
                    leptos::logging::error!("reorder_columns failed: {err}");
                }
            });
        }

        drag_payload.set(DragPayload::None);
        drag_over_col_id.set(None);
    });

    let history_scope = RwSignal::new(None::<HistoryScope>);
    provide_context(HistoryDrawer(history_scope));

    // ── Maximised card overlay ─────────────────────────────────────────────
    let maximised_card: RwSignal<Option<shared::Card>> = RwSignal::new(None);

    Effect::new(move |_| match maximised_card_number() {
        Some(num) => {
            wasm_bindgen_futures::spawn_local(async move {
                match crate::api::fetch_card_by_number(num).await {
                    Ok(card) => maximised_card.set(Some(card)),
                    Err(e) => leptos::logging::error!("fetch maximised card failed: {e}"),
                }
            });
        }
        None => {
            maximised_card.set(None);
        }
    });

    // Navigate back to the plain board URL (by slug) when the modal closes.
    let on_modal_close = Callback::new(move |_: ()| {
        navigate(&format!("/boards/{}", board_slug()), Default::default());
    });

    let on_modal_updated = Callback::new(move |_: shared::Card| {});

    // Deleting from the maximised modal has no column component above it to
    // apply the removal — the modal is rendered by `BoardView`, outside every
    // `ColumnView` — so this callback has to do locally what
    // `ColumnView::on_card_delete` does for an inline delete. It used to be a
    // no-op, which left the card *and* its links on the board for any tab whose
    // SSE stream was down, reconnecting, or lagged out of the broadcast channel.
    let on_modal_delete = Callback::new(move |card_id: String| {
        // The card lives in exactly one column's list, but which one is not
        // known here: the modal is reachable by deep link, so the board may
        // never have been told. `BoardCardIndex` holds every column's own card
        // signal, so retaining across all of them removes it from whichever
        // holds it and leaves the others untouched.
        //
        // Snapshot the entries with `get_untracked` *before* writing to any of
        // them. Iterating inside `with_untracked` would hold a borrow of
        // `board_card_index` across each `cards.update`, and an update runs the
        // subscribed effects synchronously — those read the index again and hit
        // the still-live borrow, aborting the tab with "RefCell already
        // borrowed". The entries are `Copy` handles, so the snapshot is cheap
        // and the writes below still land on the real column signals.
        let columns = board_card_index.get_untracked();
        for (_, cards) in columns {
            // Only the column that actually holds the card is written. An
            // `update` notifies its subscribers whether or not `retain` removed
            // anything, so writing every column would re-run each one's filter
            // — and every `sig.get()` inside it — for a card they never held.
            // That is wasted work, and one more chance to notify a component
            // that is mid-unmount, which is the whole family of bug this
            // iteration is about. `ColumnView`'s own SSE `CardDeleted` handler
            // guards the same way; a card is in exactly one column, so stop at
            // the first hit.
            let owned =
                cards.with_untracked(|cs| cs.iter().any(|s| s.get_untracked().id == card_id));
            if owned {
                cards.update(|cs| cs.retain(|s| s.get_untracked().id != card_id));
                break;
            }
        }
        link_index.remove_touching(&card_id);
    });

    // ── Watermark fetch ────────────────────────────────────────────────────
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(info) = crate::api::fetch_app_info().await {
                let label = if info.env == "production" {
                    format!("v{}", info.version)
                } else {
                    let branch = info.env.splitn(2, '/').last().unwrap_or(&info.env);
                    format!("v{} {}", info.version, branch)
                };
                watermark.set(label);
            }
        });
    });

    // ── Browser tab title ─────────────────────────────────────────────────
    Effect::new(move |_| {
        let name = board_name.get();
        if !name.is_empty() {
            document().set_title(&format!("{name} — bored"));
        }
    });
    on_cleanup(|| document().set_title("bored"));

    // ── SSE connection ────────────────────────────────────────────────────
    // The EventSource uses the board ULID (not the slug) for filtering because
    // all server-side events carry the ULID as their `board_id`. The ULID is
    // set after the first successful board fetch, so the effect re-runs once
    // the async fetch completes.
    Effect::new(move |_| {
        let ulid = board_ulid.get();
        if ulid.is_empty() {
            return;
        }
        let url = format!("/api/events?board_id={ulid}");
        let Ok(es) = web_sys::EventSource::new(&url) else {
            leptos::logging::error!("EventSource: failed to open {url}");
            return;
        };
        let es_for_cleanup = es.clone();

        let cb =
            Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |msg: web_sys::MessageEvent| {
                let Some(data) = msg.data().as_string() else {
                    return;
                };
                let Some(event) = crate::events::parse_sse_event(&data) else {
                    return;
                };
                sse_event.set(Some(event));
            });
        es.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        cb.forget();

        // Deployment-triggered reload via SSE reconnect.
        let initial_version: std::rc::Rc<std::cell::RefCell<Option<String>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let had_error: std::rc::Rc<std::cell::Cell<bool>> =
            std::rc::Rc::new(std::cell::Cell::new(false));

        let initial_version_open = initial_version.clone();
        let had_error_open = had_error.clone();
        let onopen_cb = Closure::<dyn Fn(web_sys::Event)>::new(move |_: web_sys::Event| {
            let is_reconnect = had_error_open.get();
            had_error_open.set(false);
            let iv = initial_version_open.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(info) = crate::api::fetch_app_info().await {
                    if is_reconnect {
                        let stored = iv.borrow().clone();
                        match stored {
                            None => leptos::logging::warn!(
                                "auto-reload: baseline version unknown; skipping reload check"
                            ),
                            Some(baseline) if baseline != info.version => {
                                let _ = leptos::prelude::window().location().reload();
                            }
                            Some(_) => {}
                        }
                    } else {
                        *iv.borrow_mut() = Some(info.version);
                    }
                }
            });
        });
        es.set_onopen(Some(onopen_cb.as_ref().unchecked_ref()));
        onopen_cb.forget();

        let had_error_err = had_error.clone();
        let onerror_cb = Closure::<dyn Fn(web_sys::Event)>::new(move |_: web_sys::Event| {
            had_error_err.set(true);
        });
        es.set_onerror(Some(onerror_cb.as_ref().unchecked_ref()));
        onerror_cb.forget();

        on_cleanup(move || es_for_cleanup.close());
    });

    // ── Initial data fetch ────────────────────────────────────────────────
    Effect::new(move |_| {
        let slug = board_slug();
        if slug.is_empty() {
            board_ulid.set(String::new());
            board_name.set(String::new());
            columns.set(Vec::new());
            return;
        }
        // Clear ULID immediately so the SSE effect closes any stale connection.
        board_ulid.set(String::new());
        // Reflect the URL immediately and clear stale cards/columns while the
        // async board load resolves. Without this, a direct navigation from one
        // board URL to another can briefly show the previous board, and a slow
        // older request can overwrite the newer route.
        board_name.set(slug.clone());
        columns.set(Vec::new());
        board_links.set(Vec::new());
        board_links_loaded.set(false);
        loading.set(true);
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(board) = crate::api::fetch_board(&slug).await
                && board_slug() == slug
            {
                board_name.set(board.name);
                // Set the ULID after fetch — triggers the SSE effect to connect.
                board_ulid.set(board.id);
            }
            match crate::api::fetch_columns(&slug).await {
                Ok(fetched) => {
                    if board_slug() == slug {
                        columns.set(fetched.into_iter().map(RwSignal::new).collect());
                    }
                }
                Err(e) => leptos::logging::error!("failed to fetch columns: {e}"),
            }
            match crate::api::fetch_board_links(&slug).await {
                Ok(fetched) => {
                    if board_slug() == slug {
                        board_links.set(fetched);
                        board_links_loaded.set(true);
                    }
                }
                // Left false on failure: the index is genuinely unknown, and a
                // sort computed from no edges would silently do nothing.
                Err(e) => leptos::logging::error!("failed to fetch links: {e}"),
            }
            if board_slug() == slug {
                loading.set(false);
            }
        });
    });

    // ── Column-level SSE events ───────────────────────────────────────────
    // Use `board_ulid.get_untracked()` for the board-ID comparison so this
    // Effect is only reactive on `sse_event`, not on `board_ulid`.
    let sse_column_owner = view_owner.clone();
    Effect::new(move |_| {
        let Some(event) = sse_event.get() else { return };
        let ulid = board_ulid.get_untracked();
        match event {
            BoardSseEvent::ColumnCreated { column } => {
                if column.board_id == ulid {
                    crate::columns::insert_absent(&sse_column_owner, columns, column);
                }
            }
            BoardSseEvent::ColumnUpdated { column } => {
                if column.board_id == ulid {
                    columns.with_untracked(|cs| {
                        if let Some(sig) = cs.iter().find(|s| s.get_untracked().id == column.id) {
                            sig.set(column);
                        }
                    });
                }
            }
            BoardSseEvent::ColumnDeleted { column_id } => {
                columns.update(|cs| cs.retain(|s| s.get_untracked().id != column_id));
            }
            BoardSseEvent::ColumnsReordered { columns: reordered } => {
                if reordered
                    .first()
                    .map(|c| c.board_id == ulid)
                    .unwrap_or(false)
                {
                    columns.update(|cs| {
                        cs.sort_by_key(|sig| {
                            let id = sig.get_untracked().id.clone();
                            reordered
                                .iter()
                                .position(|c| c.id == id)
                                .unwrap_or(usize::MAX)
                        });
                    });
                }
            }
            // Link events are already board-scoped by the SSE subscription, and
            // a link created locally is deduplicated by id.
            BoardSseEvent::CardLinkCreated { link } => link_index.insert_absent(link),
            BoardSseEvent::CardLinkUpdated { link } => link_index.replace(link),
            BoardSseEvent::CardLinkDeleted { link_id } => link_index.remove(&link_id),
            // The server does remove a card's links first and broadcast each
            // removal on its own, so in a healthy stream those `CardLinkDeleted`
            // events have already pruned the index by the time this arrives and
            // there is nothing left to retain out. This arm is for the tab that
            // *missed* them — a receiver that lags out of the backend's 128-slot
            // broadcast channel has events dropped silently, and the one
            // announcing the card can outlive the ones announcing its links.
            // Pruning by card id is always sound (a link cannot outlive either
            // of its cards) and idempotent, so it costs nothing in the healthy
            // case and heals the lagged one. The card itself is removed by the
            // owning `ColumnView`'s own handler.
            BoardSseEvent::CardDeleted { card_id } => link_index.remove_touching(&card_id),
            _ => {}
        }
    });

    view! {
        <nav class="navbar">
            <a href="/" class="navbar-brand">"bored"</a>
            <span class="navbar-sep">"/"</span>
            <BoardChooser board_name=board_name columns=columns column_owner=view_owner.clone() />
            <div class="navbar-search">
                <input
                    node_ref=search_input_ref
                    class="navbar-search-input"
                    type="text"
                    placeholder="Search"
                    role="combobox"
                    aria-autocomplete="list"
                    aria-expanded=move || suggestions_open.get().to_string()
                    aria-controls="search-hash-suggestions"
                    prop:value=move || search_query.get()
                    on:input=move |ev| {
                        search_query.set(event_target_value(&ev));
                        // A new keystroke is a new token: re-open the popup and
                        // drop any highlight from the previous one.
                        suggestions_dismissed.set(false);
                        active_suggestion.set(None);
                    }
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        match ev.key().as_str() {
                            // Esc closes an open popup first; only once it is shut
                            // does Esc clear the query, then blur the empty box.
                            "Escape" => {
                                if suggestions_open.get_untracked() {
                                    ev.stop_propagation();
                                    suggestions_dismissed.set(true);
                                    active_suggestion.set(None);
                                } else if search_query.get_untracked().is_empty() {
                                    if let Some(input) = search_input_ref.get_untracked() {
                                        let _ = input.blur();
                                    }
                                } else {
                                    search_query.set(String::new());
                                }
                            }
                            "ArrowDown" | "ArrowUp" => {
                                let len = suggestions.get_untracked().len();
                                if !suggestions_open.get_untracked() || len == 0 {
                                    return;
                                }
                                ev.prevent_default();
                                let down = ev.key() == "ArrowDown";
                                active_suggestion.update(|i| {
                                    *i = Some(match (*i, down) {
                                        (None, true) => 0,
                                        (None, false) => len - 1,
                                        (Some(current), true) => (current + 1) % len,
                                        (Some(0), false) => len - 1,
                                        (Some(current), false) => current - 1,
                                    });
                                });
                            }
                            "Enter" | "Tab" => {
                                // Only intercept when a row is actually picked, so
                                // Tab still moves focus and Enter still does nothing
                                // special in the plain search case.
                                let Some(picked) = active_suggestion
                                    .get_untracked()
                                    .and_then(|i| suggestions.get_untracked().get(i).cloned())
                                else {
                                    return;
                                };
                                ev.prevent_default();
                                ev.stop_propagation();
                                accept_suggestion(&picked);
                            }
                            _ => {}
                        }
                    }
                />

                <SearchSuggestions
                    suggestions=suggestions
                    open=suggestions_open
                    active=active_suggestion
                    on_accept=accept_suggestion_cb
                />
                <Show when=move || !search_query.get().is_empty() fallback=|| ()>
                    <button
                        class="navbar-search-clear"
                        type="button"
                        aria-label="Clear search"
                        title="Clear search"
                        on:click=move |_| {
                            search_query.set(String::new());
                            suggestions_dismissed.set(false);
                            active_suggestion.set(None);
                            // Keep the caret in the box so typing can continue.
                            if let Some(input) = search_input_ref.get_untracked() {
                                let _ = input.focus();
                            }
                        }
                    >"×"</button>
                </Show>
            </div>
            <button
                class="card-toolbar-btn navbar-history-btn"
                type="button"
                title="Board history"
                on:click=move |_| history_scope.set(Some(HistoryScope::Board))
            ><HistoryIcon /></button>
            <span class="navbar-watermark">{move || watermark.get()}</span>
            <UserBadge />
        </nav>

        <div class="page board-view">
            <Show when=move || loading.get() fallback=|| ()>
                <p class="loading-text">"Loading…"</p>
            </Show>

            <div class="columns-row">
                <For
                    each=move || columns.get()
                    key=|sig| sig.get_untracked().id.clone()
                    children=move |sig| {
                        let col_id = sig.get_untracked().id.clone();
                        let col_id_before = col_id.clone();
                        let col_id_after = col_id.clone();
                        let col_id_drop = col_id.clone();
                        let ghost_drop = Callback::new(move |_: ()| {
                            on_column_drop.run(col_id_drop.clone());
                        });
                        view! {
                            <Show when=move || {
                                drag_over_col_id.get().as_deref() == Some(col_id.as_str())
                                    && column_ghost_side(
                                        &columns.get(),
                                        &drag_payload.get(),
                                        &col_id_before,
                                    ) == Some(ColumnGhostSide::Before)
                            }>
                                <ColumnGhost
                                    columns=columns
                                    drag_payload=drag_payload
                                    on_drop=ghost_drop
                                />
                            </Show>
                            <ColumnView column=sig on_column_drop=on_column_drop />
                            <Show when=move || {
                                drag_over_col_id.get().as_deref() == Some(col_id_after.as_str())
                                    && column_ghost_side(
                                        &columns.get(),
                                        &drag_payload.get(),
                                        &col_id_after,
                                    ) == Some(ColumnGhostSide::After)
                            }>
                                <ColumnGhost
                                    columns=columns
                                    drag_payload=drag_payload
                                    on_drop=ghost_drop
                                />
                            </Show>
                        }
                    }
                />
            </div>
        </div>

        <CardModal
            card=maximised_card
            on_updated=on_modal_updated
            on_delete=on_modal_delete
            on_close=on_modal_close
        />

        <HistoryPanel board_slug=board_name board_ulid=board_ulid sse_event=sse_event />
    }
    .into_any()
}
