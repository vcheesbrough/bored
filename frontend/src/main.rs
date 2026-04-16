mod api;
mod components;
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
                <Route path=path!("/boards/:id") view=BoardView />
            </Routes>
        </Router>
        <div class="app-watermark">"v" {env!("CARGO_PKG_VERSION")}</div>
    }
}
