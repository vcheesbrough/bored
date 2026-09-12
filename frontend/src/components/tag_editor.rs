use leptos::prelude::*;

use crate::search::BoardCardIndex;

/// How many suggestions the popup offers at once. Enough to be useful, few
/// enough that the list never covers the card being edited.
const MAX_SUGGESTIONS: usize = 6;

/// Editable row of tag chips plus an always-visible `+ tag` input.
///
/// The component never writes to the server itself — it hands the card's
/// **complete new tag list** to `on_change` and lets the owner (the inline card
/// or the modal) decide how to persist it. That matches the wire contract,
/// where `PUT /api/cards/:id` replaces the tag array wholesale.
#[component]
pub fn TagEditor(
    /// The card's current tags. Reactive, so tags arriving over SSE (or from
    /// a history restore) show up without the editor being remounted.
    tags: Signal<Vec<String>>,
    /// Receives the full replacement list whenever the user adds or removes a
    /// tag. Never called when the change would be a no-op.
    on_change: Callback<Vec<String>>,
) -> AnyView {
    let draft = RwSignal::new(String::new());
    // Index into the suggestion list, moved with the arrow keys. `None` means
    // "nothing picked yet", where Enter commits whatever was typed verbatim.
    let active_suggestion: RwSignal<Option<usize>> = RwSignal::new(None);
    let input_ref = NodeRef::<leptos::html::Input>::new();

    // Suggestions come from tags already in use on this board. The editor is
    // usable without the context (e.g. a card rendered outside a board view) —
    // it simply offers no completions in that case.
    let card_index = use_context::<BoardCardIndex>();

    let suggestions = Signal::derive(move || {
        let Some(index) = card_index else {
            return Vec::new();
        };
        let typed = draft.get();
        let typed = typed.trim();
        // An empty prefix matches every tag, which is intended: focusing the
        // input offers the board's tags to browse, and typing narrows them.
        let current = tags.get();
        index
            .all_tags()
            .into_iter()
            // Never suggest a tag the card already carries.
            .filter(|tag| {
                !current
                    .iter()
                    .any(|existing| shared::tags::eq_ignore_case(existing, tag))
            })
            .filter(|tag| shared::tags::starts_with_ignore_case(tag, typed))
            .take(MAX_SUGGESTIONS)
            .collect::<Vec<String>>()
    });

    // The popup belongs to the input, so focus is what opens and closes it.
    // Without this it would hang over every expanded card on a board that has
    // tags, since an empty prefix matches all of them.
    let input_focused = RwSignal::new(false);
    let popup_open = Signal::derive(move || input_focused.get() && !suggestions.get().is_empty());

    // Append `candidate` to the card's tags and publish the result.
    //
    // Normalization runs over the *whole* list so the editor and the server
    // agree on the outcome: duplicates collapse case-insensitively, a pasted
    // `bug urgent` becomes two chips, and a leading `#` is dropped. A list the
    // server would reject (an over-long tag, too many tags) is left uncommitted
    // with the text still in the box, so the user can see and fix it.
    let commit = move |candidate: String| {
        if candidate.trim().is_empty() {
            return;
        }
        let mut next = tags.get_untracked();
        next.push(candidate);
        let Ok(normalized) = shared::tags::normalize(&next) else {
            return;
        };
        if normalized != tags.get_untracked() {
            on_change.run(normalized);
        }
        draft.set(String::new());
        active_suggestion.set(None);
    };

    let remove = move |tag: String| {
        let next: Vec<String> = tags
            .get_untracked()
            .into_iter()
            .filter(|existing| !shared::tags::eq_ignore_case(existing, &tag))
            .collect();
        if next.len() != tags.get_untracked().len() {
            on_change.run(next);
        }
    };

    view! {
        <div
            class="tag-editor"
            // Clicks inside the editor must not reach the card behind it, which
            // would flip an expanded card into editing mode.
            on:click=|e: leptos::ev::MouseEvent| e.stop_propagation()
        >
            <For
                each=move || tags.get()
                key=|tag: &String| tag.clone()
                children=move |tag: String| {
                    let label = tag.clone();
                    let removed = tag.clone();
                    view! {
                        <span class="tag-chip tag-chip-editable">
                            <span class="tag-chip-label">{format!("#{label}")}</span>
                            <button
                                type="button"
                                class="tag-chip-remove"
                                aria-label=format!("Remove tag {label}")
                                title=format!("Remove tag {label}")
                                // `mousedown` with `preventDefault`, not `click`:
                                // a click would first blur the input and commit
                                // whatever was half-typed, racing this removal.
                                // Preventing the default keeps focus put.
                                on:mousedown=move |e: leptos::ev::MouseEvent| {
                                    e.prevent_default();
                                    e.stop_propagation();
                                    remove(removed.clone());
                                }
                            >"×"</button>
                        </span>
                    }
                }
            />

            <span class="tag-editor-input-wrap">
                <input
                    node_ref=input_ref
                    class="tag-editor-input"
                    type="text"
                    placeholder="+ tag"
                    aria-label="Add tag"
                    prop:value=move || draft.get()
                    on:input=move |ev| {
                        draft.set(event_target_value(&ev));
                        // Any edit invalidates the highlighted suggestion.
                        active_suggestion.set(None);
                    }
                    on:focus=move |_| input_focused.set(true)
                    on:blur=move |_| {
                        input_focused.set(false);
                        // Closing the editor should not silently drop a tag the
                        // user has finished typing.
                        commit(draft.get_untracked());
                    }
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        match ev.key().as_str() {
                            "Enter" => {
                                ev.prevent_default();
                                ev.stop_propagation();
                                let picked = active_suggestion
                                    .get_untracked()
                                    .and_then(|i| suggestions.get_untracked().get(i).cloned());
                                commit(picked.unwrap_or_else(|| draft.get_untracked()));
                            }
                            // A space finishes a tag, matching the rule that a
                            // tag can never contain whitespace.
                            " " => {
                                ev.prevent_default();
                                commit(draft.get_untracked());
                            }
                            "Escape" => {
                                // Never let Escape reach the card and collapse it
                                // while the user is mid-tag.
                                ev.stop_propagation();
                                if draft.get_untracked().is_empty() {
                                    if let Some(el) = input_ref.get_untracked() {
                                        let _ = el.blur();
                                    }
                                } else {
                                    draft.set(String::new());
                                    active_suggestion.set(None);
                                }
                            }
                            "ArrowDown" => {
                                let len = suggestions.get_untracked().len();
                                if len > 0 {
                                    ev.prevent_default();
                                    active_suggestion.update(|i| {
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
                                    active_suggestion.update(|i| {
                                        *i = Some(match *i {
                                            Some(0) | None => len - 1,
                                            Some(current) => current - 1,
                                        });
                                    });
                                }
                            }
                            "Backspace" => {
                                // Backspace on an empty box removes the last chip,
                                // the standard behaviour for a chip input.
                                if draft.get_untracked().is_empty() {
                                    if let Some(last) = tags.get_untracked().last().cloned() {
                                        ev.prevent_default();
                                        remove(last);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                />

                <Show when=move || popup_open.get() fallback=|| ()>
                    <ul class="tag-suggestions" role="listbox">
                        <For
                            each=move || {
                                suggestions.get().into_iter().enumerate().collect::<Vec<_>>()
                            }
                            // Keyed on position as well as value: a keyed
                            // `<For>` retains a view whose key is unchanged, so
                            // a row that merely moves would keep the `index`
                            // captured by value in the highlight closures below
                            // and compare against a stale position.
                            key=|(index, tag): &(usize, String)| { format!("{index}:{tag}") }
                            children=move |(index, tag): (usize, String)| {
                                let label = tag.clone();
                                let picked = tag.clone();
                                view! {
                                    <li>
                                        <button
                                            type="button"
                                            class="tag-suggestion"
                                            class:tag-suggestion-active=move || {
                                                active_suggestion.get() == Some(index)
                                            }
                                            role="option"
                                            aria-selected=move || {
                                                (active_suggestion.get() == Some(index)).to_string()
                                            }
                                            // `mousedown` rather than `click`: the
                                            // input's blur fires first on click and
                                            // would tear the popup down before the
                                            // selection was read.
                                            on:mousedown=move |e: leptos::ev::MouseEvent| {
                                                e.prevent_default();
                                                e.stop_propagation();
                                                commit(picked.clone());
                                            }
                                        >{format!("#{label}")}</button>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </Show>
            </span>
        </div>
    }
    .into_any()
}

/// Read-only chips for a collapsed card, capped so a heavily tagged card can't
/// push its preview text out of view.
#[component]
pub fn TagChips(
    tags: Signal<Vec<String>>,
    /// Live search query — a chip matching a `#tag` term is highlighted the way
    /// the card-number badge is for `#42`.
    highlight: Signal<String>,
    /// Most chips to render before collapsing the rest into a `+N` counter.
    #[prop(default = 3)]
    max: usize,
) -> AnyView {
    let shown = Signal::derive(move || {
        tags.try_get()
            .unwrap_or_default()
            .into_iter()
            .take(max)
            .collect::<Vec<String>>()
    });
    let overflow =
        Signal::derive(move || tags.try_get().unwrap_or_default().len().saturating_sub(max));

    view! {
        <Show when=move || !tags.try_get().unwrap_or_default().is_empty() fallback=|| ()>
            <span class="tag-chip-row">
                <For
                    each=move || shown.get()
                    key=|tag: &String| tag.clone()
                    children=move |tag: String| {
                        let label = tag.clone();
                        let matched = tag;
                        view! {
                            <span
                                class="tag-chip"
                                class:tag-chip-hit=move || {
                                    crate::search::query_matches_tag(
                                        &matched,
                                        &highlight.try_get().unwrap_or_default(),
                                    )
                                }
                            >{format!("#{label}")}</span>
                        }
                    }
                />
                <Show when=move || { overflow.get() > 0 } fallback=|| ()>
                    <span class="tag-chip tag-chip-overflow">
                        {move || format!("+{}", overflow.get())}
                    </span>
                </Show>
            </span>
        </Show>
    }
    .into_any()
}
