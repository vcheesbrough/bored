mod api;
pub(crate) mod audit_edit_session;
mod components;
mod events;
mod pages;

use leptos::prelude::*;
use leptos_router::{
    components::{Route, Router, Routes},
    path,
};
use pages::{board_view::BoardView, home::Home};

fn main() {
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
