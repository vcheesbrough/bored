use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use wasm_bindgen::prelude::*;

use crate::components::board_chooser::BoardChooser;
use crate::components::column::ColumnView;
use crate::events::{BoardSseEvent, DragPayload};

#[component]
pub fn BoardView() -> impl IntoView {
    // `use_params_map()` returns a reactive map of URL parameters.
    let params = use_params_map();
    // Derived signal — re-evaluates whenever the user navigates to a different board.
    let board_id = move || params.with(|p| p.get("id").unwrap_or_default());

    let board_name = RwSignal::new(String::new());
    // Each column is wrapped in its own `RwSignal` so renaming one column
    // in the chooser propagates to the column header without touching the others.
    let columns: RwSignal<Vec<RwSignal<shared::Column>>> = RwSignal::new(Vec::new());
    let loading = RwSignal::new(true);

    // ── Context signals ────────────────────────────────────────────────────
    // These are provided so child components can access them without props.

    // Latest SSE event received from the server. Each child component reads
    // this in an effect and filters for events relevant to it.
    let sse_event: RwSignal<Option<BoardSseEvent>> = RwSignal::new(None);
    // Encodes what is currently being dragged (card or column). Set by
    // the element's dragstart handler; read by potential drop targets.
    let drag_payload: RwSignal<DragPayload> = RwSignal::new(DragPayload::None);

    // `provide_context` makes these signals available to any descendant
    // component via `use_context::<T>()` without threading them as props.
    provide_context(sse_event);
    provide_context(drag_payload);
    // Columns is also provided so ColumnView can trigger a bulk reorder
    // without needing to receive the full list as an additional prop.
    provide_context(columns);

    // ── Browser tab title ─────────────────────────────────────────────────
    Effect::new(move |_| {
        let name = board_name.get();
        if !name.is_empty() {
            document().set_title(&format!("{name} — bored"));
        }
    });

    on_cleanup(|| {
        document().set_title("bored");
    });

    // ── SSE connection ────────────────────────────────────────────────────
    // Opens a board-scoped SSE connection whenever the board ID changes.
    // Re-runs when the user navigates to a different board because `board_id()`
    // is a reactive read inside the effect — Leptos re-fires the effect and
    // the cleanup below closes the old EventSource before opening a new one.
    //
    // Passing `?board_id=` ensures the server only sends events for this board,
    // preventing data leaks between unrelated boards on a shared server.
    Effect::new(move |_| {
        let bid = board_id();
        if bid.is_empty() {
            return;
        }
        // `EventSource::new` opens the SSE connection. The browser handles
        // reconnection automatically on transient network failures.
        let url = format!("/api/events?board_id={bid}");
        let Ok(es) = web_sys::EventSource::new(&url) else {
            leptos::logging::error!("EventSource: failed to open {url}");
            return;
        };
        let es_for_cleanup = es.clone();

        // `Closure::new` wraps a Rust closure as a heap-allocated JS function.
        // The closure captures `sse_event` (a `Copy` signal handle — cheap).
        let cb =
            Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |msg: web_sys::MessageEvent| {
                // `data()` returns a `JsValue`; `.as_string()` converts it only
                // if the value really is a JS string (it always is for SSE).
                let Some(data) = msg.data().as_string() else {
                    return;
                };
                // Parse the JSON; keep-alive "ping" strings silently return None.
                let Some(event) = crate::events::parse_sse_event(&data) else {
                    return;
                };
                // Writing the signal notifies all reactive effects that read it.
                sse_event.set(Some(event));
            });
        // Attach the handler. `as_ref().unchecked_ref()` converts the Rust
        // closure reference to the `&Function` type expected by the Web API.
        es.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        // `forget()` transfers Rust's ownership of the closure to JS so it
        // isn't dropped when this effect body returns. The EventSource holds
        // a JS reference to the function; closing the source (below) is the
        // only clean-up needed.
        cb.forget();

        // Close the connection when this reactive scope is torn down (i.e.
        // when the user navigates away from any board view).
        on_cleanup(move || {
            es_for_cleanup.close();
        });
    });

    // ── Initial data fetch ────────────────────────────────────────────────
    // Re-runs whenever `board_id()` changes (user navigates to a different board).
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
    // Handles events that add, remove, rename, or reorder columns.
    // Card-level events are handled inside each `ColumnView` component.
    Effect::new(move |_| {
        let Some(event) = sse_event.get() else { return };
        let bid = board_id(); // read reactive dep so we recheck on navigation
        match event {
            BoardSseEvent::ColumnCreated { column } => {
                // Ignore events for other boards (SSE stream is global).
                if column.board_id == bid {
                    // Guard against double-insert: BoardChooser optimistically
                    // pushes the new column on API success; the SSE event that
                    // follows must not add it a second time.
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
                    // Find the existing signal and update it in-place so the
                    // column header re-renders without remounting the component.
                    columns.with_untracked(|cs| {
                        if let Some(sig) = cs.iter().find(|s| s.get_untracked().id == column.id) {
                            sig.set(column);
                        }
                    });
                }
            }
            BoardSseEvent::ColumnDeleted { column_id } => {
                // No board_id in the event; safe to retain-filter — if the column
                // isn't ours, it simply won't be found.
                columns.update(|cs| cs.retain(|s| s.get_untracked().id != column_id));
            }
            BoardSseEvent::ColumnsReordered { columns: reordered } => {
                // Only apply if the event belongs to the current board.
                if reordered
                    .first()
                    .map(|c| c.board_id == bid)
                    .unwrap_or(false)
                {
                    // Sort existing signals in-place rather than replacing them
                    // with new ones — this preserves each ColumnView's card state
                    // and avoids re-mounting components for unchanged columns.
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
            _ => {} // card events are handled in ColumnView
        }
    });

    view! {
        <nav class="navbar">
            <a href="/" class="navbar-brand">"bored"</a>
            <span class="navbar-sep">"/"</span>
            <BoardChooser board_name=board_name columns=columns />
        </nav>

        <div class="page board-view">
            <Show when=move || loading.get() fallback=|| ()>
                <p class="loading-text">"Loading..."</p>
            </Show>

            <div class="columns-row">
                <For
                    each=move || columns.get()
                    key=|sig| sig.get_untracked().id.clone()
                    children=move |sig| view! { <ColumnView column=sig /> }
                />
            </div>
        </div>
    }
}
