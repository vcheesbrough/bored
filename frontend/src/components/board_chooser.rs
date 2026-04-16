use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map};

#[component]
pub fn BoardChooser(
    board_name: RwSignal<String>,
    columns: RwSignal<Vec<RwSignal<shared::Column>>>,
) -> impl IntoView {
    let params = use_params_map();
    let current_id = move || params.with(|p| p.get("id").unwrap_or_default());

    let show = RwSignal::new(false);
    let boards: RwSignal<Vec<shared::Board>> = RwSignal::new(Vec::new());
    let new_board_name = RwSignal::new(String::new());
    let new_col_name = RwSignal::new(String::new());
    let adding_board = RwSignal::new(false);
    let adding_col = RwSignal::new(false);
    let editing_col: RwSignal<Option<String>> = RwSignal::new(None);
    let edit_buf = RwSignal::new(String::new());
    let navigate = use_navigate();
    let navigate_for_delete = navigate.clone();

    Effect::new(move |_| {
        if show.get() {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(fetched) = crate::api::fetch_boards().await {
                    boards.set(fetched);
                }
            });
        }
    });

    let submit_new_board: Callback<()> = Callback::new(move |_| {
        let name = new_board_name.get_untracked();
        if name.trim().is_empty() {
            adding_board.set(false);
            return;
        }
        let nav = navigate.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_board(name).await {
                Ok(board) => {
                    new_board_name.set(String::new());
                    adding_board.set(false);
                    show.set(false);
                    nav(&format!("/boards/{}", board.id), Default::default());
                }
                Err(e) => leptos::logging::error!("failed to create board: {e}"),
            }
        });
    });

    let submit_new_col: Callback<()> = Callback::new(move |_| {
        let name = new_col_name.get_untracked();
        if name.trim().is_empty() {
            adding_col.set(false);
            return;
        }
        let board_id = current_id();
        if board_id.is_empty() {
            return;
        }
        let position = columns.with_untracked(|cs| cs.len() as i32);
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::create_column(&board_id, name, position).await {
                Ok(col) => {
                    columns.update(|cs| cs.push(RwSignal::new(col)));
                    new_col_name.set(String::new());
                    adding_col.set(false);
                }
                Err(e) => leptos::logging::error!("failed to create column: {e}"),
            }
        });
    });

    let start_edit = move |col: &shared::Column| {
        edit_buf.set(col.name.clone());
        editing_col.set(Some(col.id.clone()));
    };

    let commit_edit = move |sig: RwSignal<shared::Column>| {
        let new_name = edit_buf.get_untracked();
        let current = sig.get_untracked();
        editing_col.set(None);
        if new_name.trim().is_empty() || new_name == current.name {
            return;
        }
        let col_id = current.id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let req = shared::UpdateColumnRequest {
                name: Some(new_name),
                position: None,
            };
            match crate::api::update_column(&col_id, req).await {
                Ok(updated) => sig.set(updated),
                Err(e) => leptos::logging::error!("failed to update column: {e}"),
            }
        });
    };

    let delete_board_cb: Callback<(String, String)> = Callback::new(move |(board_id, b_name): (String, String)| {
        let confirmed = window()
            .confirm_with_message(&format!(
                "Delete board \"{}\" and all its columns and cards?",
                b_name
            ))
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        let was_current = board_id == current_id();
        let nav = navigate_for_delete.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::delete_board(&board_id).await {
                Ok(()) => {
                    boards.update(|bs| bs.retain(|b| b.id != board_id));
                    if was_current {
                        let next_id = boards.with_untracked(|bs| bs.first().map(|b| b.id.clone()));
                        show.set(false);
                        match next_id {
                            Some(id) => nav(&format!("/boards/{}", id), Default::default()),
                            None => nav("/", Default::default()),
                        }
                    }
                }
                Err(e) => leptos::logging::error!("failed to delete board: {e}"),
            }
        });
    });

    let delete_col = move |sig: RwSignal<shared::Column>| {
        let col = sig.get_untracked();
        let confirmed = window()
            .confirm_with_message(&format!(
                "Delete column \"{}\" and all its cards?",
                col.name
            ))
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        let col_id = col.id.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::delete_column(&col_id).await {
                Ok(()) => columns.update(|cs| cs.retain(|s| s.get_untracked().id != col_id)),
                Err(e) => leptos::logging::error!("failed to delete column: {e}"),
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
                <span class="chooser-gear">"⚙"</span>
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
                <div class="chooser-section-label">"Boards"</div>
                <For
                    each=move || boards.get()
                    key=|b| b.id.clone()
                    children=move |board| {
                        let href = format!("/boards/{}", board.id);
                        let board_id_active = board.id.clone();
                        let board_id_delete = board.id.clone();
                        let board_name_delete = board.name.clone();
                        view! {
                            <div class="chooser-board-row">
                                <a
                                    href=href
                                    class="chooser-item"
                                    class:chooser-item-active=move || board_id_active == current_id()
                                    on:click=move |_| show.set(false)
                                >
                                    {board.name.clone()}
                                </a>
                                <button
                                    class="chooser-board-delete"
                                    title="Delete board"
                                    on:click=move |ev| {
                                        ev.prevent_default();
                                        ev.stop_propagation();
                                        delete_board_cb.run((
                                            board_id_delete.clone(),
                                            board_name_delete.clone(),
                                        ));
                                    }
                                >"×"</button>
                            </div>
                        }
                    }
                />
                <div
                    class="chooser-item chooser-item-phantom"
                    style:display=move || if adding_board.get() { "none" } else { "block" }
                    on:click=move |_| {
                        new_board_name.set(String::new());
                        adding_board.set(true);
                    }
                >
                    <span class="chooser-phantom-plus">"+"</span>
                    " Add board"
                </div>
                <input
                    type="text"
                    class="chooser-item-input"
                    placeholder="Board name"
                    style:display=move || if adding_board.get() { "block" } else { "none" }
                    prop:value=move || new_board_name.get()
                    on:input=move |ev| new_board_name.set(event_target_value(&ev))
                    on:blur=move |_| submit_new_board.run(())
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            ev.prevent_default();
                            submit_new_board.run(());
                        } else if ev.key() == "Escape" {
                            new_board_name.set(String::new());
                            adding_board.set(false);
                        }
                    }
                    node_ref={
                        let r = NodeRef::<leptos::html::Input>::new();
                        Effect::new(move |_| {
                            if adding_board.get() {
                                if let Some(el) = r.get() {
                                    let _ = el.focus();
                                }
                            }
                        });
                        r
                    }
                />

                <div class="chooser-divider" />

                <div class="chooser-section-label">"Columns"</div>
                <For
                    each=move || columns.get()
                    key=|sig| sig.get_untracked().id.clone()
                    children=move |sig| {
                        let start_edit = start_edit.clone();
                        let commit_edit = commit_edit.clone();
                        let delete_col = delete_col.clone();
                        let is_editing = move || {
                            editing_col.with(|e| {
                                e.as_ref()
                                    .is_some_and(|id| id == &sig.get_untracked().id)
                            })
                        };
                        view! {
                            <div class="chooser-col-row">
                                <span
                                    class="chooser-col-name"
                                    style:display=move || if is_editing() { "none" } else { "block" }
                                    on:click={
                                        let start_edit = start_edit.clone();
                                        move |_| start_edit(&sig.get_untracked())
                                    }
                                >
                                    {move || sig.get().name.clone()}
                                </span>
                                <input
                                    type="text"
                                    class="chooser-col-edit"
                                    style:display=move || if is_editing() { "block" } else { "none" }
                                    prop:value=move || edit_buf.get()
                                    on:input=move |ev| edit_buf.set(event_target_value(&ev))
                                    on:blur={
                                        let commit_edit = commit_edit.clone();
                                        move |_| commit_edit(sig)
                                    }
                                    on:keydown={
                                        let commit_edit = commit_edit.clone();
                                        move |ev| {
                                            if ev.key() == "Enter" {
                                                ev.prevent_default();
                                                commit_edit(sig);
                                            } else if ev.key() == "Escape" {
                                                editing_col.set(None);
                                            }
                                        }
                                    }
                                />
                                <button
                                    class="chooser-col-delete"
                                    title="Delete column"
                                    on:click=move |_| delete_col(sig)
                                >"×"</button>
                            </div>
                        }
                    }
                />
                <div
                    class="chooser-col-row chooser-col-row-phantom"
                    style:display=move || if adding_col.get() { "none" } else { "flex" }
                    on:click=move |_| {
                        new_col_name.set(String::new());
                        adding_col.set(true);
                    }
                >
                    <span class="chooser-phantom-plus">"+"</span>
                    <span class="chooser-col-name chooser-col-name-phantom">" Add column"</span>
                </div>
                <div
                    class="chooser-col-row"
                    style:display=move || if adding_col.get() { "flex" } else { "none" }
                >
                    <input
                        type="text"
                        class="chooser-col-edit"
                        placeholder="Column name"
                        prop:value=move || new_col_name.get()
                        on:input=move |ev| new_col_name.set(event_target_value(&ev))
                        on:blur=move |_| submit_new_col.run(())
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" {
                                ev.prevent_default();
                                submit_new_col.run(());
                            } else if ev.key() == "Escape" {
                                new_col_name.set(String::new());
                                adding_col.set(false);
                            }
                        }
                        node_ref={
                            let r = NodeRef::<leptos::html::Input>::new();
                            Effect::new(move |_| {
                                if adding_col.get() {
                                    if let Some(el) = r.get() {
                                        let _ = el.focus();
                                    }
                                }
                            });
                            r
                        }
                    />
                </div>
            </div>
        </div>
    }
}
