use leptos::prelude::*;
use leptos_router::hooks::use_navigate; // `use_navigate` returns a function you can call to redirect

use crate::components::user_badge::UserBadge;

#[component]
pub fn Home() -> impl IntoView {
    // `use_navigate()` returns a closure that triggers client-side navigation.
    // We clone it before moving into async closures because closures capture by move.
    let navigate = use_navigate();
    let navigate2 = navigate.clone(); // second clone for the create form handler

    // `RwSignal` is a reactive read-write signal. Reading it (`.get()`) subscribes
    // the current reactive context to updates; writing it (`.set(...)`) triggers re-renders.
    let new_board_name = RwSignal::new(String::new());
    // Starts as `false` (loading state). Flipped to `true` when we confirm there
    // are no boards, to show the "create your first board" form.
    let no_boards = RwSignal::new(false);

    // `Effect::new` runs the closure immediately and re-runs it whenever any
    // signal it reads changes. The `|_|` argument is the previous run's return
    // value — unused here.
    // On mount, fetch boards and redirect to the first one if any exist.
    Effect::new(move |_| {
        let nav = navigate.clone();
        // `spawn_local` schedules an async block on the WASM event loop.
        // WASM is single-threaded so there's no real concurrency — the async
        // block runs after the current synchronous code yields.
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::fetch_boards().await {
                Ok(boards) => {
                    // `.into_iter().next()` consumes the Vec and returns `Some(first)` or `None`.
                    if let Some(first) = boards.into_iter().next() {
                        // Navigate to the first board's URL. `Default::default()` uses
                        // the default navigation options (no replace, no state).
                        nav(&format!("/boards/{}", first.id), Default::default());
                    } else {
                        // No boards exist — show the empty-state form.
                        no_boards.set(true);
                    }
                }
                Err(e) => leptos::logging::error!("failed to fetch boards: {e}"),
            }
        });
    });

    // Form submit handler. `leptos::ev::SubmitEvent` is the typed DOM event.
    let on_create = move |ev: leptos::ev::SubmitEvent| {
        // Prevent the browser's default form submission (which would do a full
        // page navigation to `action="..."` — not what we want in a SPA).
        ev.prevent_default();
        // `get_untracked` reads the signal value without subscribing — we don't
        // want this closure to re-run reactively, just read the current value once.
        let name = new_board_name.get_untracked();
        if name.trim().is_empty() {
            return;
        }
        let nav = navigate2.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_board(name).await {
                Ok(board) => nav(&format!("/boards/{}", board.id), Default::default()),
                Err(e) => leptos::logging::error!("failed to create board: {e}"),
            }
        });
    };

    view! {
        <nav class="navbar">
            <a href="/" class="navbar-brand">"bored"</a>
            <UserBadge />
        </nav>
        <div class="page">
            // Show "Loading…" while the boards fetch is in flight.
            // `style:display` sets the CSS `display` property reactively.
            <p
                class="loading-text"
                style:display=move || if no_boards.get() { "none" } else { "block" }
            >
                "Loading…"
            </p>

            // Show the empty-state form only once we know there are no boards.
            <div
                class="empty-state"
                style:display=move || if no_boards.get() { "flex" } else { "none" }
            >
                <p class="page-title">"No boards yet"</p>
                <form class="create-form" on:submit=on_create>
                    // `prop:value` sets the DOM property (not the attribute) — this is
                    // how Leptos keeps a controlled input in sync with a signal.
                    // `on:input` fires on every keystroke and updates the signal.
                    <input
                        type="text"
                        placeholder="Board name"
                        prop:value=move || new_board_name.get()
                        on:input=move |ev| new_board_name.set(event_target_value(&ev))
                    />
                    <button type="submit">"Create board"</button>
                </form>
            </div>
        </div>
    }
}
