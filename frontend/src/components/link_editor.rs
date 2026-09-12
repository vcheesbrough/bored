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
use leptos_router::hooks::{use_navigate, use_params_map};
use leptos_router::NavigateOptions;

use crate::links::BoardLinkIndex;
use crate::search::BoardCardIndex;

/// How many cards the picker offers at once.
const MAX_SUGGESTIONS: usize = 6;

/// Longest card title shown on a chip, in characters. A chip is a label, not
/// a preview; the full title is one click away.
const MAX_TITLE_CHARS: usize = 32;

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
    let params = use_params_map();
    let navigate = StoredValue::new(use_navigate());

    // The chip describes the *other* card, whichever end of the link that is.
    let (other_id, other_number) = match side {
        Side::Before => (link.predecessor_id.clone(), link.predecessor_number),
        Side::After => (link.successor_id.clone(), link.successor_number),
    };
    // Reactive: the title fills in once the other card's column has loaded.
    let label = Signal::derive(move || card_label(cards, &other_id, other_number));

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
        if editing_reason.get() {
            if let Some(el) = reason_input_ref.get() {
                let _ = el.focus();
            }
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
                id: c.id,
                number: c.number,
                label: format!("#{} {}", c.number, truncate(&title, MAX_TITLE_CHARS)),
            })
            .collect();
        // Cards arrive in column order; numeric order is what a user scanning
        // for `#12` expects.
        found.sort_by_key(|c| c.number);
        found.truncate(MAX_SUGGESTIONS);
        found
    });

    let popup_open = Signal::derive(move || input_focused.get() && !suggestions.get().is_empty());

    let commit = move |candidate: Candidate| {
        let Some(me) = card_id.get_untracked() else {
            return;
        };
        draft.set(String::new());
        active.set(None);
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
}
