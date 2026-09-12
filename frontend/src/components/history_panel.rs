use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::components::markdown::StaticMarkdownPreview;
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
    let expanded_version = RwSignal::new(None::<String>);
    let restoring_version = RwSignal::new(None::<String>);
    let restore_error = RwSignal::new(None::<String>);
    // Filled once when /api/me resolves; powers the «You» rule in label_actor.
    let me_name = RwSignal::new(None::<String>);

    // Fetch the current user's display name once on first mount of the
    // drawer — `label_actor` consults it case-insensitively to label the
    // current actor's rows as «You».
    Effect::new(move |_| {
        if me_name.get_untracked().is_some() {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(user) = crate::api::fetch_me().await {
                me_name.set(Some(user.name));
            }
        });
    });

    let reload = Callback::new(move |_: ()| {
        let slug = board_slug.get_untracked();
        let scope = drawer.0.get_untracked();
        if slug.is_empty() {
            return;
        }
        let Some(scope) = scope else {
            return;
        };
        loading.set(true);
        wasm_bindgen_futures::spawn_local(async move {
            let result = match scope {
                HistoryScope::Board => crate::api::fetch_board_history(&slug).await,
                HistoryScope::Column(cid) => crate::api::fetch_column_history(&cid).await,
                HistoryScope::Card(card_id) => crate::api::fetch_card_history(&card_id).await,
            };
            match result {
                Ok(rows) => entries.set(rows),
                Err(e) => leptos::logging::error!("history fetch: {e}"),
            }
            loading.set(false);
        });
    });

    Effect::new(move |_| {
        drawer.0.get();
        board_slug.get();
        expanded_version.set(None);
        restore_error.set(None);
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
            // Unbox once; the rest of this block works with the entry by value.
            let entry = *entry;
            if entry.board_id != ulid {
                return;
            }
            let keep = match drawer.0.get_untracked().as_ref() {
                Some(HistoryScope::Board) => true,
                Some(HistoryScope::Column(cid)) => entry.matches_history_column_scope(cid),
                Some(HistoryScope::Card(kid)) => entry.matches_history_card_scope(kid),
                None => false,
            };
            if !keep {
                return;
            }
            entries.update(|es| {
                if let Some(i) = es.iter().position(|r| r.id == entry.id) {
                    es[i] = entry.clone();
                } else {
                    es.insert(0, entry);
                }
            });
        }
    });

    // The newest version row — the one whose content the card currently has.
    // Used to badge that row "Current" and to disable its restore button.
    let current_card_version = Signal::derive(move || {
        if !matches!(drawer.0.get(), Some(HistoryScope::Card(_))) {
            return None;
        }
        entries
            .get()
            .into_iter()
            .find_map(|entry| card_content_version(&entry).map(|v| (entry.id.clone(), v)))
    });

    let drawer_title = Signal::derive(move || match drawer.0.get().as_ref() {
        Some(HistoryScope::Board) => "Board history",
        Some(HistoryScope::Column(_)) => "Column history",
        Some(HistoryScope::Card(_)) => "Card history",
        None => "History",
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

                <Show when=move || restore_error.get().is_some() fallback=|| ()>
                    <p class="history-error">{move || restore_error.get().unwrap_or_default()}</p>
                </Show>

                <Show when=move || !loading.get() fallback=move || view! {
                    <p class="loading-text">"Loading…"</p>
                }>
                    <ul class="history-list">
                        <For
                            each=move || {
                                entries
                                    .get()
                                    .into_iter()
                                    .filter(|e| show_moves.get() || e.action != "move")
                                    .collect::<Vec<shared::AuditLogEntry>>()
                            }
                            key=|e: &shared::AuditLogEntry| {
                                (e.id.clone(), e.created_at.clone())
                            }
                            children=move |e: shared::AuditLogEntry| {
                                // Snapshot the runtime context at render time
                                // (re-rendered when entries / show_moves / me_name change).
                                let me = me_name.get();
                                let now = current_now_ms();
                                let offset = current_local_offset_minutes();
                                let tz = current_tz_label();
                                let then = parse_audit_ts(&e.created_at);

                                let actor_label = shared::history::label_actor(
                                    &e.actor_sub,
                                    &e.actor_display_name,
                                    me.as_deref(),
                                );
                                let time_label =
                                    shared::history::format_history_time(now, then, offset);
                                let tooltip =
                                    shared::history::format_history_tooltip(then, offset, &tz);
                                let summary = shared::history::derive_summary(&e);

                                let aid = e.id.clone();
                                let delete_restore_aid = aid.clone();
                                let action = e.action.clone();
                                let entity_id = e.entity_id.clone();
                                let badge_class = format!("history-badge history-badge-{action}");
                                // A deleted link is not replayable — another
                                // link may have closed the loop it would
                                // complete — so the server refuses (422) and
                                // the control is not offered.
                                let can_restore = e.action == "delete" && e.entity_type != "card_link";
                                let is_card_scope = matches!(
                                    drawer.0.get_untracked(),
                                    Some(HistoryScope::Card(_))
                                );
                                let version = is_card_scope
                                    .then(|| card_content_version(&e))
                                    .flatten();
                                let headline = summary.headline;
                                let sub = summary.sub;
                                view! {
                                    <li class="history-row" data-entity-id=entity_id>
                                        <div class="history-row-meta">
                                            <span class=badge_class>{action}</span>
                                            <span class="history-headline">{headline}</span>
                                        </div>
                                        {sub.map(|s| view! {
                                            <div class="history-sub">{s}</div>
                                        })}
                                        <div class="history-meta-line" title=tooltip>
                                            <span class="history-actor">{actor_label}</span>
                                            <span class="history-meta-sep">" · "</span>
                                            <span class="history-time">{time_label}</span>
                                        </div>
                                        <Show when=move || can_restore fallback=|| ()>
                                            <button
                                                type="button"
                                                class="btn btn-restore"
                                                on:click={
                                                    let reload = reload;
                                                    let audit_id = delete_restore_aid.clone();
                                                    move |_| {
                                                        let id = audit_id.clone();
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
                                        {version.map(|version| view! {
                                            <CardVersionActions
                                                audit_id=aid.clone()
                                                version=version
                                                current=current_card_version
                                                expanded=expanded_version
                                                restoring=restoring_version
                                                restore_error=restore_error
                                                reload=reload
                                            />
                                        })}
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

/// Preview / restore controls for one restorable card version in the drawer.
///
/// Pulled out of `HistoryPanel`'s row rendering rather than left inline. A
/// Leptos `view!` compiles to one deeply nested generic type, and this markup
/// sat inside `Show > aside > ul > For > li` — deep enough that adding to it
/// pushed the frontend crate's compile time from minutes into the tens of
/// minutes. A component boundary caps the nesting so this block is type-checked
/// and monomorphised on its own.
#[component]
fn CardVersionActions(
    /// Audit row this version came from; identifies it for preview + restore.
    audit_id: String,
    /// Body and tags captured by that row.
    version: CardVersion,
    /// The row whose content the card currently has, if any.
    current: Signal<Option<(String, CardVersion)>>,
    /// Audit id whose preview is expanded, shared across rows so only one opens.
    expanded: RwSignal<Option<String>>,
    /// Audit id currently being restored, used to disable its button.
    restoring: RwSignal<Option<String>>,
    /// Drawer-level error banner text.
    restore_error: RwSignal<Option<String>>,
    /// Re-fetches the history list after a successful restore.
    reload: Callback<()>,
) -> AnyView {
    let preview_aid = audit_id.clone();
    let preview_toggle_aid = audit_id.clone();
    let preview_label_aid = audit_id.clone();
    let preview_show_aid = audit_id.clone();
    let restore_aid = audit_id.clone();
    let restore_request_aid = audit_id.clone();
    let current_aid = audit_id;
    let version_for_match = version.clone();
    let version_for_title = version.clone();
    let preview_body = version.body;
    // Built once, up front: the preview is static, so the chips need no closure.
    let preview_tag_chips = (!version.tags.is_empty()).then(|| {
        let chips = version
            .tags
            .iter()
            .map(|tag| view! { <span class="tag-chip">{format!("#{tag}")}</span> })
            .collect::<Vec<_>>();
        view! { <div class="history-version-tags">{chips}</div> }
    });

    view! {
        <div class="history-version-actions">
            <button
                type="button"
                class="btn btn-history-version"
                aria-expanded=move || {
                    (expanded.get().as_deref() == Some(preview_aid.as_str())).to_string()
                }
                on:click=move |_| {
                    expanded.update(|open| {
                        if open.as_deref() == Some(preview_toggle_aid.as_str()) {
                            *open = None;
                        } else {
                            *open = Some(preview_toggle_aid.clone());
                        }
                    });
                }
            >
                {move || {
                    if expanded.get().as_deref() == Some(preview_label_aid.as_str()) {
                        "Hide preview"
                    } else {
                        "Preview"
                    }
                }}
            </button>
            <Show
                when=move || {
                    current.get().as_ref().is_some_and(|(id, _)| id == &current_aid)
                }
                fallback=|| ()
            >
                <span class="history-current">"Current"</span>
            </Show>
            <button
                type="button"
                class="btn btn-restore btn-history-version"
                disabled=move || {
                    current
                        .get()
                        .as_ref()
                        .is_some_and(|(_, current)| current == &version_for_match)
                        || restoring.get().as_deref() == Some(restore_aid.as_str())
                }
                title=move || {
                    if current
                        .get()
                        .as_ref()
                        .is_some_and(|(_, current)| current == &version_for_title)
                    {
                        "This version is already current"
                    } else {
                        "Restore this body and tags"
                    }
                }
                on:click=move |_| {
                    let id = restore_request_aid.clone();
                    restoring.set(Some(id.clone()));
                    restore_error.set(None);
                    wasm_bindgen_futures::spawn_local(async move {
                        match crate::api::restore_audit_entry(&id).await {
                            Ok(_) => reload.run(()),
                            Err(err) => {
                                leptos::logging::error!("content restore failed: {err}");
                                restore_error.set(Some(
                                    "Could not restore this version. Refresh and try again."
                                        .to_string(),
                                ));
                            }
                        }
                        restoring.set(None);
                    });
                }
            >"Restore version"</button>
        </div>
        <Show
            when=move || { expanded.get().as_deref() == Some(preview_show_aid.as_str()) }
            fallback=|| ()
        >
            <div class="history-version-preview">
                {preview_tag_chips.clone()}
                <StaticMarkdownPreview
                    body=preview_body.clone()
                    class="card-markdown history-version-markdown"
                />
            </div>
        </Show>
    }
    .into_any()
}

/// The card **content** — body plus tags — one audit row captured, or `None`
/// when the row isn't a restorable card version.
///
/// Mirrors `backend::audit::card_content_version`, which decides what a restore
/// actually writes; the two must agree or the drawer would offer to restore
/// something different from what it previews.
fn card_content_version(entry: &shared::AuditLogEntry) -> Option<CardVersion> {
    if entry.entity_type != "card"
        || !matches!(
            entry.action.as_str(),
            "create" | "baseline" | "update" | "restore"
        )
    {
        return None;
    }
    let snapshot = entry.snapshot_after.as_ref()?;
    let body = snapshot.get("body")?.as_str()?.to_string();
    // Rows predating tags simply have no `tags` key — that card had no tags.
    let tags = snapshot
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(CardVersion { body, tags })
}

/// One restorable snapshot of a card's content.
#[derive(Clone, PartialEq, Eq)]
struct CardVersion {
    body: String,
    tags: Vec<String>,
}

// ── JS bridge helpers ────────────────────────────────────────────────────
//
// These wrap the small slice of `js_sys` / `Intl.DateTimeFormat` that the
// pure helpers in `shared::history` cannot reach. Kept here (rather than
// in the shared crate) so the shared crate stays target-agnostic and can
// be tested with `cargo test -p shared --lib` from CI.

fn current_now_ms() -> i64 {
    js_sys::Date::new_0().get_time() as i64
}

/// Local UTC offset in minutes, **east-positive** (BST = +60, PST = -480).
/// `Date.getTimezoneOffset()` returns the opposite sign convention; we
/// negate it so the value matches the contract of `format_history_time`.
fn current_local_offset_minutes() -> i32 {
    -(js_sys::Date::new_0().get_timezone_offset() as i32)
}

/// Short timezone abbreviation for the user's locale (e.g. "BST", "PDT").
/// Sourced from `Intl.DateTimeFormat` so we don't ship a tz table.
/// Returns an empty string when `Intl` is unavailable in the host engine
/// (the tooltip falls back to the un-suffixed form in that case).
fn current_tz_label() -> String {
    use js_sys::{Array, Function, Object, Reflect};
    use wasm_bindgen::JsValue;

    let opts = Object::new();
    if Reflect::set(
        &opts,
        &JsValue::from_str("timeZoneName"),
        &JsValue::from_str("short"),
    )
    .is_err()
    {
        return String::new();
    }

    // Resolve `Intl.DateTimeFormat` constructor dynamically; using
    // `js_sys::Intl::DateTimeFormat` directly would couple us to a
    // particular wasm-bindgen feature subset and the Reflect path is
    // sufficient and stable.
    let global = js_sys::global();
    let intl = match Reflect::get(&global, &JsValue::from_str("Intl")) {
        Ok(v) if !v.is_undefined() => v,
        _ => return String::new(),
    };
    let dtf_ctor = match Reflect::get(&intl, &JsValue::from_str("DateTimeFormat")) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let dtf_ctor: Function = match dtf_ctor.dyn_into::<Function>() {
        Ok(f) => f,
        Err(_) => return String::new(),
    };

    let args = Array::of2(&JsValue::UNDEFINED, &opts.into());
    let dtf = match Reflect::construct(&dtf_ctor, &args) {
        Ok(o) => o,
        Err(_) => return String::new(),
    };

    let format_to_parts = match Reflect::get(&dtf, &JsValue::from_str("formatToParts")) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let format_to_parts: Function = match format_to_parts.dyn_into::<Function>() {
        Ok(f) => f,
        Err(_) => return String::new(),
    };

    let date = js_sys::Date::new_0();
    let parts = match format_to_parts.call1(&dtf, &date.into()) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let parts: Array = match parts.dyn_into::<Array>() {
        Ok(a) => a,
        Err(_) => return String::new(),
    };

    for i in 0..parts.length() {
        let part = parts.get(i);
        let ty = Reflect::get(&part, &JsValue::from_str("type"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        if ty == "timeZoneName" {
            return Reflect::get(&part, &JsValue::from_str("value"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();
        }
    }
    String::new()
}

/// Parse a Surreal-emitted timestamp string (e.g. `"d'2026-05-07T01:27:04.823026281Z'"`)
/// into milliseconds since the Unix epoch.
///
/// Strips the `d'…'` wrapper first, then trims fractional seconds to three
/// digits (JS `Date.parse` is permissive about trailing fractional digits
/// in modern engines, but trimming keeps the input portable).
///
/// Falls back to the current time when parsing fails so a malformed row
/// still renders rather than blowing up the whole drawer.
fn parse_audit_ts(raw: &str) -> i64 {
    let inner = shared::history::strip_surreal_wrapper(raw);
    let normalised = trim_fractional_seconds(inner);
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(&normalised));
    let ms = date.get_time();
    if ms.is_nan() {
        current_now_ms()
    } else {
        ms as i64
    }
}

/// Truncate the fractional-seconds part of an ISO-8601 string to at most
/// three digits, e.g. `"2026-05-07T01:27:04.823026281Z"` →
/// `"2026-05-07T01:27:04.823Z"`. Returns the input unchanged when there's
/// no fractional part or no trailing `Z`.
fn trim_fractional_seconds(s: &str) -> String {
    let Some(dot_at) = s.find('.') else {
        return s.to_string();
    };
    let after_dot = &s[dot_at + 1..];
    let Some(z_at) = after_dot.find('Z') else {
        return s.to_string();
    };
    let fractional = &after_dot[..z_at];
    let truncated: String = fractional.chars().take(3).collect();
    format!(
        "{prefix}.{truncated}{suffix}",
        prefix = &s[..dot_at],
        suffix = &after_dot[z_at..]
    )
}
