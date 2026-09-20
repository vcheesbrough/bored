use leptos::prelude::*;

#[derive(Clone, Copy)]
pub struct BoardSearchQuery(pub RwSignal<String>);

/// One column's contribution to [`BoardCardIndex`]: its ID plus the very signal
/// `ColumnView` renders from, so the index never holds a stale copy.
pub type ColumnCardsEntry = (String, RwSignal<Vec<RwSignal<shared::Card>>>);

/// Board-level registry of every column's card list.
///
/// Cards are owned per column by `ColumnView`, but the search box lives in the
/// navbar and needs to see the whole board to suggest tags and card numbers.
/// Each column registers its own signal here on mount and removes it on
/// cleanup, so the aggregate stays reactive without moving ownership of the
/// cards themselves up to `BoardView`.
#[derive(Clone, Copy)]
pub struct BoardCardIndex(pub RwSignal<Vec<ColumnCardsEntry>>);

impl BoardCardIndex {
    /// Every card currently on the board, in no particular column order.
    pub fn all_cards(&self) -> Vec<shared::Card> {
        self.0
            .get()
            .into_iter()
            .flat_map(|(_, cards)| cards.get())
            .filter_map(|card| card.try_get())
            .collect()
    }

    /// The card with this ID, wherever on the board it sits — or `None` if no
    /// column holds it (deleted, or moved to another board).
    ///
    /// Read **untracked** at every level, unlike [`Self::all_cards`]. The one
    /// caller is the `BoardView` effect that reacts to the search *query*
    /// changing (card #375); were these reads tracked, that effect would also
    /// wake on every card edit, and "the card changed" is exactly the case in
    /// which the expanded-card pin must hold (see `card_is_visible`).
    ///
    /// `try_*` throughout because the index hands out signals it does not own:
    /// a column removes its entry in `on_cleanup`, and a card signal can be
    /// disposed between the column dropping it and this lookup running. Both
    /// simply mean "not on the board any more", which is `None`.
    ///
    /// **Which way `None` fails.** The caller cannot tell this `None` from
    /// "nothing is expanded", and `query_change_unpins(None, ..)` is `false`, so
    /// a lock this index cannot resolve **keeps** the pin. That is the safe
    /// direction — releasing a lock on a guess is what would unmount an open
    /// card mid-edit, the #304 trap — and it costs nothing, because a card this
    /// lookup cannot find is a card no column can render either: each entry is
    /// the very `cards` signal its `ColumnView` filters and renders from,
    /// registered in the component body as the column mounts. Walking the ways
    /// a lock goes unresolved: deleting a card locally clears the lock itself; a
    /// remote delete leaves a stale lock naming a card that is in no column's
    /// list; a removed column takes its cards off screen with it. In each, the
    /// pin that survives is pinning nothing on screen. If that ever stops being
    /// true — an index entry that is a *copy* of the column's list, say — this
    /// is where #375 would come back, so keep the two the same signal.
    pub fn find_untracked(&self, id: &str) -> Option<shared::Card> {
        self.0
            .get_untracked()
            .into_iter()
            .flat_map(|(_, cards)| cards.try_get_untracked().unwrap_or_default())
            // Compare IDs through `try_with_untracked` — a borrow — so only the
            // one matching card is cloned, not every body on the board.
            .find(|card| card.try_with_untracked(|c| c.id == id).unwrap_or_default())
            .and_then(|card| card.try_get_untracked())
    }

    /// Distinct tags in use anywhere on the board, case-insensitively deduped
    /// and sorted so the suggestion list is stable between keystrokes.
    pub fn all_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = Vec::new();
        for card in self.all_cards() {
            for tag in card.tags {
                if !tags
                    .iter()
                    .any(|existing| shared::tags::eq_ignore_case(existing, &tag))
                {
                    tags.push(tag);
                }
            }
        }
        tags.sort_by_key(|t| t.to_lowercase());
        tags
    }
}

/// A search query split into its three kinds of term.
///
/// `#` opens a token that is either a **card number** (`#42`, all digits) or a
/// **tag** (`#bug`). Everything else is free text matched against the card
/// body. All three kinds are AND-ed: `#bug deploy` is "tagged bug *and*
/// mentioning deploy".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// `#42`-style card-number terms.
    pub numbers: Vec<u32>,
    /// `#bug`-style tag terms, matched as case-insensitive prefixes so the
    /// filter narrows while the tag is still being typed.
    pub tags: Vec<String>,
    /// Everything that wasn't a `#` token, rejoined with single spaces.
    pub text: String,
}

/// Split a raw query string into [`ParsedQuery`].
///
/// A bare `#` (the instant the user opens a token) contributes nothing — it is
/// the cue to show the suggestion popup, not a filter that should blank the
/// board.
pub fn parse_query(query: &str) -> ParsedQuery {
    let mut parsed = ParsedQuery::default();
    let mut text_terms: Vec<&str> = Vec::new();

    for token in query.split_whitespace() {
        let Some(rest) = token.strip_prefix('#') else {
            text_terms.push(token);
            continue;
        };
        // A bare `#` opens the suggestion popup; on its own it filters nothing.
        if rest.is_empty() {
            continue;
        }
        match rest
            .chars()
            .all(|c| c.is_ascii_digit())
            .then(|| rest.parse::<u32>().ok())
            .flatten()
        {
            Some(number) => parsed.numbers.push(number),
            // Not a number — or a digit run too large to be a card number, in
            // which case treating it as a tag term correctly matches nothing.
            None => parsed.tags.push(rest.to_string()),
        }
    }

    parsed.text = text_terms.join(" ");
    parsed
}

