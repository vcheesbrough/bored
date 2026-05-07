use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map};

use crate::components::history_panel::{HistoryDrawer, HistoryIcon, HistoryScope};

#[component]
pub fn BoardChooser(
    board_name: RwSignal<String>,
    columns: RwSignal<Vec<RwSignal<shared::Column>>>,
) -> impl IntoView {
    let params = use_params_map();
    // Reads the `:slug` route parameter — the board name, which doubles as the URL slug.
    let current_slug = move || params.with(|p| p.get("slug").unwrap_or_default());

    let show = RwSignal::new(false);
    let boards: RwSignal<Vec<shared::Board>> = RwSignal::new(Vec::new());
    let new_board_name = RwSignal::new(String::new());
    let new_col_name = RwSignal::new(String::new());
    let adding_board = RwSignal::new(false);
    let adding_col = RwSignal::new(false);
    // Prevents duplicate `create_column` calls when Enter and `blur` both fire.
    let create_col_inflight = RwSignal::new(false);
    let editing_col: RwSignal<Option<String>> = RwSignal::new(None);
    let edit_buf = RwSignal::new(String::new());
    let navigate = use_navigate();
    let navigate_for_delete = navigate.clone();
    let history_drawer = use_context::<HistoryDrawer>();

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
                    // Board name is the slug; navigate by name.
                    nav(&format!("/boards/{}", board.name), Default::default());
                }
                Err(e) => leptos::logging::error!("failed to create board: {e}"),
            }
        });
    });

    let submit_new_col: Callback<()> = Callback::new(move |_| {
        if create_col_inflight.get_untracked() {
            return;
        }
        let name = new_col_name.get_untracked();
        if name.trim().is_empty() {
            adding_col.set(false);
            return;
        }
        // Use the board slug (current route param) as the API path segment.
        let board_id = current_slug();
        if board_id.is_empty() {
            return;
        }
        let position = columns.with_untracked(|cs| cs.len() as i32);
        create_col_inflight.set(true);
        wasm_bindgen_futures::spawn_local(async move {
            let res = crate::api::create_column(&board_id, name, position).await;
            create_col_inflight.set(false);
            match res {
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

    let delete_board_cb: Callback<(String, String)> =
        Callback::new(move |(board_slug, b_name): (String, String)| {
            let confirmed = window()
                .confirm_with_message(&format!(
                    "Delete board \"{}\" and all its columns and cards?",
                    b_name
                ))
                .unwrap_or(false);
            if !confirmed {
                return;
            }
            // Compare slug vs current route slug to decide whether to navigate away.
            let was_current = board_slug == current_slug();
            let nav = navigate_for_delete.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match crate::api::delete_board(&board_slug).await {
                    Ok(()) => {
                        // Retain by name (slug), not by ULID.
                        boards.update(|bs| bs.retain(|b| b.name != board_slug));
                        if was_current {
                            // Navigate to the next board by its slug (name), or home.
                            let next_slug =
                                boards.with_untracked(|bs| bs.first().map(|b| b.name.clone()));
                            show.set(false);
                            match next_slug {
                                Some(slug) => nav(&format!("/boards/{}", slug), Default::default()),
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
                <svg
                    class="chooser-gear"
                    viewBox="0 0 24 24"
                    fill="currentColor"
                >
                    <path
                        fill-rule="evenodd"
                        clip-rule="evenodd"
                        d="M11.078 2.25c-.917 0-1.699.663-1.85 1.567L9.05 4.889c-.02.12-.115.26-.297.348a7.493 7.493 0 0 0-.986.57c-.166.115-.334.126-.45.083L6.3 5.508a1.875 1.875 0 0 0-2.282.819l-.922 1.597a1.875 1.875 0 0 0 .432 2.385l.84.692c.095.078.17.229.154.43a7.598 7.598 0 0 0 0 1.139c.015.2-.059.352-.153.43l-.841.692a1.875 1.875 0 0 0-.432 2.385l.922 1.597a1.875 1.875 0 0 0 2.282.818l1.019-.382c.115-.043.283-.031.45.082.312.214.641.405.985.57.182.088.277.228.297.35l.178 1.071c.151.904.933 1.567 1.85 1.567h1.844c.916 0 1.699-.663 1.85-1.567l.178-1.072c.02-.12.114-.26.297-.349.344-.165.673-.356.985-.57.167-.114.335-.125.45-.082l1.02.382a1.875 1.875 0 0 0 2.28-.819l.923-1.597a1.875 1.875 0 0 0-.432-2.385l-.84-.692c-.095-.078-.17-.229-.154-.43a7.614 7.614 0 0 0 0-1.139c-.016-.2.059-.352.153-.43l.84-.692c.708-.582.891-1.59.433-2.385l-.922-1.597a1.875 1.875 0 0 0-2.282-.818l-1.02.382c-.114.043-.282.031-.449-.083a7.49 7.49 0 0 0-.985-.57c-.183-.087-.277-.227-.297-.348l-.179-1.072a1.875 1.875 0 0 0-1.85-1.567h-1.843ZM12 15.75a3.75 3.75 0 1 0 0-7.5 3.75 3.75 0 0 0 0 7.5Z"
                    />
                </svg>
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
                        // Board name IS the slug; use it directly in the URL.
                        let href = format!("/boards/{}", board.name);
                        let board_slug_active = board.name.clone();
                        let board_slug_delete = board.name.clone();
                        let board_name_delete = board.name.clone();
                        view! {
                            <div class="chooser-board-row">
                                <a
                                    href=href
                                    class="chooser-item"
                                    class:chooser-item-active=move || board_slug_active == current_slug()
                                    on:click=move |_| show.set(false)
                                >
                                    {board.name.clone()}
                                </a>
                                <Show when=move || history_drawer.is_some() fallback=|| ()>
                                    <button
                                        class="chooser-history-btn"
                                        type="button"
                                        title="Board history"
                                        on:click=move |ev| {
                                            ev.prevent_default();
                                            ev.stop_propagation();
                                            if let Some(hd) = history_drawer {
                                                hd.0.set(Some(HistoryScope::Board));
                                            }
                                            show.set(false);
                                        }
                                    ><HistoryIcon /></button>
                                </Show>
                                <button
                                    class="chooser-board-delete"
                                    title="Delete board"
                                    on:click=move |ev| {
                                        ev.prevent_default();
                                        ev.stop_propagation();
                                        delete_board_cb.run((
                                            board_slug_delete.clone(),
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
                    on:input=move |ev| {
                        // Sanitize on every input event (covers paste, autocomplete,
                        // drag-and-drop — not just keystrokes). Lowercase first so
                        // pasted uppercase text is converted rather than stripped.
                        let sanitized: String = event_target_value(&ev)
                            .to_ascii_lowercase()
                            .chars()
                            .filter(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
                            .collect();
                        new_board_name.set(sanitized);
                    }
                    on:blur=move |_| submit_new_board.run(())
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        let key = ev.key();
                        if key == "Enter" {
                            ev.prevent_default();
                            submit_new_board.run(());
                        } else if key == "Escape" {
                            new_board_name.set(String::new());
                            adding_board.set(false);
                        } else if key.len() == 1 {
                            // Block non-slug characters (only [a-z0-9-] allowed).
                            let ch = key.chars().next().unwrap();
                            if !matches!(ch, 'a'..='z' | '0'..='9' | '-') {
                                ev.prevent_default();
                            }
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
                                        move |_| commit_edit(sig)
                                    }
                                    on:keydown={
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
                                <Show when=move || history_drawer.is_some() fallback=|| ()>
                                    <button
                                        class="chooser-history-btn"
                                        type="button"
                                        title="Column history"
                                        on:click=move |ev| {
                                            ev.prevent_default();
                                            ev.stop_propagation();
                                            if let Some(hd) = history_drawer {
                                                let cid = sig.get_untracked().id.clone();
                                                hd.0.set(Some(HistoryScope::Column(cid)));
                                            }
                                            show.set(false);
                                        }
                                    ><HistoryIcon /></button>
                                </Show>
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
