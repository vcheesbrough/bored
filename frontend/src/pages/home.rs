use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

#[component]
pub fn Home() -> impl IntoView {
    let navigate = use_navigate();
    let navigate2 = navigate.clone();
    let new_board_name = RwSignal::new(String::new());
    let no_boards = RwSignal::new(false);

    Effect::new(move |_| {
        let nav = navigate.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::fetch_boards().await {
                Ok(boards) => {
                    if let Some(first) = boards.into_iter().next() {
                        nav(&format!("/boards/{}", first.id), Default::default());
                    } else {
                        no_boards.set(true);
                    }
                }
                Err(e) => leptos::logging::error!("failed to fetch boards: {e}"),
            }
        });
    });

    let on_create = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
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
        </nav>
        <div class="page">
            <p
                class="loading-text"
                style:display=move || if no_boards.get() { "none" } else { "block" }
            >
                "Loading…"
            </p>
            <div
                class="empty-state"
                style:display=move || if no_boards.get() { "flex" } else { "none" }
            >
                <p class="page-title">"No boards yet"</p>
                <form class="create-form" on:submit=on_create>
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