/// One entry in the `#` helper popup: either an existing tag or a card number.
///
/// Both live in the same list because `#` is one token type to the user — they
/// type `#` and expect to be shown what can follow it, whether that turns out
/// to be `#bug` or `#42`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashSuggestion {
    Tag(String),
    /// A card number plus that card's title, so the number means something.
    Card {
        number: u32,
        title: String,
    },
}

impl HashSuggestion {
    /// The text this suggestion inserts, without the leading `#`.
    pub fn value(&self) -> String {
        match self {
            HashSuggestion::Tag(tag) => tag.clone(),
            HashSuggestion::Card { number, .. } => number.to_string(),
        }
    }

    /// What the popup row reads.
    pub fn label(&self) -> String {
        match self {
            HashSuggestion::Tag(tag) => format!("#{tag}"),
            HashSuggestion::Card { number, title } => format!("#{number} · {title}"),
        }
    }
}

/// Most tag and most card suggestions offered at once, each capped separately so
/// a board with hundreds of cards can't crowd the tags out of the list.
const MAX_TAG_SUGGESTIONS: usize = 6;
const MAX_CARD_SUGGESTIONS: usize = 5;

/// The open `#`-token at the end of the query, if any.
///
/// Returns the text after the `#` (possibly empty, right after `#` is typed).
/// A query ending in whitespace has no open token — the user finished that
/// term, so the popup should close rather than keep offering completions.
///
/// Only the last token is considered; the caret position is not consulted, so
/// editing a `#` term in the middle of a longer query offers no completions.
/// That is the rare case, and reading the caret would mean plumbing DOM state
/// into an otherwise pure function.
pub fn active_hash_prefix(query: &str) -> Option<&str> {
    if query.is_empty() || query.ends_with(char::is_whitespace) {
        return None;
    }
    query.split_whitespace().next_back()?.strip_prefix('#')
}

/// Suggestions for the open `#` token: tags first, then card numbers.
///
/// An all-digit prefix is unambiguously a card number, so tags are dropped from
/// the list; a prefix with any non-digit can only be a tag. An empty prefix (the
/// moment `#` is typed) shows both.
///
/// Within each group the order is most-recently-used first: entries the user
/// has picked before, in pick order, then everything else by how recently the
/// underlying card changed. `recent_tags` and `recent_card_ids` come from
/// [`crate::recent::RecentPicks`] and are empty until the user picks something,
/// at which point only the recency fallback applies.
pub fn hash_suggestions(
    prefix: &str,
    all_tags: &[String],
    cards: &[shared::Card],
    recent_tags: &[String],
    recent_card_ids: &[String],
) -> Vec<HashSuggestion> {
    let digits_only = !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit());
    let mut out: Vec<HashSuggestion> = Vec::new();

    if !digits_only {
        let mut tags: Vec<&String> = all_tags
            .iter()
            .filter(|tag| shared::tags::starts_with_ignore_case(tag, prefix))
            .collect();
        tags.sort_by_cached_key(|tag| {
            (
                crate::recent::tag_rank_of(recent_tags, tag),
                // A tag is only as recent as the liveliest card wearing it,
                // so the newest such card stands in for the tag's own recency.
                std::cmp::Reverse(newest_card_with_tag(cards, tag)),
                tag.to_lowercase(),
            )
        });
        out.extend(
            tags.into_iter()
                .take(MAX_TAG_SUGGESTIONS)
                .map(|tag| HashSuggestion::Tag(tag.clone())),
        );
    }

    if prefix.is_empty() || digits_only {
        let mut numbered: Vec<&shared::Card> = cards
            .iter()
            .filter(|card| card.number.to_string().starts_with(prefix))
            .collect();
        numbered.sort_by_cached_key(|card| {
            (
                crate::recent::rank_of(recent_card_ids, &card.id),
                std::cmp::Reverse(crate::recent::recency_key(&card.updated_at)),
                // Highest number last among equals: with no history to go on,
                // the newest cards are the ones being referenced.
                std::cmp::Reverse(card.number),
            )
        });
        out.extend(numbered.into_iter().take(MAX_CARD_SUGGESTIONS).map(|card| {
            HashSuggestion::Card {
                number: card.number,
                title: shared::history::card_title_from_body(&card.body),
            }
        }));
    }

    out
}

/// Sort key for "how recently was a card carrying this tag touched": the
/// greatest [`crate::recent::recency_key`] among the cards tagged `tag`, or an
/// empty string when no card carries it (which sorts last under `Reverse`).
fn newest_card_with_tag(cards: &[shared::Card], tag: &str) -> String {
    cards
        .iter()
        .filter(|card| {
            card.tags
                .iter()
                .any(|card_tag| shared::tags::eq_ignore_case(card_tag, tag))
        })
        .map(|card| crate::recent::recency_key(&card.updated_at))
        .max()
        .unwrap_or_default()
}

/// Replace the query's open `#` token with `#value`, leaving a trailing space so
/// the next term can be typed straight away.
pub fn apply_hash_suggestion(query: &str, value: &str) -> String {
    let trimmed_end = query.trim_end_matches(|c: char| !c.is_whitespace());
    format!("{trimmed_end}#{value} ")
}

/// Whether `card_number` sits one hop from `target` on the board's links, in
/// either direction — `target`'s predecessors and its successors alike.
///
/// Both ends are compared by **number**, not id, because [`shared::CardLink`]
/// carries `predecessor_number` and `successor_number` already (projected from
/// the cards when the link is read). That is what keeps the `#42` filter a pure
/// function of the card and the link list: no `BoardCardIndex` lookup to turn
/// the typed number into an id, and nothing to go stale between the two.
///
/// Deliberately one hop. Links form a DAG, so following the chain would answer
/// `#42` with everything up- and downstream of it — on a board where the cards
/// are sequenced end to end, that is most of the board, and a filter that
/// returns most of the board is not a filter. "Before and after this ticket" is
/// the question the link editor poses and the one this answers.
pub fn linked_to_number(links: &[shared::CardLink], card_number: u32, target: u32) -> bool {
    links.iter().any(|link| {
        (link.predecessor_number == target && link.successor_number == card_number)
            || (link.successor_number == target && link.predecessor_number == card_number)
    })
}

