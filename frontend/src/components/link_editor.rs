//! Predecessor/successor link editing for one card.
//!
//! Two groups — "before" and "after" — each listing the linked cards as chips
//! and ending in a picker that adds another. The chips and the picker read the
//! board-level [`BoardLinkIndex`], so the inline card and the modal edit the
//! same links and a change made anywhere (including over SSE) shows up in
//! both without either being remounted.
//!
//! Every component here returns `AnyView`: a `view!` compiles to one nested
//! generic type, and a component boundary caps that nesting so each piece is
//! type-checked on its own rather than as part of the card it sits in.

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_params_map};

use crate::links::BoardLinkIndex;
use crate::recent::RecentPicks;
use crate::search::BoardCardIndex;

/// How many cards the picker offers at once.
const MAX_SUGGESTIONS: usize = 6;

/// Longest card title shown on a chip, in characters. A chip is a label, not
/// a preview; the full title is one click away.
const MAX_TITLE_CHARS: usize = 32;

/// Longest column name shown as a chip prefix, in characters. Shorter than a
/// title: the prefix is a status word ("todo", "in progress"), and the card it
/// qualifies has to stay the part you read first.
const MAX_COLUMN_CHARS: usize = 16;

/// Which end of the card a group edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// Predecessors — the cards that come before this one.
    Before,
    /// Successors — the cards that come after this one.
    After,
}

impl Side {
    fn direction(self) -> shared::LinkDirection {
        match self {
            Side::Before => shared::LinkDirection::Predecessor,
            Side::After => shared::LinkDirection::Successor,
        }
    }

