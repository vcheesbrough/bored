use leptos::prelude::*;

use crate::search::HashSuggestion;

/// Dropdown of `#`-token completions under the navbar search box.
///
/// Extracted into its own component rather than written inline in `BoardView`.
/// A Leptos `view!` builds one deeply nested generic type, and `BoardView`'s is
/// already very large; adding a `Show > ul > For` inside it pushed the frontend
/// crate's compile time from minutes to over an hour. A component boundary caps
/// that nesting, so the popup's markup is type-checked and monomorphised on its
/// own instead of as another layer of the whole page's type.
#[component]
pub fn SearchSuggestions(
    /// Completions for the token being typed; empty renders nothing.
    suggestions: Signal<Vec<HashSuggestion>>,
    /// Whether the popup is showing — `false` while dismissed with Escape.
    open: Signal<bool>,
    /// Index of the arrow-key-highlighted row.
    active: RwSignal<Option<usize>>,
    /// Invoked with the chosen suggestion when a row is clicked.
    on_accept: Callback<HashSuggestion>,
) -> AnyView {
    view! {
        <Show when=move || { open.get() } fallback=|| ()>
            <ul class="search-suggestions" id="search-hash-suggestions" role="listbox">
                <For
                    each=move || { suggestions.get().into_iter().enumerate().collect::<Vec<_>>() }
                    key=|(_, s): &(usize, HashSuggestion)| s.label()
                    children=move |(index, suggestion): (usize, HashSuggestion)| {
                        let label = suggestion.label();
                        let picked = suggestion;
                        view! {
                            <li>
                                <button
                                    type="button"
                                    class="search-suggestion"
                                    class:search-suggestion-active=move || { active.get() == Some(index) }
                                    role="option"
                                    aria-selected=move || { (active.get() == Some(index)).to_string() }
                                    // `mousedown` fires before the input's blur
                                    // tears the popup down.
                                    on:mousedown=move |e: leptos::ev::MouseEvent| {
                                        e.prevent_default();
                                        on_accept.run(picked.clone());
                                    }
                                >{label}</button>
                            </li>
                        }
                    }
                />
            </ul>
        </Show>
    }
    .into_any()
}
