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

pub fn card_matches_query(card: &shared::Card, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    let parsed = parse_query(query);

    // `#42` is an exact card-number term — it deliberately ignores body text.
    if !parsed.numbers.iter().all(|n| *n == card.number) {
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

    // A bare number matches the card number *or* the body, unlike `#42`.
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

    #[test]
    fn matches_empty_query() {
        assert!(card_matches_query(&card(42, "Deploy preview"), "  "));
    }

    #[test]
    fn matches_card_number_with_or_without_hash() {
        let c = card(42, "Deploy preview");
        assert!(card_matches_query(&c, "#42"));
        assert!(card_matches_query(&c, "42"));
        assert!(!card_matches_query(&c, "#41"));
    }

    #[test]
    fn hash_prefixed_number_does_not_match_body_text() {
        assert!(!card_matches_query(&card(7, "See #42 in the notes"), "#42"));
    }

    #[test]
    fn matches_case_insensitive_substrings() {
        assert!(card_matches_query(&card(1, "Deploy Preview"), "deploy"));
    }

    #[test]
    fn matches_fuzzy_word_subsequences() {
        let c = card(1, "SSE card created in another browser context");
        assert!(card_matches_query(&c, "sse crd"));
        assert!(card_matches_query(&c, "brwsr ctx"));
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
        assert!(card_matches_query(&card(1, "anything"), "#"));
    }

    #[test]
    fn tag_token_filters_by_tag() {
        let tagged = tagged_card(1, "Deploy preview", &["bug"]);
        let untagged = card(2, "Deploy preview");
        assert!(card_matches_query(&tagged, "#bug"));
        assert!(!card_matches_query(&untagged, "#bug"));
    }

    #[test]
    fn tag_token_matches_a_prefix_case_insensitively() {
        // Filtering narrows as the tag is typed, and `Bug` == `bug`.
        let tagged = tagged_card(1, "body", &["Bug"]);
        assert!(card_matches_query(&tagged, "#bu"));
        assert!(card_matches_query(&tagged, "#BUG"));
        assert!(!card_matches_query(&tagged, "#ug"));
    }

    #[test]
    fn multiple_tag_tokens_are_anded() {
        let both = tagged_card(1, "body", &["bug", "urgent"]);
        let one = tagged_card(2, "body", &["bug"]);
        assert!(card_matches_query(&both, "#bug #urgent"));
        assert!(!card_matches_query(&one, "#bug #urgent"));
    }

    #[test]
    fn tag_and_free_text_are_anded() {
        let match_both = tagged_card(1, "Deploy preview", &["bug"]);
        let wrong_text = tagged_card(2, "Something else", &["bug"]);
        let wrong_tag = tagged_card(3, "Deploy preview", &["chore"]);
        assert!(card_matches_query(&match_both, "#bug deploy"));
        assert!(!card_matches_query(&wrong_text, "#bug deploy"));
        assert!(!card_matches_query(&wrong_tag, "#bug deploy"));
    }

    #[test]
    fn number_token_still_ignores_body_text_when_combined_with_a_tag() {
        let c = tagged_card(7, "See 42 in the notes", &["bug"]);
        assert!(!card_matches_query(&c, "#42 #bug"));
        assert!(card_matches_query(&c, "#7 #bug"));
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
            assert!(card_matches_query(&c, query), "{query} should match");
            assert!(
                !highlight_spans(&c.body, query).is_empty(),
                "{query} should highlight"
            );
        }
    }
}