    fn attr(self) -> &'static str {
        match self {
            Side::Before => "before",
            Side::After => "after",
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// `#12 Title…` for a card on the board, or a bare `#12` when the card is not
/// in the index (its column has not loaded yet, or it has just been deleted).
fn card_label(cards: Option<BoardCardIndex>, card_id: &str, number: u32) -> String {
    let title = cards
        .and_then(|index| index.all_cards().into_iter().find(|c| c.id == card_id))
        .map(|c| shared::history::card_title_from_body(&c.body));
    match title {
        Some(title) => format!("#{number} {}", truncate(&title, MAX_TITLE_CHARS)),
        None => format!("#{number}"),
    }
}

/// The name of the column holding `card_id`, or `None` when the card or its
/// column is not in the client's view of the board.
///
/// A whitespace-only name counts as missing too: nothing stops the API from
/// creating one, and an empty prefix still draws its separator.
///
/// Two hops — card → `column_id` → column name — and either can miss: a card
/// whose column has not loaded yet (or that was just deleted) is not in
/// `cards`, and a card that has just moved can name a column the client has
/// not seen. Both mean "no prefix" rather than a placeholder, so the chip
/// degrades to the same bare `#12` that a missing title already produces.
///
/// Takes plain slices rather than the signals themselves so the lookup is
/// testable without a reactive runtime; the caller does the reading, which is
/// also what makes the prefix reactive.
fn column_name_for(
    cards: &[shared::Card],
    columns: &[shared::Column],
    card_id: &str,
) -> Option<String> {
    let column_id = &cards.iter().find(|c| c.id == card_id)?.column_id;
    let name = &columns.iter().find(|col| &col.id == column_id)?.name;
    // A blank name is treated as no name at all: column creation over the API
    // does not reject one (the UI rename path does), and `Some("")` would
    // render an empty prefix whose `::after` separator still fires, giving the
    // user a leading "· #12 Title".
    if name.trim().is_empty() {
        return None;
    }
    Some(truncate(name, MAX_COLUMN_CHARS))
}

/// Editable before/after link groups for one card.
///
/// Renders nothing outside a board (no [`BoardLinkIndex`]), so the card
/// components can mount it unconditionally.
#[component]
pub fn LinkEditor(
    /// The card being edited. `None` while the modal has no card.
    card_id: Signal<Option<String>>,
) -> AnyView {
    let Some(links) = use_context::<BoardLinkIndex>() else {
        return ().into_any();
    };
    // One error line for the whole editor: only the most recent failed action
    // matters, and it clears the moment another action succeeds.
    let error: RwSignal<Option<String>> = RwSignal::new(None);

    let predecessors = Signal::derive(move || {
        card_id
            .try_get()
            .flatten()
            .map(|id| links.predecessors_of(&id))
            .unwrap_or_default()
    });
    let successors = Signal::derive(move || {
        card_id
            .try_get()
            .flatten()
            .map(|id| links.successors_of(&id))
            .unwrap_or_default()
    });

    view! {
        <div
            class="link-editor"
            // Clicks inside the editor must not reach the card behind it, which
            // would flip an expanded card into editing mode.
            on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()
        >
            <LinkGroup side=Side::Before card_id=card_id links=predecessors error=error />
            <LinkGroup side=Side::After card_id=card_id links=successors error=error />
            <Show when=move || error.get().is_some() fallback=|| ()>
                <p class="link-editor-error" role="alert">
                    {move || error.get().unwrap_or_default()}
                </p>
            </Show>
        </div>
    }
    .into_any()
}

/// One side of the editor: a label, the chips for that side, and a picker.
#[component]
fn LinkGroup(
    side: Side,
    card_id: Signal<Option<String>>,
    links: Signal<Vec<shared::CardLink>>,
    error: RwSignal<Option<String>>,
) -> AnyView {
    let (glyph, label, title) = match side {
        Side::Before => ("↑", "before", "Cards that come before this one"),
        Side::After => ("↓", "after", "Cards that come after this one"),
    };
    view! {
        <div class="link-group" data-side=side.attr()>
            <span class="link-group-label" title=title>
                <span aria-hidden="true">{glyph}</span>
                " "
                {label}
            </span>
            <For
                each=move || links.get()
                // Keyed on the reason as well as the id: a chip is built from a
                // plain value, so a reason change has to remount it to show.
                key=|link: &shared::CardLink| (link.id.clone(), link.reason.clone())
                children=move |link: shared::CardLink| {
                    view! { <LinkChip link=link side=side error=error /> }
                }
            />
            <LinkPicker side=side card_id=card_id error=error />
        </div>
    }
    .into_any()
}

/// One linked card: its label (a button that opens the card), the reason
/// when there is one, and controls to edit the reason or remove the link.
#[component]
fn LinkChip(link: shared::CardLink, side: Side, error: RwSignal<Option<String>>) -> AnyView {
    let links = expect_context::<BoardLinkIndex>();
    let cards = use_context::<BoardCardIndex>();
    // Optional for the same reason `cards` is: the editor mounts wherever a
    // card does, and only a board provides the column list.
    let columns = use_context::<RwSignal<Vec<RwSignal<shared::Column>>>>();
    let params = use_params_map();
    let navigate = StoredValue::new(use_navigate());

    // The chip describes the *other* card, whichever end of the link that is.
    let (other_id, other_number) = match side {
        Side::Before => (link.predecessor_id.clone(), link.predecessor_number),
        Side::After => (link.successor_id.clone(), link.successor_number),
    };
    // Reactive: the title fills in once the other card's column has loaded.
    let id_for_column = other_id.clone();
    let label = Signal::derive(move || card_label(cards, &other_id, other_number));
    // Which column the linked card sits in, shown before its number. Reading
    // the cards and the columns through their signals is what keeps this
    // current: the prefix fills in when a column finishes loading and follows
    // the card when it is moved, locally or over SSE.
    // A `Memo`, not a `Signal::derive`: the lookup walks every card on the
    // board (and `all_cards()` clones each one), so it must run at most once
    // per dependency change rather than once per read, and it must not
    // re-render the prefix when an unrelated card edit leaves the name the
    // same.
    let column = Memo::new(move |_| {
        let (Some(cards), Some(columns)) = (cards, columns) else {
            return None;
        };
        // The column reads are `try_get`: a chip can outlive the board view it
        // read them from by a frame, and a disposed column should mean "no
        // prefix" rather than a panic. The cards side goes through
        // `all_cards()`, which reads the index and each column's list with
        // plain `get`, so it carries the same disposal exposure `card_label`
        // already has — guarding it belongs in `BoardCardIndex`, for both
        // call sites at once.
        let columns: Vec<shared::Column> = columns
            .try_get()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|col| col.try_get())
            .collect();
        column_name_for(&cards.all_cards(), &columns, &id_for_column)
    });

    // Held in `StoredValue`s so the handlers below capture only `Copy`
    // handles: the same closure is then usable from several event attributes
    // without cloning it per attribute.
    let link_id = StoredValue::new(link.id.clone());
    let saved_reason = StoredValue::new(link.reason.clone());
    let reason_for_title = link.reason.clone().unwrap_or_default();
    let reason_for_display = link.reason.clone();
    let editing_reason = RwSignal::new(false);
    let removing = RwSignal::new(false);
    let draft = RwSignal::new(link.reason.clone().unwrap_or_default());
    let reason_input_ref = NodeRef::<leptos::html::Input>::new();

    Effect::new(move |_| {
        if editing_reason.get()
            && let Some(el) = reason_input_ref.get()
        {
            let _ = el.focus();
        }
    });

    let open_card = move |e: leptos::ev::MouseEvent| {
        e.stop_propagation();
        let slug = params.with_untracked(|p| p.get("slug").unwrap_or_default());
        let url = format!("/boards/{slug}?card={other_number}");
        navigate.with_value(|nav| nav(&url, NavigateOptions::default()));
    };

    let save_reason = move || {
        // Enter closes the box, and closing it unmounts the input, which fires
        // `blur` — the second caller of this closure. Only the first call per
        // edit may submit, or the same reason goes to the server twice at once.
        if !editing_reason.get_untracked() {
            return;
        }
        editing_reason.set(false);
        let value = draft.get_untracked();
        // The server treats an empty reason as none; compare the same way so
        // clearing an already-empty box is not a request.
        let unchanged = match shared::links::normalize_reason(Some(&value)) {
            Ok(next) => saved_reason.with_value(|current| *current == next),
            Err(_) => false,
        };
        if unchanged {
            return;
        }
        let id = link_id.get_value();
        wasm_bindgen_futures::spawn_local(async move {
            let req = shared::UpdateCardLinkRequest {
                reason: Some(value),
            };
            match crate::api::update_card_link(&id, req).await {
                Ok(updated) => {
                    links.replace(updated);
                    error.set(None);
                }
                Err(e) => {
                    leptos::logging::error!("link reason save failed: {e}");
                    error.set(Some(e.message));
                }
            }
        });
    };

    let remove = move || {
        // Guards re-entry the same way `save_reason` does: the chip stays
        // mounted (and its × button armed) until the response handler below
        // removes it, so without this a double click could send a second
        // DELETE that races the first and surfaces a spurious 404 error under
        // an unlink that already succeeded.
        if removing.get_untracked() {
            return;
        }
        removing.set(true);
        let id = link_id.get_value();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::api::delete_card_link(&id).await {
                Ok(()) => {
                    links.remove(&id);
                    error.set(None);
                }
                Err(e) => {
                    leptos::logging::error!("unlink failed: {e}");
                    error.set(Some(e.message));
                    removing.set(false);
                }
            }
        });
    };

    view! {
        <span class="link-chip" title=reason_for_title>
            // A sibling of the card button rather than part of its label, so
            // the column name is not part of the link text and assertions on
            // `.link-chip-card` still read just `#N Title`. Mapped from a
            // single read instead of a `Show` wrapping a second one: `None`
            // renders nothing either way, and one read means one board walk.
            {move || {
                column
                    .get()
                    .map(|name| view! { <span class="link-chip-column">{name}</span> })
            }}
            <button
                type="button"
                class="link-chip-card"
                title=format!("Open card #{other_number}")
                on:click=open_card
            >{move || label.get()}</button>
            <Show when=move || !editing_reason.get() fallback=|| ()>
                {reason_for_display.clone().map(|r| view! {
                    <span class="link-chip-reason">{r}</span>
                })}
            </Show>
            <Show when=move || editing_reason.get() fallback=|| ()>
                <input
                    node_ref=reason_input_ref
                    class="link-reason-input"
                    type="text"
                    placeholder="reason"
                    aria-label=format!("Reason for link to #{other_number}")
                    maxlength=shared::links::MAX_REASON_CHARS.to_string()
                    prop:value=move || draft.get()
                    on:input=move |ev| draft.set(event_target_value(&ev))
                    on:blur=move |_| save_reason()
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        match ev.key().as_str() {
                            "Enter" => {
                                ev.prevent_default();
                                ev.stop_propagation();
                                save_reason();
                            }
                            "Escape" => {
                                // Cancel: put the saved reason back and close,
                                // without letting Escape collapse the card.
                                ev.stop_propagation();
                                draft.set(saved_reason.get_value().unwrap_or_default());
                                editing_reason.set(false);
                            }
                            _ => {}
                        }
                    }
                />
            </Show>
            <button
                type="button"
                class="link-chip-btn link-chip-edit"
                aria-label=format!("Edit reason for link to #{other_number}")
                title="Edit reason"
                // `mousedown` + `preventDefault` so the picker input next to
                // the chip does not blur first and swallow the click.
                on:mousedown=move |e: leptos::ev::MouseEvent| {
                    e.prevent_default();
                    e.stop_propagation();
                    // A second click while the box is open commits it, the
                    // same as Enter — never a silent discard of typed text.
                    if editing_reason.get_untracked() {
                        save_reason();
                    } else {
                        editing_reason.set(true);
                    }
                }
            >"✎"</button>
            <button
                type="button"
                class="link-chip-btn link-chip-remove"
                aria-label=format!("Remove link to #{other_number}")
                title="Remove link"
                prop:disabled=move || removing.get()
                on:mousedown=move |e: leptos::ev::MouseEvent| {
                    e.prevent_default();
                    e.stop_propagation();
                    remove();
                }
            >"×"</button>
        </span>
    }
    .into_any()
}

