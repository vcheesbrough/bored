use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map};

#[component]
pub fn BoardChooser(board_name: RwSignal<String>) -> impl IntoView {
    let params = use_params_map();
    let current_id = move || params.with(|p| p.get("id").unwrap_or_default());

    let show = RwSignal::new(false);
    let boards: RwSignal<Vec<shared::Board>> = RwSignal::new(Vec::new());
    let new_name = RwSignal::new(String::new());
    let navigate = use_navigate();

    Effect::new(move |_| {
        if show.get() {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(fetched) = crate::api::fetch_boards().await {
                    boards.set(fetched);
                }
            });
        }
    });

    let on_create = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let name = new_name.get_untracked();
        if name.trim().is_empty() {
            return;
        }
        let nav = navigate.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_board(name).await {
                Ok(board) => {
                    new_name.set(String::new());
                    show.set(false);
                    nav(&format!("/boards/{}", board.id), Default::default());
                }
                Err(e) => leptos::logging::error!("failed to create board: {e}"),
            }
        });
    };

    view! {
        <div class="board-chooser-wrap">
            <button
                class="navbar-board-btn"
                on:click=move |_| show.update(|s| *s = !*s)
            >
                {move || board_name.get()}
                <span class="chooser-caret">"▾"</span>
            </button>

            <div
                class="chooser-backdrop"
                style:display=move || if show.get() { "block" } else { "none" }
                on:click=move |_| show.set(false)
            />

            <div
                class="board-chooser"
                style:display=move || if show.get() { "flex" } else { "none" }
            >
                <For
                    each=move || boards.get()
                    key=|b| b.id.clone()
                    children=move |board| {
                        let href = format!("/boards/{}", board.id);
                        let board_id = board.id.clone();
                        view! {
                            <a
                                href=href
                                class="chooser-item"
                                class:chooser-item-active=move || board_id == current_id()
                                on:click=move |_| show.set(false)
                            >
                                {board.name.clone()}
                            </a>
                        }
                    }
                />
                <div class="chooser-divider" />
                <form class="chooser-new-form" on:submit=on_create>
                    <input
                        type="text"
                        placeholder="New board name"
                        prop:value=move || new_name.get()
                        on:input=move |ev| new_name.set(event_target_value(&ev))
                    />
                    <button type="submit">"Create"</button>
                </form>
            </div>
        </div>
    }
}