/// Whether `card` survives `query`.
///
/// `links` is the board's whole link list ([`crate::links::BoardLinkIndex`]),
/// which only the `#42` card-number arm consults — see [`linked_to_number`].
/// Pass an empty slice where there are no links to consider; every other kind
/// of term ignores it.
pub fn card_matches_query(card: &shared::Card, query: &str, links: &[shared::CardLink]) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    let parsed = parse_query(query);

    // `#42` is a card-number term — it deliberately ignores body text, and
    // matches card 42 *or* any card directly linked to it (card #305).
    if !parsed
        .numbers
        .iter()
        .all(|n| *n == card.number || linked_to_number(links, card.number, *n))
    {
        return false;
    }
    // Every `#tag` term must match some tag on the card (prefix, ignoring case).
    if !parsed.tags.iter().all(|needle| {
        card.tags
            .iter()
            .any(|tag| shared::tags::starts_with_ignore_case(tag, needle))
    }) {
        return false;
    }

    let text = parsed.text.trim();
    if text.is_empty() {
        // Nothing but `#` terms (or just a bare `#`): they already decided it.
        return true;
    }

    // A bare number matches the card number *or* the body, unlike `#42` — and,
    // also unlike `#42`, it stops there: it does not reach along links. `42` is
    // as often someone searching for a figure in a body as for a ticket, and
    // widening that to a card's neighbours would pull in cards containing
    // neither. The link expansion is what the explicit `#` asks for.
    if text.chars().all(|c| c.is_ascii_digit())
        && text
            .parse::<u32>()
            .is_ok_and(|number| number == card.number)
    {
        return true;
    }

    let haystack = normalize(&card.body);
    let needle = normalize(text);
    if needle.is_empty() {
        return true;
    }
    if haystack.contains(&needle) {
        return true;
    }

    let haystack_words: Vec<&str> = haystack.split_whitespace().collect();
    needle.split_whitespace().all(|part| {
        haystack_words
            .iter()
            .any(|word| word.contains(part) || is_subsequence(part, word))
    })
}

/// Whether a column should render `card` at all: it matches the query, **or**
/// it is the one card currently expanded.
///
/// The expanded card is pinned deliberately, and for two reasons.
///
/// The behavioural one: the card you are editing must not vanish from under the
/// cursor the instant your edit stops matching the filter. Removing the very tag
/// you are filtering on, or typing the body past a text search, otherwise
/// unmounts the editor mid-keystroke. Pinned, the card stays put while expanded
/// and leaves the filtered view when you collapse it — the point at which you
/// are done with it.
///
/// The structural one: that unmount was a reactive-disposal trap. The column's
/// `<For>` re-filters on the same `card` write that the expanded card's own
/// render closures are subscribed to, so the card was unmounted — disposing its
/// signals — while those closures were queued to read them. The resulting
/// `Get::get` panic ("Tried to access a reactive value that has already been
/// disposed") fires inside the `wasm-bindgen-futures` task queue and wedges the
/// executor: the in-flight `PUT` never runs and no effect ever runs again, so
/// the whole tab is dead until a reload. Keeping the card mounted removes the
/// trigger at its source, for every edit that could cause it — local, remote, or
/// from another agent.
///
/// `expanded_id` is the board-level `ExpandedCardId` lock, so at most one card
/// on the board is ever pinned.
///
/// **What the pin does not cover.** It protects a card from *its own content*
/// moving out from under a query that is standing still. It is not meant to
/// hold a card against the *query* moving: someone typing a new search has
/// turned their attention to the search box, and every card that fails the new
/// query should leave — the expanded one included (card #375, where a freshly
/// created, still-expanded card sat in the results of every search typed after
/// it). This function cannot tell the two cases apart — it sees one card and
/// one query, not which of them just changed — so the distinction is drawn by
/// [`query_change_unpins`], which `BoardView` consults whenever the query
/// changes and answers by releasing the lock.
pub fn card_is_visible(
    card: &shared::Card,
    query: &str,
    expanded_id: Option<&str>,
    links: &[shared::CardLink],
) -> bool {
    if expanded_id == Some(card.id.as_str()) {
        return true;
    }
    card_matches_query(card, query, links)
}

/// Whether the search moving from `old_query` to `new_query` should release the
/// expanded-card pin described on [`card_is_visible`].
///
/// True only when **both** hold:
///
/// * the query really changed. The caller is an `Effect`, which can be woken
///   by a write that set the same string again; that is not the user moving
///   the search, so it must not collapse the card they are working in;
/// * there is an expanded card and it fails the new query. An expanded card
///   that still matches stays open — narrowing a search around the card you are
///   reading should not slam it shut.
///
/// `expanded` is the card the board-level `ExpandedCardId` lock currently names,
/// or `None` when nothing is expanded (or the lock names a card that is no
/// longer on the board). Borrowed as `Option<&Card>` rather than taking an ID,
/// so this stays a pure function of its arguments: no signals, no board lookup,
/// and therefore testable on the host target.
///
/// Note what is deliberately *absent*: whether the card matched `old_query`. A
/// card that was already pinned-and-unmatching (edited out of the filter, which
/// is the case the pin exists for) is released by the next query change just
/// the same. "The user changed the search" re-evaluates every card on the
/// board; the pin is a courtesy that lasts while the search stands still.
///
/// `links` is passed through to [`card_matches_query`] so that "fails the new
/// query" means the same thing here as it does in the column filter. Without
/// it, typing `#42` while a card linked to 42 is expanded would release the pin
/// and collapse a card the filter then keeps on screen anyway (card #305).
pub fn query_change_unpins(
    expanded: Option<&shared::Card>,
    old_query: &str,
    new_query: &str,
    links: &[shared::CardLink],
) -> bool {
    old_query != new_query
        && expanded.is_some_and(|card| !card_matches_query(card, new_query, links))
}