/// A card the picker may offer.
#[derive(Clone, PartialEq, Eq)]
struct Candidate {
    id: String,
    number: u32,
    label: String,
    /// Sort key for how recently the card itself changed — see
    /// [`crate::recent::recency_key`], which is what built it.
    recency: String,
}

/// Order the picker's candidates most-recently-used first and cut the list to
/// [`MAX_SUGGESTIONS`].
///
/// Cards the user has linked to before come first, in the order they were
/// picked; the rest follow by how recently each card changed. Numeric order —
/// what this used to sort by outright — survives only as the tie-break, for
/// the boards where nothing has been picked and the timestamps agree.
///
/// Truncation happens *after* the sort, so a recently used card can push a
/// lower-numbered one off the end. That is the point: the six rows are spent
/// on what the user is likely to want rather than on the six oldest cards.
fn order_candidates(mut found: Vec<Candidate>, recent_card_ids: &[String]) -> Vec<Candidate> {
    found.sort_by(|a, b| {
        crate::recent::rank_of(recent_card_ids, &a.id)
            .cmp(&crate::recent::rank_of(recent_card_ids, &b.id))
            .then_with(|| b.recency.cmp(&a.recency))
            .then_with(|| a.number.cmp(&b.number))
    });
    found.truncate(MAX_SUGGESTIONS);
    found
}

