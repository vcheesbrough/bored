use leptos::prelude::*;

#[derive(Clone, Copy)]
pub struct BoardSearchQuery(pub RwSignal<String>);

pub fn card_matches_query(card: &shared::Card, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    let number_query = query.strip_prefix('#').unwrap_or(query);
    if !number_query.is_empty()
        && number_query.chars().all(|c| c.is_ascii_digit())
        && number_query
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
    fn matches_case_insensitive_substrings() {
        assert!(card_matches_query(&card(1, "Deploy Preview"), "deploy"));
    }

    #[test]
    fn matches_fuzzy_word_subsequences() {
        let c = card(1, "SSE card created in another browser context");
        assert!(card_matches_query(&c, "sse crd"));
        assert!(card_matches_query(&c, "brwsr ctx"));
    }
}
