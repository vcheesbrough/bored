use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::events::BoardSseEvent;

/// Where the user opened history from — drives default tab + filters.
#[derive(Clone, PartialEq)]
pub enum HistoryScope {
    Board,
    Column(String),
    Card(String),
}

#[derive(Clone, Copy)]
pub struct HistoryDrawer(pub RwSignal<Option<HistoryScope>>);

#[component]
pub fn HistoryPanel(
    board_slug: RwSignal<String>,
    board_ulid: RwSignal<String>,
    sse_event: RwSignal<Option<BoardSseEvent>>,
) -> impl IntoView {
    let drawer = use_context::<HistoryDrawer>().expect("HistoryDrawer context missing");
    let entries = RwSignal::new(Vec::<shared::AuditLogEntry>::new());
    let loading = RwSignal::new(false);
    let show_moves = RwSignal::new(false);
    let tab = RwSignal::new(0u8);
    let filter_column_id: RwSignal<Option<String>> = RwSignal::new(None);
    let filter_card_id: RwSignal<Option<String>> = RwSignal::new(None);

    let reload = Callback::new(move |_: ()| {
        let slug = board_slug.get_untracked();
        if slug.is_empty() {
            return;
        }
        loading.set(true);
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::fetch_board_history(&slug).await {
                Ok(rows) => entries.set(rows),
                Err(e) => leptos::logging::error!("fetch_board_history: {e}"),
            }
            loading.set(false);
        });
    });

    Effect::new(move |_| {
        if let Some(scope) = drawer.0.get() {
            match scope {
                HistoryScope::Board => {
                    tab.set(0);
                    filter_column_id.set(None);
                    filter_card_id.set(None);
                }
                HistoryScope::Column(id) => {
                    tab.set(1);
                    filter_column_id.set(Some(id));
                    filter_card_id.set(None);
                }
                HistoryScope::Card(id) => {
                    tab.set(2);
                    filter_card_id.set(Some(id));
                    filter_column_id.set(None);
                }
            }
        }
    });

    Effect::new(move |_| {
        drawer.0.get();
        board_slug.get();
        if drawer.0.get_untracked().is_some() {
            reload.run(());
        }
    });

    Effect::new(move |_| {
        let Some(ev) = sse_event.get() else {
            return;
        };
        if drawer.0.get_untracked().is_none() {
            return;
        };
        let ulid = board_ulid.get_untracked();
        if let BoardSseEvent::AuditAppended { entry } = ev {
            if entry.board_id == ulid {
                entries.update(|es| {
                    if !es.iter().any(|r| r.id == entry.id) {
                        es.insert(0, entry);
                    }
                });
            }
        }
    });

    let filtered = Signal::derive(move || {
        let mut rows: Vec<_> = entries
            .get()
            .into_iter()
            .filter(|e| show_moves.get() || e.action != "move")
            .collect();
        match tab.get() {
            1 => {
                if let Some(cid) = filter_column_id.get() {
                    rows.retain(|e| {
                        (e.entity_type == "column" && e.entity_id == cid)
                            || (e.entity_type == "card"
                                && e.snapshot_before
                                    .as_ref()
                                    .and_then(|v| v.get("column_id"))
                                    .and_then(|c| c.as_str())
                                    == Some(cid.as_str()))
                            || (e.entity_type == "card"
                                && e.snapshot_after
                                    .as_ref()
                                    .and_then(|v| v.get("column_id"))
                                    .and_then(|c| c.as_str())
                                    == Some(cid.as_str()))
                    });
                } else {
                    rows.retain(|e| e.entity_type == "column");
                }
            }
            2 => {
                if let Some(kid) = filter_card_id.get() {
                    rows.retain(|e| e.entity_type == "card" && e.entity_id == kid);
                } else {
                    rows.retain(|e| e.entity_type == "card");
                }
            }
            _ => {}
        }
        rows
    });

    let close = move |_| drawer.0.set(None);

    view! {
        <Show when=move || drawer.0.get().is_some() fallback=|| ()>
            <div class="history-backdrop" on:click=close></div>
            <aside class="history-drawer">
                <div class="history-drawer-header">
                    <span class="history-drawer-title">"🕘 History"</span>
                    <button
                        class="card-toolbar-btn history-drawer-close"
                        type="button"
                        title="Close"
                        on:click=close
                    >"✕"</button>
                </div>

                <div class="history-tabs">
                    <button
                        type="button"
                        class="history-tab"
                        class:history-tab-active=move || tab.get() == 0
                        on:click=move |_| tab.set(0)
                    >"Board"</button>
                    <button
                        type="button"
                        class="history-tab"
                        class:history-tab-active=move || tab.get() == 1
                        on:click=move |_| tab.set(1)
                    >"Column"</button>
                    <button
                        type="button"
                        class="history-tab"
                        class:history-tab-active=move || tab.get() == 2
                        on:click=move |_| tab.set(2)
                    >"Card"</button>
                </div>

                <label class="history-toggle">
                    <input
                        type="checkbox"
                        prop:checked=move || show_moves.get()
                        on:change=move |ev: web_sys::Event| {
                            let checked = ev
                                .target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                                .map(|el| el.checked())
                                .unwrap_or(false);
                            show_moves.set(checked);
                        }
                    />
                    <span>"Show moves"</span>
                </label>

                <Show when=move || !loading.get() fallback=move || view! {
                    <p class="loading-text">"Loading…"</p>
                }>
                    <ul class="history-list">
                        <For
                            each=move || filtered.get()
                            key=|e| e.id.clone()
                            children=move |e| {
                                let aid = e.id.clone();
                                let action = e.action.clone();
                                let badge_class = format!("history-badge history-badge-{action}");
                                let can_restore = e.action == "delete";
                                let line1 = format!("{} {}", e.entity_type, e.entity_id);
                                let line2 = format!("{} · {}", e.actor_display_name, e.created_at);
                                view! {
                                    <li class="history-row">
                                        <div class="history-row-meta">
                                            <span class=badge_class>{action}</span>
                                            <span class="history-entity">{line1}</span>
                                        </div>
                                        <div class="history-actor">{line2}</div>
                                        <Show when=move || can_restore fallback=|| ()>
                                            <button
                                                type="button"
                                                class="btn btn-restore"
                                                on:click={
                                                    let reload = reload.clone();
                                                    let audit_id = aid.clone();
                                                    move |_| {
                                                        let id = audit_id.clone();
                                                        let reload = reload.clone();
                                                        wasm_bindgen_futures::spawn_local(async move {
                                                            match crate::api::restore_audit_entry(&id).await {
                                                                Ok(_) => reload.run(()),
                                                                Err(err) => leptos::logging::error!("restore failed: {err}"),
                                                            }
                                                        });
                                                    }
                                                }
                                            >"Restore"</button>
                                        </Show>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </Show>
            </aside>
        </Show>
    }
}
