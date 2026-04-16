use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::components::board_chooser::BoardChooser;
use crate::components::column::ColumnView;

#[component]
pub fn BoardView() -> impl IntoView {
    let params = use_params_map();
    let board_id = move || params.with(|p| p.get("id").unwrap_or_default());

    let board_name = RwSignal::new(String::new());
    let columns = RwSignal::new(Vec::<shared::Column>::new());
    let new_col_name = RwSignal::new(String::new());
    let loading = RwSignal::new(true);

    Effect::new(move |_| {
        let name = board_name.get();
        if !name.is_empty() {
            document().set_title(&format!("{name} — bored"));
        }
    });

    on_cleanup(|| {
        document().set_title("bored");
    });

    Effect::new(move |_| {
        let id = board_id();
        if id.is_empty() {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(board) = crate::api::fetch_board(&id).await {
                board_name.set(board.name);
            }
            match crate::api::fetch_columns(&id).await {
                Ok(fetched) => columns.set(fetched),
                Err(e) => leptos::logging::error!("failed to fetch columns: {e}"),
            }
            loading.set(false);
        });
    });

    let on_add_column = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let name = new_col_name.get_untracked();
        if name.trim().is_empty() {
            return;
        }
        let id = board_id();
        let next_position = columns.get_untracked().len() as i32;
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_column(&id, name, next_position).await {
                Ok(col) => {
                    columns.update(|cs| cs.push(col));
                    new_col_name.set(String::new());
                }
                Err(e) => leptos::logging::error!("failed to create column: {e}"),
            }
        });
    };

    view! {
        <nav class="navbar">
            <a href="/" class="navbar-brand">"bored"</a>
            <span class="navbar-sep">"/"</span>
            <BoardChooser board_id=board_id() board_name=board_name />
        </nav>

        <div class="page board-view">
            <div class="page-header">
                <form class="add-col-form" on:submit=on_add_column>
                    <input
                        type="text"
                        placeholder="Add column…"
                        prop:value=move || new_col_name.get()
                        on:input=move |ev| new_col_name.set(event_target_value(&ev))
                    />
                    <button type="submit">"Add"</button>
                </form>
            </div>

            <Show when=move || loading.get() fallback=|| ()>
                <p class="loading-text">"Loading..."</p>
            </Show>

            <div class="columns-row">
                <For
                    each=move || columns.get()
                    key=|c| c.id.clone()
                    children=|col| view! { <ColumnView column=col /> }
                />
            </div>
        </div>
    }
}
