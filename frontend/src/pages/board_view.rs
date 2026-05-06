use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use wasm_bindgen::prelude::*;

use crate::components::board_chooser::BoardChooser;
use crate::components::card::ExpandedCardId;
use crate::components::card_modal::CardModal;
use crate::components::column::ColumnView;
use crate::components::history_panel::{HistoryDrawer, HistoryPanel, HistoryScope};
use crate::components::user_badge::UserBadge;
use crate::events::{BoardSseEvent, DragOverColId, DragPayload};

#[component]
pub fn BoardView() -> impl IntoView {
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
    let loading = RwSignal::new(true);

    let watermark = RwSignal::new(format!("v{}", env!("CARGO_PKG_VERSION")));

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
    let on_modal_delete = Callback::new(move |_: String| {});

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
            return;
        }
        // Clear ULID immediately so the SSE effect closes any stale connection.
        board_ulid.set(String::new());
        loading.set(true);
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(board) = crate::api::fetch_board(&slug).await {
                board_name.set(board.name);
                // Set the ULID after fetch — triggers the SSE effect to connect.
                board_ulid.set(board.id);
            }
            match crate::api::fetch_columns(&slug).await {
                Ok(fetched) => {
                    columns.set(fetched.into_iter().map(RwSignal::new).collect());
                }
                Err(e) => leptos::logging::error!("failed to fetch columns: {e}"),
            }
            loading.set(false);
        });
    });

    // ── Column-level SSE events ───────────────────────────────────────────
    // Use `board_ulid.get_untracked()` for the board-ID comparison so this
    // Effect is only reactive on `sse_event`, not on `board_ulid`.
    Effect::new(move |_| {
        let Some(event) = sse_event.get() else { return };
        let ulid = board_ulid.get_untracked();
        match event {
            BoardSseEvent::ColumnCreated { column } => {
                if column.board_id == ulid {
                    columns.update(|cs| {
                        if cs.iter().any(|s| s.get_untracked().id == column.id) {
                            return;
                        }
                        cs.push(RwSignal::new(column));
                    });
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
            _ => {}
        }
    });

    view! {
        <nav class="navbar">
            <a href="/" class="navbar-brand">"bored"</a>
            <span class="navbar-sep">"/"</span>
            <BoardChooser board_name=board_name columns=columns />
            <button
                class="card-toolbar-btn navbar-history-btn"
                type="button"
                title="Board history"
                on:click=move |_| history_scope.set(Some(HistoryScope::Board))
            >"🕘"</button>
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

        <CardModal
            card=maximised_card
            on_updated=on_modal_updated
            on_delete=on_modal_delete
            on_close=on_modal_close
        />

        <HistoryPanel board_slug=board_name board_ulid=board_ulid sse_event=sse_event />
    }
}
