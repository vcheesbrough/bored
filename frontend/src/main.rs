mod api;
pub(crate) mod audit_edit_session;
mod columns;
mod components;
mod events;
mod links;
mod pages;
mod search;

use leptos::prelude::*;
use leptos_router::{
    components::{Route, Router, Routes},
    path,
};
use pages::{board_view::BoardView, home::Home};

fn main() {
    // Without a hook, a panic in the WASM module aborts with a bare
    // `unreachable` trap: no message, no location, and — because the panic
    // takes out the reactive runtime — a board that silently stops responding
    // to its own events. Log the panic so that failure mode is diagnosable
    // from the browser console instead of looking like a rendering bug.
    std::panic::set_hook(Box::new(|info| {
        leptos::logging::error!("wasm panic: {info}");
    }));
    mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    view! {
        <Router>
            <Routes fallback=|| view! { <p class="page loading-text">"Not found"</p> }>
                <Route path=path!("/") view=Home />
                <Route path=path!("/boards/:slug") view=BoardView />
            </Routes>
        </Router>
    }
}
