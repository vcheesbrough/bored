use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use wasm_bindgen::prelude::*;

use crate::components::board_chooser::BoardChooser;
use crate::components::card::ExpandedCardId;
use crate::components::card_modal::CardModal;
use crate::components::column::ColumnView;
use crate::components::user_badge::UserBadge;
use crate::events::{BoardSseEvent, DragOverColId, DragPayload};

#[component]
pub fn BoardView() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();

    // Board ID from the route path parameter.
    let board_id = move || params.with(|p| p.get("id").unwrap_or_default());
    // Optional card ID from the `?card=` query parameter; drives the maximised overlay.
    let maximised_card_id = move || query.with(|q| q.get("card"));

    let board_name = RwSignal::new(String::new());
    let columns: RwSignal<Vec<RwSignal<shared::Column>>> = RwSignal::new(Vec::new());
    let loading = RwSignal::new(true);

    // Watermark: version + environment fetched once from the backend.
    let watermark = RwSignal::new(format!("v{}", env!("CARGO_PKG_VERSION")));

    // ── Context signals ────────────────────────────────────────────────────
    let sse_event: RwSignal<Option<BoardSseEvent>> = RwSignal::new(None);
    let drag_payload: RwSignal<DragPayload> = RwSignal::new(DragPayload::None);
    // Tracks which card (if any) is currently expanded or editing across the
    // whole board so that only one card can be open at a time.
    let expanded_card_id: RwSignal<Option<String>> = RwSignal::new(None);
    // Tracks which column ID (if any) a dragged column is currently hovering
    // over, driving the narrow ghost placeholder shown before that column.
    let drag_over_col_id: RwSignal<Option<String>> = RwSignal::new(None);

    provide_context(sse_event);
    provide_context(drag_payload);
    provide_context(columns);
    provide_context(ExpandedCardId(expanded_card_id));
    // Wrapped in DragOverColId so use_context in ColumnView retrieves the
    // correct signal even after ColumnView provides its own RwSignal<Option<String>>.
    provide_context(DragOverColId(drag_over_col_id));

    // ── Maximised card overlay ─────────────────────────────────────────────
    // When the URL has `?card=<id>`, we fetch that card and show it in a
    // full-screen `CardModal`.  Navigating away from the URL (or closing the
    // modal) removes the query parameter without remounting the board.
    let maximised_card: RwSignal<Option<shared::Card>> = RwSignal::new(None);

    Effect::new(move |_| {
        match maximised_card_id() {
            Some(card_id) => {
                wasm_bindgen_futures::spawn_local(async move {
                    match crate::api::fetch_card(&card_id).await {
                        Ok(card) => maximised_card.set(Some(card)),
                        Err(e) => leptos::logging::error!("fetch maximised card failed: {e}"),
                    }
                });
            }
            None => {
                // Clear any stale card when the query param is removed.
                maximised_card.set(None);
            }
        }
    });

    // Navigate back to the plain board URL when the modal signals it should close.
    let on_modal_close = Callback::new(move |_: ()| {
        navigate(&format!("/boards/{}", board_id()), Default::default());
    });

    let on_modal_updated = Callback::new(move |_: shared::Card| {
        // SSE `CardUpdated` event keeps the column list in sync; no extra action needed.
    });

    let on_modal_delete = Callback::new(move |_: String| {
        // SSE `CardDeleted` removes the card from all columns.
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
    Effect::new(move |_| {
        let bid = board_id();
        if bid.is_empty() {
            return;
        }
        let url = format!("/api/events?board_id={bid}");
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

        // ── Deployment-triggered reload via SSE reconnect ─────────────────
        // Store the version seen on first connection. On every reconnect
        // (onerror → onopen) we re-fetch /api/info; if the version has
        // changed the server was redeployed and we hard-reload to pick up
        // new frontend assets.
        let initial_version: std::rc::Rc<std::cell::RefCell<Option<String>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let had_error: std::rc::Rc<std::cell::Cell<bool>> =
            std::rc::Rc::new(std::cell::Cell::new(false));

        // onopen: on first open capture the version; on reconnect after an
        // error check whether the version changed.
        let initial_version_open = initial_version.clone();
        let had_error_open = had_error.clone();
        let onopen_cb = Closure::<dyn Fn(web_sys::Event)>::new(move |_: web_sys::Event| {
            let is_reconnect = had_error_open.get();
            had_error_open.set(false);
            let iv = initial_version_open.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(info) = crate::api::fetch_app_info().await {
                    if is_reconnect {
                        // Reconnected after a drop — reload if version changed.
                        let stored = iv.borrow().clone();
                        match stored {
                            // Baseline was never stored (initial fetch failed); log
                            // so the silent miss is visible in production diagnostics.
                            None => leptos::logging::warn!(
                                "auto-reload: baseline version unknown (initial fetch failed); \
                                 skipping reload check"
                            ),
                            Some(baseline) if baseline != info.version => {
                                let _ = leptos::prelude::window().location().reload();
                            }
                            Some(_) => {}
                        }
                    } else {
                        // First open — record baseline version.
                        *iv.borrow_mut() = Some(info.version);
                    }
                }
            });
        });
        es.set_onopen(Some(onopen_cb.as_ref().unchecked_ref()));
        onopen_cb.forget();

        // onerror: mark that the connection dropped so the next onopen knows
        // it is a reconnect rather than the initial open.
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
        let id = board_id();
        if id.is_empty() {
            return;
        }
        loading.set(true);
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(board) = crate::api::fetch_board(&id).await {
                board_name.set(board.name);
            }
            match crate::api::fetch_columns(&id).await {
                Ok(fetched) => {
                    columns.set(fetched.into_iter().map(RwSignal::new).collect());
                }
                Err(e) => leptos::logging::error!("failed to fetch columns: {e}"),
            }
            loading.set(false);
        });
    });

    // ── Column-level SSE events ───────────────────────────────────────────
    Effect::new(move |_| {
        let Some(event) = sse_event.get() else { return };
        let bid = board_id();
        match event {
            BoardSseEvent::ColumnCreated { column } => {
                if column.board_id == bid {
                    columns.update(|cs| {
                        if cs.iter().any(|s| s.get_untracked().id == column.id) {
                            return;
                        }
                        cs.push(RwSignal::new(column));
                    });
                }
            }
            BoardSseEvent::ColumnUpdated { column } => {
                if column.board_id == bid {
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
                    .map(|c| c.board_id == bid)
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
            _ => {}
        }
    });

    view! {
        <nav class="navbar">
            <a href="/" class="navbar-brand">"bored"</a>
            <span class="navbar-sep">"/"</span>
            <BoardChooser board_name=board_name columns=columns />
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
                        view! {
                            // Ghost placeholder shown before the hovered column while
                            // a column drag is in progress.  Shows the dragged column's
                            // name so the user knows what they are repositioning.
                            <Show when=move || {
                                drag_over_col_id.get().as_deref() == Some(col_id.as_str())
                                    && matches!(drag_payload.get(), DragPayload::Column { .. })
                            }>
                                <div class="column-ghost">
                                    <span class="column-ghost-name">
                                        {move || {
                                            if let DragPayload::Column { column_id: ref id } =
                                                drag_payload.get()
                                            {
                                                columns
                                                    .get()
                                                    .iter()
                                                    .find(|s| s.get_untracked().id == *id)
                                                    .map(|s| s.get_untracked().name.clone())
                                                    .unwrap_or_default()
                                            } else {
                                                String::new()
                                            }
                                        }}
                                    </span>
                                </div>
                            </Show>
                            <ColumnView column=sig />
                        }
                    }
                />
            </div>
        </div>

        // Maximised card overlay — shown when `?card=<id>` is in the URL.
        <CardModal
            card=maximised_card
            on_updated=on_modal_updated
            on_delete=on_modal_delete
            on_close=on_modal_close
        />
    }
}
