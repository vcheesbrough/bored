use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

#[component]
pub fn BoardView() -> impl IntoView {
    let params = use_params_map();
    let board_id = move || params.with(|p| p.get("id").unwrap_or_default());

    let columns = RwSignal::new(Vec::<shared::Column>::new());
    let new_col_name = RwSignal::new(String::new());
    let loading = RwSignal::new(true);

    // Fetch columns when board_id changes
    Effect::new(move |_| {
        let id = board_id();
        if id.is_empty() {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            let fetched = crate::api::fetch_columns(&id).await;
            columns.set(fetched);
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
            let col = crate::api::create_column(&id, name, next_position).await;
            columns.update(|cs| cs.push(col));
            new_col_name.set(String::new());
        });
    };

    view! {
        <div class="board-view">
            <h1>"Board: " {move || board_id()}</h1>

            <Show when=move || loading.get() fallback=|| ()>
                <p>"Loading..."</p>
            </Show>

            <div class="columns-row">
                <For
                    each=move || columns.get()
                    key=|c| c.id.clone()
                    children=|col| {
                        view! {
                            <div class="column-card">
                                <h3>{col.name.clone()}</h3>
                                <p>"Position: " {col.position}</p>
                            </div>
                        }
                    }
                />
            </div>

            <form on:submit=on_add_column>
                <input
                    type="text"
                    placeholder="New column name"
                    prop:value=move || new_col_name.get()
                    on:input=move |ev| {
                        new_col_name.set(event_target_value(&ev));
                    }
                />
                <button type="submit">"Add Column"</button>
            </form>

            <a href="/">"Back to Boards"</a>
        </div>
    }
}
