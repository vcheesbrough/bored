// Declare the three submodules that make up the frontend crate.
// Rust looks for each in `src/<name>/mod.rs` or `src/<name>.rs`.
mod api;        // HTTP fetch wrappers (one function per backend endpoint)
mod components; // Reusable UI components (column, card, modals, chooser)
mod pages;      // Top-level page components (Home, BoardView)

use leptos::prelude::*;
use leptos_router::{
    components::{Route, Router, Routes},
    path, // `path!` macro — converts a string literal into a typed route path at compile time
};
use pages::{board_view::BoardView, home::Home};

// `main` is the WASM entry point. `mount_to_body` renders the `App` component
// into `document.body`, replacing the loading placeholder in `index.html`.
fn main() {
    mount_to_body(App);
}

// `#[component]` is a Leptos macro that marks this function as a reactive component.
// Components return `impl IntoView` — any value that can be turned into DOM nodes.
#[component]
fn App() -> impl IntoView {
    view! {
        // `<Router>` provides the client-side routing context. All `<Route>` and
        // navigation hooks (like `use_navigate`) must be descendants of `<Router>`.
        <Router>
            // `<Routes>` is where route matching happens. `fallback` renders
            // when no route matches — acts like a 404 page.
            <Routes fallback=|| view! { <p class="page loading-text">"Not found"</p> }>
                // `path!(...)` creates a typed path matcher. `"/"` matches only the root.
                <Route path=path!("/") view=Home />
                // `:id` is a named URL parameter — readable via `use_params_map()` inside `BoardView`.
                <Route path=path!("/boards/:id") view=BoardView />
            </Routes>
        </Router>

        // `env!("CARGO_PKG_VERSION")` is expanded at compile time to the version
        // string from `Cargo.toml` (e.g. "0.5"). It's rendered as a barely-visible
        // watermark in the bottom-left corner via CSS.
        <div class="app-watermark">"v" {env!("CARGO_PKG_VERSION")}</div>
    }
}
