use leptos::prelude::*;
use leptos_router::hooks::use_params_map; // reads `:id` from the current URL

use crate::components::board_chooser::BoardChooser;
use crate::components::column::ColumnView;

#[component]
pub fn BoardView() -> impl IntoView {
    // `use_params_map()` returns a reactive map of URL parameters.
    // Calling `.with(|p| p.get("id"))` inside a closure reads the `:id` segment.
    let params = use_params_map();
    // `board_id` is a derived signal — a closure that re-evaluates whenever
    // `params` changes (i.e. when the user navigates to a different board).
    let board_id = move || params.with(|p| p.get("id").unwrap_or_default());

    // `RwSignal::new(...)` creates a reactive signal with an initial value.
    // Writing to it re-renders any component that reads it.
    let board_name = RwSignal::new(String::new());

    // `RwSignal<Vec<RwSignal<shared::Column>>>` — each column is itself a signal
    // so that renaming a column in the chooser instantly updates the column header
    // on the board without re-fetching everything.
    let columns: RwSignal<Vec<RwSignal<shared::Column>>> = RwSignal::new(Vec::new());
    let loading = RwSignal::new(true);

    // Update the browser tab title whenever `board_name` changes.
    Effect::new(move |_| {
        let name = board_name.get(); // reading `board_name` subscribes this effect to it
        if !name.is_empty() {
            document().set_title(&format!("{name} — bored"));
        }
    });

    // `on_cleanup` runs when this component is unmounted (e.g. navigating away).
    // We reset the tab title so it doesn't stay showing the old board name.
    on_cleanup(|| {
        document().set_title("bored");
    });

    // Fetch board data whenever the URL's `:id` segment changes.
    // `board_id()` is called inside the effect, so any change to `params` triggers a re-run.
    Effect::new(move |_| {
        let id = board_id();
        if id.is_empty() {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(board) = crate::api::fetch_board(&id).await {
                board_name.set(board.name);
            }
            match crate::api::fetch_columns(&id).await {
                Ok(fetched) => {
                    // Wrap each `Column` in an `RwSignal` so that individual column
                    // updates (rename) propagate reactively to the column header.
                    columns.set(fetched.into_iter().map(RwSignal::new).collect());
                }
                Err(e) => leptos::logging::error!("failed to fetch columns: {e}"),
            }
            loading.set(false);
        });
    });

    view! {
        <nav class="navbar">
            <a href="/" class="navbar-brand">"bored"</a>
            <span class="navbar-sep">"/"</span>
            // `BoardChooser` receives both signals so it can update the board name
            // displayed in the navbar and add/rename/delete columns on the board.
            <BoardChooser board_name=board_name columns=columns />
        </nav>

        <div class="page board-view">
            // `<Show>` renders its children only when `when` is true.
            // `fallback=|| ()` means "render nothing" when the condition is false.
            <Show when=move || loading.get() fallback=|| ()>
                <p class="loading-text">"Loading..."</p>
            </Show>

            <div class="columns-row">
                // `<For>` is Leptos's keyed list renderer. It diffs the list by `key`
                // so only changed items re-render, rather than recreating the whole list.
                // `key=|sig| sig.get_untracked().id.clone()` reads the ID without
                // subscribing — we only need it for diffing, not reactive tracking.
                <For
                    each=move || columns.get()
                    key=|sig| sig.get_untracked().id.clone()
                    children=move |sig| view! { <ColumnView column=sig /> }
                />
            </div>
        </div>
    }
}
