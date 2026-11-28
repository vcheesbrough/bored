//! The navbar switch between text mode and graph mode (card #303).
//!
//! A graph icon followed by a toggle switch, left of the search box. It is one
//! `<button role="switch">`, so the icon is part of the click target, a
//! screen reader announces it as an on/off switch, and `aria-checked` says
//! which way it is set. There was no toggle component in the app before this
//! one; the switch's look is plain CSS (`.graph-switch` in `style.css`).
//!
//! Flipping it navigates between `/boards/:slug` and `/boards/:slug/graph`.
//! Both are children of the same parent route, so `BoardView` — and with it
//! the board's data, its event stream and the search box's query — stays
//! mounted; only the mode changes.

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::use_navigate;

use crate::graph::{BoardMode, BoardModeContext};

#[component]
pub fn GraphToggle(board_slug: RwSignal<String>) -> AnyView {
    // caps monomorphization at this boundary — see CardVersionActions doc comment in history_panel.rs
    let mode = expect_context::<BoardModeContext>();
    // `use_navigate` returns a closure that is not `Copy`; a `StoredValue`
    // gives the click handler a `Copy` handle to it instead of a clone.
    let navigate = StoredValue::new(use_navigate());

    // Tracked read for the bindings; `try_get` because the memo belongs to
    // `BoardView` and a binding can run while the page unmounts.
    let in_graph = move || mode.0.try_get() == Some(BoardMode::Graph);

    let flip = move |_: web_sys::MouseEvent| {
        let next = mode.now().toggled();
        let url = next.board_path(&board_slug.get_untracked());
        navigate.with_value(|nav| nav(&url, NavigateOptions::default()));
    };

    view! {
        <button
            class="graph-toggle"
            type="button"
            role="switch"
            aria-checked=move || in_graph().to_string()
            aria-label="Graph view"
            title=move || if in_graph() { "Show the board as cards" } else { "Show the board's links as a graph" }
            on:click=flip
        >
            // Three nodes joined by two lines: one card leading to two.
            <svg
                class="graph-toggle-icon"
                width="18"
                height="18"
                viewBox="0 0 18 18"
                aria-hidden="true"
            >
                <line x1="9" y1="5" x2="4.5" y2="13" />
                <line x1="9" y1="5" x2="13.5" y2="13" />
                <circle cx="9" cy="4" r="2.4" />
                <circle cx="4" cy="14" r="2.4" />
                <circle cx="14" cy="14" r="2.4" />
            </svg>
            <span class="graph-switch" aria-hidden="true">
                <span class="graph-switch-thumb"></span>
            </span>
        </button>
    }
    .into_any()
}
