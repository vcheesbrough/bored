use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::events::BoardSseEvent;

/// Where the user opened history from — drives drawer title and row filtering (no tabs).
#[derive(Clone, PartialEq)]
pub enum HistoryScope {
    Board,
    Column(String),
    Card(String),
}

#[derive(Clone, Copy)]
pub struct HistoryDrawer(pub RwSignal<Option<HistoryScope>>);

/// Monochrome clock glyph using `currentColor` — avoids colourful emoji clocks in headers/toolbars.
#[component]
pub fn HistoryIcon() -> impl IntoView {
    view! {
        <svg
            class="history-icon-svg"
            width="1em"
            height="1em"
            viewBox="0 0 16 16"
            fill="none"
            aria-hidden="true"
        >
            <circle cx="8" cy="8" r="6.25" stroke="currentColor" stroke-width="1.5" />
            <path
                d="M8 4.75V8h3.25"
                stroke="currentColor"
                stroke-width="1.5"
                stroke-linecap="round"
                stroke-linejoin="round"
            />
        </svg>
    }
}

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
                    if let Some(i) = es.iter().position(|r| r.id == entry.id) {
                        es[i] = entry.clone();
                    } else {
                        es.insert(0, entry);
                    }
                });
            }
        }
    });

    let drawer_title = Signal::derive(move || match drawer.0.get().as_ref() {
        Some(HistoryScope::Board) => "Board history",
        Some(HistoryScope::Column(_)) => "Column history",
        Some(HistoryScope::Card(_)) => "Card history",
        None => "History",
    });

    let filtered = Signal::derive(move || {
        let mut rows: Vec<_> = entries
            .get()
            .into_iter()
            .filter(|e| show_moves.get() || e.action != "move")
            .collect();

        match drawer.0.get().as_ref() {
            Some(HistoryScope::Board) => {}
            Some(HistoryScope::Column(cid)) => {
                rows.retain(|e| {
                    (e.entity_type == "column" && e.entity_id == *cid)
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
            }
            Some(HistoryScope::Card(kid)) => {
                rows.retain(|e| e.entity_type == "card" && e.entity_id == *kid);
            }
            None => {}
        }
        rows
    });

    let close = move |_| drawer.0.set(None);

    view! {
        <Show when=move || drawer.0.get().is_some() fallback=|| ()>
            <div class="history-backdrop" on:click=close></div>
            <aside class="history-drawer">
                <div class="history-drawer-header">
                    <span class="history-drawer-title">{move || drawer_title.get()}</span>
                    <button
                        class="card-toolbar-btn history-drawer-close"
                        type="button"
                        title="Close"
                        on:click=close
                    >"✕"</button>
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
