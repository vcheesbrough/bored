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

#[component]
fn App() -> impl IntoView {
    // Fetch runtime version/env from the backend once on mount.
    // Falls back to the compile-time version while the request is in flight or if it fails.
    let watermark = RwSignal::new(format!("v{}", env!("CARGO_PKG_VERSION")));

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(info) = crate::api::fetch_app_info().await {
                let label = if info.env == "production" {
                    format!("v{}", info.version)
                } else {
                    // Strip the type prefix (feat/, fix/, etc.) for brevity.
                    let branch = info.env.splitn(2, '/').last().unwrap_or(&info.env);
                    format!("v{} {}", info.version, branch)
                };
                watermark.set(label);
            }
        });
    });

    view! {
        <Router>
            <Routes fallback=|| view! { <p class="page loading-text">"Not found"</p> }>
                <Route path=path!("/") view=Home />
                <Route path=path!("/boards/:id") view=BoardView />
            </Routes>
        </Router>
        <div class="app-watermark">{move || watermark.get()}</div>
    }
}
