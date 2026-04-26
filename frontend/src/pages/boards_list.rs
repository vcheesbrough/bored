use leptos::prelude::*;
use leptos_router::components::A;

#[component]
pub fn BoardsList() -> impl IntoView {
    let boards = RwSignal::new(Vec::<shared::Board>::new());
    let new_board_name = RwSignal::new(String::new());
    let loading = RwSignal::new(true);

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::fetch_boards().await {
                Ok(fetched) => boards.set(fetched),
                Err(e) => leptos::logging::error!("failed to fetch boards: {e}"),
            }
            loading.set(false);
        });
    });

    let on_create = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let name = new_board_name.get_untracked();
        if name.trim().is_empty() {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_board(name).await {
                Ok(board) => {
                    boards.update(|bs| bs.push(board));
                    new_board_name.set(String::new());
                }
                Err(e) => leptos::logging::error!("failed to create board: {e}"),
            }
        });
    };

    view! {
        <nav class="navbar">
            <a href="/" class="navbar-brand">"bored"</a>
        </nav>

        <div class="page">
            <div class="page-header">
                <h1 class="page-title">"Boards"</h1>
            </div>

            <Show when=move || loading.get() fallback=|| ()>
                <p class="loading-text">"Loading..."</p>
            </Show>

            <div class="boards-grid">
                <For
                    each=move || boards.get()
                    key=|b| b.id.clone()
                    children=|board| {
                        view! {
                            <div class="board-card">
                                <A href=format!("/boards/{}", board.name)>
                                    {board.name.clone()}
                                </A>
                            </div>
                        }
                    }
                />
            </div>

            <form class="create-form" on:submit=on_create>
                <input
                    type="text"
                    placeholder="New board name"
                    prop:value=move || new_board_name.get()
                    on:input=move |ev| new_board_name.set(event_target_value(&ev))
                />
                <button type="submit">"Create"</button>
            </form>
        </div>
    }
}