/// True when `typed` narrows to this card: a digit run matches the number as
/// a prefix (`1` offers `#1`, `#12`, `#100`), anything else matches the title
/// case-insensitively. Empty text matches everything, so focusing the box
/// browses the board.
fn candidate_matches(number: u32, title: &str, typed: &str) -> bool {
    let typed = typed.trim().trim_start_matches('#');
    if typed.is_empty() {
        return true;
    }
    if typed.chars().all(|c| c.is_ascii_digit()) {
        return number.to_string().starts_with(typed);
    }
    title.to_lowercase().contains(&typed.to_lowercase())
}

/// Text box plus popup that adds a link on one side of the card.
///
/// The popup only ever offers cards the server would accept: not this card,
/// not one already linked in either direction, and not one that would close
/// a loop. The server still checks — another client may have changed the
/// graph since the list was built — and its refusal lands in `error`.
#[component]
fn LinkPicker(
    side: Side,
    card_id: Signal<Option<String>>,
    error: RwSignal<Option<String>>,
) -> AnyView {
    let links = expect_context::<BoardLinkIndex>();
    let cards = use_context::<BoardCardIndex>();
    // Absent in the card-modal-only tests and anywhere the picker is mounted
    // outside a board; without it the list simply keeps its recency order.
    let recent = use_context::<RecentPicks>();

    let draft = RwSignal::new(String::new());
    let active: RwSignal<Option<usize>> = RwSignal::new(None);
    let input_focused = RwSignal::new(false);
    let input_ref = NodeRef::<leptos::html::Input>::new();

    let suggestions = Signal::derive(move || {
        let Some(cards) = cards else {
            return Vec::new();
        };
        let Some(me) = card_id.get() else {
            return Vec::new();
        };
        let typed = draft.get();
        let mut found: Vec<Candidate> = cards
            .all_cards()
            .into_iter()
            .filter(|c| c.id != me)
            .filter(|c| !links.are_linked(&me, &c.id))
            .filter(|c| {
                let (pred, succ) = match side {
                    Side::Before => (c.id.as_str(), me.as_str()),
                    Side::After => (me.as_str(), c.id.as_str()),
                };
                !links.would_create_cycle(pred, succ)
            })
            .map(|c| {
                let title = shared::history::card_title_from_body(&c.body);
                (c, title)
            })
            .filter(|(c, title)| candidate_matches(c.number, title, &typed))
            .map(|(c, title)| Candidate {
                number: c.number,
                label: format!("#{} {}", c.number, truncate(&title, MAX_TITLE_CHARS)),
                recency: crate::recent::recency_key(&c.updated_at),
                id: c.id,
            })
            .collect();
        // Cards arrive in column order; the user's own link history, then
        // board activity, is what puts the likely one within reach.
        let picked = recent.map(|r| r.cards.get()).unwrap_or_default();
        found = order_candidates(found, &picked);
        found
    });

    let popup_open = Signal::derive(move || input_focused.get() && !suggestions.get().is_empty());

    let commit = move |candidate: Candidate| {
        let Some(me) = card_id.get_untracked() else {
            return;
        };
        draft.set(String::new());
        active.set(None);
        // Recorded on the pick, not on the server's reply: the user picked
        // this card whether or not the link survives validation, and that is
        // what the next list should lead with.
        if let Some(recent) = recent {
            recent.record_card(&candidate.id);
        }
        wasm_bindgen_futures::spawn_local(async move {
            let req = shared::CreateCardLinkRequest {
                direction: side.direction(),
                other_card_id: candidate.id,
                reason: None,
            };
            match crate::api::create_card_link(&me, req).await {
                Ok(link) => {
                    links.insert_absent(link);
                    error.set(None);
                }
                Err(e) => {
                    leptos::logging::error!("link create failed: {e}");
                    error.set(Some(e.message));
                }
            }
        });
    };

    let placeholder = match side {
        Side::Before => "+ before",
        Side::After => "+ after",
    };
    let aria_label = match side {
        Side::Before => "Add a card that comes before",
        Side::After => "Add a card that comes after",
    };

    view! {
        <span class="link-picker">
            <input
                node_ref=input_ref
                class="link-picker-input"
                type="text"
                placeholder=placeholder
                aria-label=aria_label
                prop:value=move || draft.get()
                on:input=move |ev| {
                    draft.set(event_target_value(&ev));
                    active.set(None);
                }
                on:focus=move |_| input_focused.set(true)
                on:blur=move |_| {
                    input_focused.set(false);
                    // A link is a deliberate action, so unlike a tag nothing is
                    // committed on blur — the half-typed text is just dropped.
                    draft.set(String::new());
                    active.set(None);
                }
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    match ev.key().as_str() {
                        "Enter" => {
                            ev.prevent_default();
                            ev.stop_propagation();
                            // Enter takes the highlighted row, or the only row
                            // when the text has narrowed the list to one.
                            let list = suggestions.get_untracked();
                            let picked = match active.get_untracked() {
                                Some(i) => list.get(i).cloned(),
                                None if list.len() == 1 => list.first().cloned(),
                                None => None,
                            };
                            if let Some(candidate) = picked {
                                commit(candidate);
                            }
                        }
                        "Escape" => {
                            // Never let Escape reach the card and collapse it
                            // while the user is mid-pick.
                            ev.stop_propagation();
                            if draft.get_untracked().is_empty() {
                                if let Some(el) = input_ref.get_untracked() {
                                    let _ = el.blur();
                                }
                            } else {
                                draft.set(String::new());
                                active.set(None);
                            }
                        }
                        "ArrowDown" => {
                            let len = suggestions.get_untracked().len();
                            if len > 0 {
                                ev.prevent_default();
                                active.update(|i| {
                                    *i = Some(match *i {
                                        Some(current) => (current + 1) % len,
                                        None => 0,
                                    });
                                });
                            }
                        }
                        "ArrowUp" => {
                            let len = suggestions.get_untracked().len();
                            if len > 0 {
                                ev.prevent_default();
                                active.update(|i| {
                                    *i = Some(match *i {
                                        Some(0) | None => len - 1,
                                        Some(current) => current - 1,
                                    });
                                });
                            }
                        }
                        _ => {}
                    }
                }
            />
            <Show when=move || popup_open.get() fallback=|| ()>
                <ul class="link-suggestions" role="listbox">
                    <For
                        each=move || {
                            suggestions.get().into_iter().enumerate().collect::<Vec<_>>()
                        }
                        // Keyed on position as well as id so a row that moves
                        // does not keep a stale `index` in its closures.
                        key=|(index, c): &(usize, Candidate)| format!("{index}:{}", c.id)
                        children=move |(index, candidate): (usize, Candidate)| {
                            let label = candidate.label.clone();
                            let picked = candidate.clone();
                            view! {
                                <li>
                                    <button
                                        type="button"
                                        class="link-suggestion"
                                        class:link-suggestion-active=move || active.get() == Some(index)
                                        role="option"
                                        aria-selected=move || (active.get() == Some(index)).to_string()
                                        // `mousedown`, not `click`: the input's
                                        // blur fires first on click and would
                                        // tear the popup down before the
                                        // selection was read.
                                        on:mousedown=move |e: leptos::ev::MouseEvent| {
                                            e.prevent_default();
                                            e.stop_propagation();
                                            commit(picked.clone());
                                        }
                                    >{label}</button>
                                </li>
                            }
                        }
                    />
                </ul>
            </Show>
        </span>
    }
    .into_any()
}

