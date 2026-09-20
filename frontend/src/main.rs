mod api;
pub(crate) mod audit_edit_session;
mod caret;
mod columns;
mod components;
mod connection;
mod events;
mod links;
mod pages;
mod recent;
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
    // The heartbeat that notices a redeploy and a dead server. Started here,
    // at the root, so it covers every route — including home and the boards
    // list, which have no SSE stream of their own. It is idempotent.
    connection::start();

    // Mirror the connection state onto `<html data-connection="…">`.
    //
    // The attribute rather than a wrapper element: the pages' markup starts at
    // `<nav class="navbar">` under `<body>`, and wrapping that in a new div to
    // carry one attribute would change the layout every stylesheet rule is
    // written against. `style.css` keys its offline rules off this attribute.
    let connection_state = connection::state();
    Effect::new(move |_| {
        let Some(root) = document().document_element() else {
            return;
        };
        match connection_state.get() {
            connection::ConnectionState::Connected => {
                let _ = root.remove_attribute("data-connection");
            }
            connection::ConnectionState::Disconnected => {
                let _ = root.set_attribute("data-connection", "offline");
            }
        }
    });

    view! {
        <Router>
            <Routes fallback=|| view! { <p class="page loading-text">"Not found"</p> }>
                <Route path=path!("/") view=Home />
                <Route path=path!("/boards/:slug") view=BoardView />
            </Routes>
        </Router>
    }
}
