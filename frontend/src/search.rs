use leptos::prelude::*;

#[derive(Clone, Copy)]
pub struct BoardSearchQuery(pub RwSignal<String>);

pub fn card_matches_query(card: &shared::Card, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    if let Some(number_query) = query.strip_prefix('#') {
        if !number_query.is_empty() && number_query.chars().all(|c| c.is_ascii_digit()) {
            return number_query
                .parse::<u32>()
                .is_ok_and(|number| number == card.number);
        }
    }
    if query.chars().all(|c| c.is_ascii_digit())
        && query
            .parse::<u32>()
            .is_ok_and(|number| number == card.number)
    {
        return true;
    }

    let haystack = normalize(&card.body);
    let needle = normalize(query);
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

/// Returns `true` when `query` is a card-number search (`#42` or `42`) that
/// matches `number`.
///
/// The card badge is highlighted for these queries; the body is not, because a
/// number search deliberately ignores body text (see `card_matches_query`).
pub fn query_matches_number(number: u32, query: &str) -> bool {
    let query = query.trim();
    let digits = query.strip_prefix('#').unwrap_or(query);
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && digits.parse::<u32>().is_ok_and(|parsed| parsed == number)
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
    let query = query.trim();
    // `#42` is a pure number search: `card_matches_query` never consults the
    // body for it, so marking body text would be misleading.
    if query.starts_with('#') {
        return Vec::new();
    }

    let terms = terms_of(query);
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
        shared::Card {
            id: "card".to_string(),
            column_id: "column".to_string(),
            body: body.to_string(),
            position: 0,
            number,
            last_edited_by: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
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