/// One pill per linked card for a collapsed card: an arrow for the side plus
/// the other card's `#number`. Renders nothing when the card has no links, so
/// an unlinked board looks exactly as it did before.
#[component]
pub fn LinkBadges(card_id: Signal<Option<String>>) -> AnyView {
    let Some(links) = use_context::<BoardLinkIndex>() else {
        return ().into_any();
    };
    // `try_get`: the collapsed card can be asked to render once more after the
    // search filter disposed its signals (see `CardItem`).
    let predecessors = Signal::derive(move || {
        card_id
            .try_get()
            .flatten()
            .map(|id| links.predecessors_of(&id))
            .unwrap_or_default()
    });
    let successors = Signal::derive(move || {
        card_id
            .try_get()
            .flatten()
            .map(|id| links.successors_of(&id))
            .unwrap_or_default()
    });
    let has_any =
        Signal::derive(move || !predecessors.get().is_empty() || !successors.get().is_empty());

    view! {
        <Show when=move || has_any.get() fallback=|| ()>
            <span class="card-link-badges">
                <For
                    each=move || predecessors.get()
                    key=|link: &shared::CardLink| link.id.clone()
                    children=move |link: shared::CardLink| {
                        let number = link.predecessor_number;
                        let title = link.reason.clone().unwrap_or_else(|| format!("#{number}"));
                        view! {
                            <span class="link-badge link-badge-before" title=title>
                                <span aria-hidden="true">"↑"</span>
                                <span class="link-badge-number">{format!("#{number}")}</span>
                            </span>
                        }
                    }
                />
                <For
                    each=move || successors.get()
                    key=|link: &shared::CardLink| link.id.clone()
                    children=move |link: shared::CardLink| {
                        let number = link.successor_number;
                        let title = link.reason.clone().unwrap_or_else(|| format!("#{number}"));
                        view! {
                            <span class="link-badge link-badge-after" title=title>
                                <span aria-hidden="true">"↓"</span>
                                <span class="link-badge-number">{format!("#{number}")}</span>
                            </span>
                        }
                    }
                />
            </span>
        </Show>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_matches_every_card() {
        assert!(candidate_matches(12, "Anything", ""));
        assert!(candidate_matches(12, "Anything", "   "));
    }

    #[test]
    fn digits_match_the_number_as_a_prefix() {
        assert!(candidate_matches(12, "Title", "1"));
        assert!(candidate_matches(12, "Title", "12"));
        assert!(candidate_matches(12, "Title", "#12"));
        assert!(!candidate_matches(12, "Title", "2"));
        assert!(!candidate_matches(12, "Title", "123"));
    }

    #[test]
    fn text_matches_the_title_case_insensitively() {
        assert!(candidate_matches(12, "Deploy preview", "PREV"));
        assert!(!candidate_matches(12, "Deploy preview", "release"));
    }

    #[test]
    fn digits_never_match_a_title_that_contains_them() {
        // `2` is a number prefix, not a title substring.
        assert!(!candidate_matches(15, "Phase 2", "2"));
    }

    /// A card carrying only the fields `column_name_for` reads.
    fn card(id: &str, column_id: &str) -> shared::Card {
        shared::Card {
            id: id.to_string(),
            column_id: column_id.to_string(),
            number: 1,
            body: String::new(),
            position: 0,
            tags: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
            last_edited_by: None,
        }
    }

    fn column(id: &str, name: &str) -> shared::Column {
        shared::Column {
            id: id.to_string(),
            board_id: "board".to_string(),
            name: name.to_string(),
            position: 0,
            created_at: String::new(),
            updated_at: String::new(),
            last_edited_by: None,
        }
    }

    #[test]
    fn resolves_the_column_of_a_card_on_the_board() {
        let cards = vec![card("card-a", "col-1"), card("card-b", "col-2")];
        let columns = vec![column("col-1", "todo"), column("col-2", "done")];
        assert_eq!(
            column_name_for(&cards, &columns, "card-b"),
            Some("done".to_string())
        );
    }

    fn candidate(number: u32, updated_at: &str) -> Candidate {
        Candidate {
            id: format!("card-{number}"),
            number,
            label: format!("#{number} Title"),
            recency: crate::recent::recency_key(updated_at),
        }
    }

    fn numbers(candidates: &[Candidate]) -> Vec<u32> {
        candidates.iter().map(|c| c.number).collect()
    }

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn candidates_lead_with_the_most_recently_linked_card() {
        let found = vec![
            candidate(3, "2026-09-14T10:00:00Z"),
            candidate(11, "2026-09-13T10:00:00Z"),
            candidate(7, "2026-09-12T10:00:00Z"),
        ];
        // #7 was linked most recently, then #11; #3 has never been picked.
        let picked = ids(&["card-7", "card-11"]);
        assert_eq!(numbers(&order_candidates(found, &picked)), vec![7, 11, 3]);
    }

    #[test]
    fn unpicked_candidates_order_by_card_recency() {
        let found = vec![
            candidate(3, "2026-09-10T10:00:00Z"),
            candidate(11, "2026-09-14T10:00:00Z"),
            candidate(7, "2026-09-12T10:00:00Z"),
        ];
        assert_eq!(numbers(&order_candidates(found, &[])), vec![11, 7, 3]);
    }

    #[test]
    fn candidates_with_equal_recency_fall_back_to_card_number() {
        // Every card untouched since the same instant — the old numeric order
        // is what is left, so a board that has seen no activity is unchanged.
        let found = vec![
            candidate(11, "2026-09-14T10:00:00Z"),
            candidate(3, "2026-09-14T10:00:00Z"),
            candidate(7, "2026-09-14T10:00:00Z"),
        ];
        assert_eq!(numbers(&order_candidates(found, &[])), vec![3, 7, 11]);
    }

    #[test]
    fn candidates_are_cut_to_the_popup_size_after_sorting() {
        // A recently linked low-priority card keeps its place in the list;
        // truncation drops the least interesting rows, not the highest numbers.
        let found: Vec<Candidate> = (1..=MAX_SUGGESTIONS as u32 + 2)
            .map(|n| candidate(n, "2026-09-14T10:00:00Z"))
            .collect();
        let last = format!("card-{}", MAX_SUGGESTIONS + 2);
        let ordered = order_candidates(found, &ids(&[&last]));
        assert_eq!(ordered.len(), MAX_SUGGESTIONS);
        assert_eq!(ordered[0].number, MAX_SUGGESTIONS as u32 + 2);
    }

    #[test]
    fn no_column_for_a_card_outside_the_index() {
        // Its column has not loaded yet, or the card has just been deleted.
        let columns = vec![column("col-1", "todo")];
        assert_eq!(column_name_for(&[], &columns, "card-a"), None);
    }

    #[test]
    fn no_column_when_the_cards_column_is_unknown() {
        // The card names a column the client has not seen.
        let cards = vec![card("card-a", "col-9")];
        let columns = vec![column("col-1", "todo")];
        assert_eq!(column_name_for(&cards, &columns, "card-a"), None);
    }

    #[test]
    fn no_column_when_the_name_is_blank() {
        // Column creation over the API does not reject a blank name; a blank
        // prefix would render as a bare separator, so treat it as missing.
        let cards = vec![card("card-a", "col-1")];
        for blank in ["", "   ", "\t\n"] {
            let columns = vec![column("col-1", blank)];
            assert_eq!(
                column_name_for(&cards, &columns, "card-a"),
                None,
                "{blank:?} should yield no prefix"
            );
        }
    }

    #[test]
    fn a_long_column_name_is_truncated() {
        let cards = vec![card("card-a", "col-1")];
        let columns = vec![column("col-1", "waiting on review from ops")];
        let name = column_name_for(&cards, &columns, "card-a").expect("column resolves");
        assert_eq!(name.chars().count(), MAX_COLUMN_CHARS);
        assert!(name.ends_with('…'), "{name} should be elided");
    }
}