/// Returns `true` when `query` contains a card-number search (`#42` or a bare
/// `42`) that matches `number`.
///
/// The card badge is highlighted for these queries; the body is not, because a
/// number search deliberately ignores body text (see `card_matches_query`).
pub fn query_matches_number(number: u32, query: &str) -> bool {
    let parsed = parse_query(query.trim());
    if parsed.numbers.contains(&number) {
        return true;
    }
    let text = parsed.text.trim();
    !text.is_empty()
        && text.chars().all(|c| c.is_ascii_digit())
        && text.parse::<u32>().is_ok_and(|parsed| parsed == number)
}

/// Returns `true` when `query` has a `#tag` term that `tag` satisfies, so the
/// matching chip on the card can be highlighted the way the number badge is.
pub fn query_matches_tag(tag: &str, query: &str) -> bool {
    parse_query(query.trim())
        .tags
        .iter()
        .any(|needle| shared::tags::starts_with_ignore_case(tag, needle))
}

/// Byte ranges of `text` that `query` matched, for rendering `<mark>` spans.
///
/// The ranges mirror the three matching paths in [`card_matches_query`]:
///
/// * a `#42`-style query highlights nothing in the body — it is a number search,
///   and the number badge is highlighted instead;
/// * each query term that occurs literally inside a word highlights exactly that
///   substring (`deploy` in `Deployment` → the first six bytes);
/// * a term that only matches as a subsequence highlights the whole word
///   (`crd` in `card` → all four bytes), because the individual matched letters
///   are not a meaningful visual unit.
///
/// Returned ranges are sorted and non-overlapping, so wrapping each one in a
/// `<mark>` can never produce nested or crossing elements.
pub fn highlight_spans(text: &str, query: &str) -> Vec<(usize, usize)> {
    // `#` terms never consult the body — `#42` is a number search and `#bug` a
    // tag search — so only the free-text remainder can mark anything.
    let free_text = parse_query(query.trim()).text;

    let terms = terms_of(&free_text);
    if terms.is_empty() {
        return Vec::new();
    }

    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (start, word) in words_of(text) {
        // ASCII-lowercasing preserves byte length, so offsets into `lower` are
        // also valid offsets into `word`.
        let lower = word.to_ascii_lowercase();
        for term in &terms {
            let mut search_from = 0;
            let mut literal_hit = false;
            // Every occurrence is marked, not just the first — a term can repeat
            // inside one word (`ana` in `banana`).
            while let Some(offset) = lower[search_from..].find(term.as_str()) {
                let hit = search_from + offset;
                spans.push((start + hit, start + hit + term.len()));
                literal_hit = true;
                // Advance by one byte rather than `term.len()` so overlapping
                // occurrences (`ana` in `banana`) are all found; the merge step
                // below collapses them into one span.
                search_from = hit + 1;
                if search_from >= lower.len() {
                    break;
                }
            }
            if !literal_hit && is_subsequence(term, &lower) {
                spans.push((start, start + word.len()));
            }
        }
    }

    merge_spans(spans)
}

/// Splits `query` into lowercase alphanumeric runs, matching how [`normalize`]
/// tokenizes the card body so both sides agree on what a "word" is.
fn terms_of(query: &str) -> Vec<String> {
    normalize(query)
        .split_whitespace()
        .map(|term| term.to_string())
        .collect()
}

/// Yields `(byte_offset, word)` for every run of ASCII alphanumerics in `text`.
///
/// Non-ASCII characters are separators, exactly as in [`normalize`], so every
/// offset lands on a UTF-8 character boundary.
fn words_of(text: &str) -> Vec<(usize, &str)> {
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        if ch.is_ascii_alphanumeric() {
            start.get_or_insert(index);
        } else if let Some(word_start) = start.take() {
            words.push((word_start, &text[word_start..index]));
        }
    }
    if let Some(word_start) = start {
        words.push((word_start, &text[word_start..]));
    }
    words
}

