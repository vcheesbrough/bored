// Navbar badge for a lost link to the server.
//
// Renders nothing at all while the tab is connected — the healthy state is
// already conveyed by the board working — and a short "offline" pill the
// moment either the `/api/info` heartbeat or the board's SSE stream stops
// answering. See [`crate::connection`] for what sets that state and
// [`crate::api`] for the mutations it refuses while it holds.
//
// Lives in every navbar (board view, home, boards list) so the explanation is
// wherever the user is when their edits stop being accepted.

use leptos::prelude::*;

use crate::connection::{self, ConnectionState};

#[component]
pub fn ConnectionStatus() -> impl IntoView {
    let state = connection::state();

    view! {
        <Show when=move || state.get() == ConnectionState::Disconnected>
            <span
                class="navbar-connection"
                // Announced by screen readers when it appears, and read on
                // hover by everyone else: the pill itself has room for two
                // words, and the consequence is the part that matters.
                role="status"
                title="No connection to the server. Changes cannot be saved until it returns."
            >
                <span class="navbar-connection-dot" aria-hidden="true"></span>
                "offline"
            </span>
        </Show>
    }
}