/// Sorts spans and folds overlapping or touching ones together.
fn merge_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            // `start <= last_end` also folds abutting spans, which keeps
            // adjacent marks from rendering as two boxes with a seam.
            Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn normalize(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(number: u32, body: &str) -> shared::Card {
        tagged_card(number, body, &[])
    }

    fn tagged_card(number: u32, body: &str, tags: &[&str]) -> shared::Card {
        shared::Card {
            // Distinct per card: the suggestion order keys recent picks by ID.
            id: format!("card-{number}"),
            column_id: "column".to_string(),
            body: body.to_string(),
            position: 0,
            number,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            last_edited_by: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// A card with an explicit `updated_at`, for the recency fallback.
    fn card_updated(number: u32, body: &str, tags: &[&str], updated_at: &str) -> shared::Card {
        shared::Card {
            updated_at: updated_at.to_string(),
            ..tagged_card(number, body, tags)
        }
    }

    /// `hash_suggestions` with no pick history — the pre-MRU default order.
    fn suggestions(prefix: &str, tags: &[String], cards: &[shared::Card]) -> Vec<HashSuggestion> {
        hash_suggestions(prefix, tags, cards, &[], &[])
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// A link `#predecessor → #successor`. Only the two numbers are ever read
    /// by the filter, so the rest is filler.
    fn link(predecessor: u32, successor: u32) -> shared::CardLink {
        shared::CardLink {
            id: format!("link-{predecessor}-{successor}"),
            predecessor_id: format!("card-{predecessor}"),
            successor_id: format!("card-{successor}"),
            predecessor_number: predecessor,
            successor_number: successor,
            reason: None,
            last_edited_by: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    // The three filter rules on a board with **no** links, which is what all
    // but the link tests below are about: there, `#42` means card 42 and
    // nothing else, exactly as it did before card #305. The link cases call the
    // real functions with a list.
    fn matches(card: &shared::Card, query: &str) -> bool {
        card_matches_query(card, query, &[])
    }

    fn visible(card: &shared::Card, query: &str, expanded_id: Option<&str>) -> bool {
        card_is_visible(card, query, expanded_id, &[])
    }

    fn unpins(expanded: Option<&shared::Card>, old_query: &str, new_query: &str) -> bool {
        query_change_unpins(expanded, old_query, new_query, &[])
    }

    #[test]
    fn matches_empty_query() {
        assert!(matches(&card(42, "Deploy preview"), "  "));
    }

    // ── card_is_visible: the expanded-card pin ─────────────────────────────

    #[test]
    fn visibility_follows_the_query_when_nothing_is_expanded() {
        let c = tagged_card(1, "Deploy preview", &["bug"]);
        assert!(visible(&c, "#bug", None));
        assert!(!visible(&c, "#chore", None));
    }

    #[test]
    fn the_expanded_card_survives_a_query_it_no_longer_matches() {
        // The bug this pin exists for: the user filters `#bug`, expands the
        // card, and removes that very tag. Without the pin the card unmounts
        // mid-edit and the disposal trap kills the tab.
        let c = tagged_card(1, "Deploy preview", &[]);
        assert!(!matches(&c, "#bug"));
        assert!(visible(&c, "#bug", Some("card-1")));
    }

    #[test]
    fn a_collapsed_card_is_not_pinned() {
        // The other half of the behaviour: collapsing is what lets a card that
        // stopped matching finally leave the filtered view.
        let c = tagged_card(1, "Deploy preview", &[]);
        assert!(!visible(&c, "#bug", None));
    }

    #[test]
    fn a_different_expanded_card_does_not_pin_this_one() {
        let c = tagged_card(1, "Deploy preview", &[]);
        assert!(!visible(&c, "#bug", Some("card-2")));
    }

    #[test]
    fn an_empty_query_shows_every_card_expanded_or_not() {
        let c = tagged_card(1, "Deploy preview", &[]);
        assert!(visible(&c, "", None));
        assert!(visible(&c, "", Some("card-1")));
    }

    // ── query_change_unpins: the query moving releases the pin ─────────────
    //
    // Expected values below are written out by hand from the card bodies, never
    // derived by calling `card_matches_query` — the function under test is built
    // on it, so an oracle that shared it could not disagree with it.

    #[test]
    fn typing_past_the_expanded_card_releases_the_pin() {
        // Card #375 verbatim: an expanded card reading `abcdef`, and a search
        // that grows from the prefix it matches to one it cannot.
        let c = card(1, "abcdef");
        assert!(unpins(Some(&c), "abc", "abcX"));
        assert!(unpins(Some(&c), "abcXX", "abcXXX"));
    }

    #[test]
    fn narrowing_around_a_still_matching_expanded_card_keeps_it_open() {
        let c = card(1, "abcdef");
        assert!(!unpins(Some(&c), "", "a"));
        assert!(!unpins(Some(&c), "ab", "abc"));
        // Clearing the search matches everything, so it never collapses a card.
        assert!(!unpins(Some(&c), "abcXXX", ""));
    }

    #[test]
    fn a_rewrite_of_the_same_query_is_not_the_search_moving() {
        // The card was edited out of `#bug` while expanded — pinned and
        // unmatching, the state #304 protects. A signal write that sets the
        // identical string must leave it alone…
        let c = tagged_card(1, "Deploy preview", &[]);
        assert!(!unpins(Some(&c), "#bug", "#bug"));
        // …but the user really changing the search re-evaluates it like any
        // other card, even though it did not match the old query either.
        assert!(unpins(Some(&c), "#bug", "#bug "));
        assert!(unpins(Some(&c), "#bug", "#bu"));
    }

    #[test]
    fn nothing_expanded_means_nothing_to_release() {
        assert!(!unpins(None, "abc", "abcX"));
    }

    #[test]
    fn tag_and_number_terms_release_the_pin_too() {
        // The rule is about the whole query, not just free text.
        let c = tagged_card(7, "Deploy preview", &["bug"]);
        assert!(!unpins(Some(&c), "", "#bug"));
        assert!(!unpins(Some(&c), "", "#7"));
        assert!(unpins(Some(&c), "#bug", "#chore"));
        assert!(unpins(Some(&c), "#7", "#8"));
    }

    #[test]
    fn the_pin_also_covers_a_body_edit_past_a_text_search() {
        // Same shape as the tag case, reached by editing the body instead.
        let c = card(1, "unrelated now");
        assert!(!matches(&c, "deploy"));
        assert!(visible(&c, "deploy", Some("card-1")));
    }

    #[test]
    fn matches_card_number_with_or_without_hash() {
        let c = card(42, "Deploy preview");
        assert!(matches(&c, "#42"));
        assert!(matches(&c, "42"));
        assert!(!matches(&c, "#41"));
    }

    #[test]
    fn hash_prefixed_number_does_not_match_body_text() {
        assert!(!matches(&card(7, "See #42 in the notes"), "#42"));
    }

    // ── `#42` reaches one hop along the links — card #305 ──────────────────

    #[test]
    fn linked_to_number_reads_a_link_from_either_end() {
        // 42 comes before 7, and 9 comes before 42.
        let links = [link(42, 7), link(9, 42)];
        assert!(linked_to_number(&links, 7, 42));
        assert!(linked_to_number(&links, 9, 42));
        // …and the same two links seen from the other card's point of view.
        assert!(linked_to_number(&links, 42, 7));
        assert!(linked_to_number(&links, 42, 9));
        // A card at neither end of any link, and a link between two other cards.
        assert!(!linked_to_number(&links, 5, 42));
        assert!(!linked_to_number(&links, 7, 9));
        assert!(!linked_to_number(&[], 7, 42));
    }

    #[test]
    fn hash_number_matches_cards_linked_in_either_direction() {
        let links = [link(42, 7), link(9, 42)];
        // The card the query names.
        assert!(card_matches_query(&card(42, "The ticket"), "#42", &links));
        // Its successor and its predecessor, neither of which says "42".
        assert!(card_matches_query(&card(7, "Comes after"), "#42", &links));
        assert!(card_matches_query(&card(9, "Comes before"), "#42", &links));
        // An unlinked card stays out.
        assert!(!card_matches_query(&card(5, "Unrelated"), "#42", &links));
        // And the same card list with no links behaves as it did before #305.
        assert!(!card_matches_query(&card(7, "Comes after"), "#42", &[]));
    }

    #[test]
    fn hash_number_does_not_follow_a_chain() {
        // 42 → 7 → 8: card 8 is two hops out, so `#42` leaves it hidden.
        let links = [link(42, 7), link(7, 8)];
        assert!(card_matches_query(&card(7, "One hop"), "#42", &links));
        assert!(!card_matches_query(&card(8, "Two hops"), "#42", &links));
    }

    #[test]
    fn a_bare_number_does_not_reach_along_links() {
        let links = [link(42, 7)];
        assert!(!card_matches_query(&card(7, "Comes after"), "42", &links));
        // The bare form still does what it always did: number, or body text.
        assert!(card_matches_query(&card(42, "The ticket"), "42", &links));
        assert!(card_matches_query(
            &card(7, "See 42 in the notes"),
            "42",
            &links
        ));
    }

    #[test]
    fn a_linked_card_still_has_to_satisfy_the_other_terms() {
        let links = [link(42, 7), link(42, 9)];
        let tagged = tagged_card(7, "Deploy preview", &["bug"]);
        let plain = card(9, "Release notes");
        assert!(card_matches_query(&tagged, "#42 deploy", &links));
        assert!(card_matches_query(&tagged, "#42 #bug", &links));
        // Linked to 42, but the text and the tag term are not satisfied.
        assert!(!card_matches_query(&plain, "#42 deploy", &links));
        assert!(!card_matches_query(&plain, "#42 #bug", &links));
    }

    #[test]
    fn two_number_terms_can_now_both_be_satisfied() {
        // `#1 #2` matches nothing without links — no card has two numbers — but
        // a card linked to both is in the neighbourhood of both.
        let links = [link(1, 7), link(2, 7)];
        assert!(card_matches_query(&card(7, "After both"), "#1 #2", &links));
        // Card 1 satisfies `#1` by being itself, but is not linked to 2.
        assert!(!card_matches_query(&card(1, "First"), "#1 #2", &links));
    }

    #[test]
    fn a_linked_card_lights_up_its_link_badge_not_its_number() {
        // What `LinkBadges` and the card's number badge ask, for card 7 under
        // `#42`: the link pill naming 42 is the hit; 7's own badge is not.
        assert!(query_matches_number(42, "#42"));
        assert!(!query_matches_number(7, "#42"));
    }

    #[test]
    fn the_pin_holds_for_a_card_that_matches_only_by_link() {
        let c = card(7, "Comes after");
        let links = [link(42, 7)];
        // Typing `#42` leaves card 7 on screen, so releasing the pin would
        // collapse a card the filter keeps — the effect must not fire.
        assert!(!query_change_unpins(Some(&c), "", "#42", &links));
        // Control: without that link the same query does release it.
        assert!(query_change_unpins(Some(&c), "", "#42", &[]));
        // And a query that card 7 fails either way still releases it.
        assert!(query_change_unpins(Some(&c), "", "#41", &links));
    }

    #[test]
    fn matches_case_insensitive_substrings() {
        assert!(matches(&card(1, "Deploy Preview"), "deploy"));
    }

    #[test]
    fn matches_fuzzy_word_subsequences() {
        let c = card(1, "SSE card created in another browser context");
        assert!(matches(&c, "sse crd"));
        assert!(matches(&c, "brwsr ctx"));
    }

    // ── `#` token parsing ────────────────────────────────────────────

    #[test]
    fn parse_query_splits_numbers_tags_and_free_text() {
        let parsed = parse_query("#42 #bug deploy preview");
        assert_eq!(parsed.numbers, vec![42]);
        assert_eq!(parsed.tags, vec!["bug".to_string()]);
        assert_eq!(parsed.text, "deploy preview");
    }

    #[test]
    fn parse_query_ignores_a_bare_hash() {
        // The instant `#` is typed the popup opens; the board must not blank.
        let parsed = parse_query("# deploy");
        assert!(parsed.numbers.is_empty());
        assert!(parsed.tags.is_empty());
        assert_eq!(parsed.text, "deploy");
    }

    #[test]
    fn bare_hash_matches_every_card() {
        assert!(matches(&card(1, "anything"), "#"));
    }

    #[test]
    fn tag_token_filters_by_tag() {
        let tagged = tagged_card(1, "Deploy preview", &["bug"]);
        let untagged = card(2, "Deploy preview");
        assert!(matches(&tagged, "#bug"));
        assert!(!matches(&untagged, "#bug"));
    }

    #[test]
    fn tag_token_matches_a_prefix_case_insensitively() {
        // Filtering narrows as the tag is typed, and `Bug` == `bug`.
        let tagged = tagged_card(1, "body", &["Bug"]);
        assert!(matches(&tagged, "#bu"));
        assert!(matches(&tagged, "#BUG"));
        assert!(!matches(&tagged, "#ug"));
    }

    #[test]
    fn multiple_tag_tokens_are_anded() {
        let both = tagged_card(1, "body", &["bug", "urgent"]);
        let one = tagged_card(2, "body", &["bug"]);
        assert!(matches(&both, "#bug #urgent"));
        assert!(!matches(&one, "#bug #urgent"));
    }

    #[test]
    fn tag_and_free_text_are_anded() {
        let match_both = tagged_card(1, "Deploy preview", &["bug"]);
        let wrong_text = tagged_card(2, "Something else", &["bug"]);
        let wrong_tag = tagged_card(3, "Deploy preview", &["chore"]);
        assert!(matches(&match_both, "#bug deploy"));
        assert!(!matches(&wrong_text, "#bug deploy"));
        assert!(!matches(&wrong_tag, "#bug deploy"));
    }

    #[test]
    fn number_token_still_ignores_body_text_when_combined_with_a_tag() {
        let c = tagged_card(7, "See 42 in the notes", &["bug"]);
        assert!(!matches(&c, "#42 #bug"));
        assert!(matches(&c, "#7 #bug"));
    }

    #[test]
    fn free_text_highlight_survives_a_leading_tag_token() {
        // The `#bug` term contributes nothing to the body marks, but "deploy"
        // still lights up — a filtered card must never render with zero marks.
        assert_eq!(
            highlight_spans("Deploy preview", "#bug deploy"),
            vec![(0, 6)]
        );
    }

    #[test]
    fn query_matches_number_reads_a_hash_token_anywhere_in_the_query() {
        assert!(query_matches_number(42, "#bug #42"));
        assert!(query_matches_number(42, "42"));
        assert!(!query_matches_number(42, "#bug"));
    }

    #[test]
    fn query_matches_tag_reports_the_chip_to_highlight() {
        assert!(query_matches_tag("bug", "#bu deploy"));
        assert!(!query_matches_tag("chore", "#bu deploy"));
        // A number token is not a tag term.
        assert!(!query_matches_tag("42", "#42"));
    }

    // ── `#` suggestion popup ─────────────────────────────────────────

    #[test]
    fn active_hash_prefix_finds_the_open_token() {
        assert_eq!(active_hash_prefix("#"), Some(""));
        assert_eq!(active_hash_prefix("#bu"), Some("bu"));
        assert_eq!(active_hash_prefix("deploy #bu"), Some("bu"));
        // Finished terms close the popup.
        assert_eq!(active_hash_prefix("#bug "), None);
        assert_eq!(active_hash_prefix("deploy"), None);
        assert_eq!(active_hash_prefix(""), None);
    }

    #[test]
    fn hash_suggestions_offer_tags_and_cards_for_an_empty_prefix() {
        let tags = vec!["bug".to_string()];
        let cards = vec![card(42, "# Deploy preview")];
        let out = suggestions("", &tags, &cards);
        assert_eq!(
            out,
            vec![
                HashSuggestion::Tag("bug".to_string()),
                HashSuggestion::Card {
                    number: 42,
                    title: "Deploy preview".to_string(),
                },
            ]
        );
    }

    #[test]
    fn hash_suggestions_drop_tags_for_an_all_digit_prefix() {
        let tags = vec!["4chan".to_string()];
        let cards = vec![card(42, "# Deploy"), card(7, "# Other")];
        let out = suggestions("4", &tags, &cards);
        assert_eq!(
            out,
            vec![HashSuggestion::Card {
                number: 42,
                title: "Deploy".to_string(),
            }]
        );
    }

    #[test]
    fn hash_suggestions_drop_cards_for_a_non_digit_prefix() {
        let tags = vec!["bug".to_string(), "chore".to_string()];
        let cards = vec![card(42, "# Deploy")];
        assert_eq!(
            suggestions("bu", &tags, &cards),
            vec![HashSuggestion::Tag("bug".to_string())]
        );
    }

    #[test]
    fn hash_suggestions_list_highest_card_numbers_first() {
        let cards = vec![card(3, "# Three"), card(11, "# Eleven"), card(7, "# Seven")];
        let numbers: Vec<String> = suggestions("", &[], &cards)
            .iter()
            .map(HashSuggestion::value)
            .collect();
        assert_eq!(numbers, vec!["11", "7", "3"]);
    }

    #[test]
    fn hash_suggestions_lead_with_the_most_recently_picked_card() {
        let cards = vec![card(3, "# Three"), card(11, "# Eleven"), card(7, "# Seven")];
        // `card-3` was picked most recently, so it jumps the number order.
        let picked = strings(&["card-3", "card-7"]);
        let numbers: Vec<String> = hash_suggestions("", &[], &cards, &[], &picked)
            .iter()
            .map(HashSuggestion::value)
            .collect();
        assert_eq!(numbers, vec!["3", "7", "11"]);
    }

    #[test]
    fn hash_suggestions_fall_back_to_card_recency_without_a_pick_history() {
        let cards = vec![
            card_updated(3, "# Three", &[], "2026-09-12T10:00:00Z"),
            card_updated(11, "# Eleven", &[], "2026-09-10T10:00:00Z"),
            card_updated(7, "# Seven", &[], "2026-09-14T10:00:00Z"),
        ];
        let numbers: Vec<String> = suggestions("", &[], &cards)
            .iter()
            .map(HashSuggestion::value)
            .collect();
        // Most recently touched first — not highest-numbered first.
        assert_eq!(numbers, vec!["7", "3", "11"]);
    }

    #[test]
    fn hash_suggestions_lead_with_the_most_recently_picked_tag() {
        let tags = strings(&["bug", "chore", "docs"]);
        let picked = strings(&["docs", "bug"]);
        let out = hash_suggestions("", &tags, &[], &picked, &[]);
        assert_eq!(
            out,
            vec![
                HashSuggestion::Tag("docs".to_string()),
                HashSuggestion::Tag("bug".to_string()),
                HashSuggestion::Tag("chore".to_string()),
            ]
        );
    }

    #[test]
    fn hash_suggestions_match_a_picked_tag_ignoring_case() {
        let tags = strings(&["Bug", "chore"]);
        let picked = strings(&["bug"]);
        let out = hash_suggestions("", &tags, &[], &picked, &[]);
        assert_eq!(out.first(), Some(&HashSuggestion::Tag("Bug".to_string())));
    }

    #[test]
    fn unpicked_tags_order_by_their_liveliest_card() {
        // No pick history: `chore` leads because the card wearing it changed
        // most recently, even though `bug` sorts first alphabetically.
        let cards = vec![
            card_updated(1, "# One", &["bug"], "2026-09-10T10:00:00Z"),
            card_updated(2, "# Two", &["chore"], "2026-09-14T10:00:00Z"),
        ];
        let tags = strings(&["bug", "chore"]);
        let out = suggestions("", &tags, &cards);
        assert_eq!(
            out.first(),
            Some(&HashSuggestion::Tag("chore".to_string())),
            "{out:?}"
        );
    }

    #[test]
    fn tags_on_no_card_sort_last_and_stay_alphabetical() {
        // A tag whose cards have all gone has no recency at all; two of them
        // must still order predictably rather than by `Vec` happenstance.
        let cards = vec![card_updated(1, "# One", &["live"], "2026-09-14T10:00:00Z")];
        let tags = strings(&["zeta", "alpha", "live"]);
        // An empty prefix also offers cards; only the tag half is under test.
        let offered: Vec<HashSuggestion> = suggestions("", &tags, &cards)
            .into_iter()
            .filter(|s| matches!(s, HashSuggestion::Tag(_)))
            .collect();
        assert_eq!(
            offered,
            vec![
                HashSuggestion::Tag("live".to_string()),
                HashSuggestion::Tag("alpha".to_string()),
                HashSuggestion::Tag("zeta".to_string()),
            ]
        );
    }

    #[test]
    fn applying_a_suggestion_replaces_only_the_open_token() {
        assert_eq!(apply_hash_suggestion("#bu", "bug"), "#bug ");
        assert_eq!(apply_hash_suggestion("deploy #bu", "bug"), "deploy #bug ");
        assert_eq!(apply_hash_suggestion("#", "42"), "#42 ");
    }

    #[test]
    fn highlight_spans_marks_literal_substring() {
        // "deploy" inside "Deployment" marks only the matched prefix.
        let text = "Deployment checklist";
        assert_eq!(highlight_spans(text, "deploy"), vec![(0, 6)]);
        assert_eq!(&text[0..6], "Deploy");
    }

    #[test]
    fn highlight_spans_is_case_insensitive_and_marks_every_occurrence() {
        let text = "Deploy the deploy script";
        assert_eq!(highlight_spans(text, "DEPLOY"), vec![(0, 6), (11, 17)]);
    }

    #[test]
    fn highlight_spans_marks_each_term_of_a_multi_word_query() {
        let text = "Deploy preview environment";
        let spans = highlight_spans(text, "deploy preview");
        assert_eq!(spans, vec![(0, 6), (7, 14)]);
    }

    #[test]
    fn highlight_spans_marks_whole_word_for_subsequence_match() {
        // "crd" matches "card" and "created" only as a subsequence, so each
        // whole word is marked rather than three disconnected letters.
        let text = "SSE card created";
        assert_eq!(highlight_spans(text, "crd"), vec![(4, 8), (9, 16)]);
        assert_eq!(&text[4..8], "card");
        assert_eq!(&text[9..16], "created");
    }

    #[test]
    fn highlight_spans_ignores_hash_number_query() {
        // `#42` is a number search — the badge is highlighted, not the body.
        assert!(highlight_spans("See 42 items in card 42", "#42").is_empty());
    }

    #[test]
    fn highlight_spans_marks_bare_number_in_body() {
        // A bare `42` matches the body as well as the card number, so body
        // occurrences are marked.
        assert_eq!(highlight_spans("Ticket 42 done", "42"), vec![(7, 9)]);
    }

    #[test]
    fn highlight_spans_merges_overlapping_matches() {
        // "ana" occurs twice, overlapping, in "banana": one merged span.
        assert_eq!(highlight_spans("banana", "ana"), vec![(1, 6)]);
    }

    #[test]
    fn highlight_spans_merges_overlapping_terms() {
        // "dep" (literal) and "dply" (subsequence over the whole word) overlap.
        assert_eq!(highlight_spans("deploy now", "dep dply"), vec![(0, 6)]);
    }

    #[test]
    fn highlight_spans_empty_for_blank_query() {
        assert!(highlight_spans("Deploy checklist", "   ").is_empty());
        assert!(highlight_spans("Deploy checklist", "---").is_empty());
    }

    #[test]
    fn highlight_spans_land_on_char_boundaries_with_multibyte_text() {
        // Non-ASCII characters are word separators, so spans never split a
        // multi-byte character; slicing with them must not panic.
        let text = "café deploy — naïve";
        for (start, end) in highlight_spans(text, "caf deploy nave") {
            let _ = &text[start..end];
        }
        assert!(!highlight_spans(text, "deploy").is_empty());
    }

    #[test]
    fn highlight_spans_cover_a_query_that_matched_the_card() {
        // Anything `card_matches_query` accepts by text should light something
        // up, so a filtered card never renders with zero marks.
        let c = card(1, "SSE card created in another browser context");
        for query in ["sse crd", "brwsr ctx", "created", "Another Browser"] {
            assert!(matches(&c, query), "{query} should match");
            assert!(
                !highlight_spans(&c.body, query).is_empty(),
                "{query} should highlight"
            );
        }
    }
}
