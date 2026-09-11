//! Card tag normalization — shared so the server and the browser agree on what
//! a tag *is* before one of them stores it and the other renders it.
//!
//! The rules, in one place:
//!
//! * a tag is a **whitespace-free token** — a value containing spaces is split
//!   into several tags rather than rejected, so pasting `bug urgent` into the
//!   tag input does the obvious thing;
//! * a leading `#` is stripped, because that is how tags are written in search
//!   (`#bug`) and users type what they see;
//! * tags are **trimmed** and empties dropped;
//! * duplicates are removed **case-insensitively**, keeping the first spelling
//!   the user typed (so `Bug` then `bug` stays a single `Bug`).
//!
//! Limits are enforced rather than silently applied: a too-long tag or an
//! over-long list is an error the caller surfaces, never a quiet truncation of
//! something the user asked for.

/// Longest single tag, in characters. Generous for a label, small enough that a
/// card's tag list can never become a body-sized payload by another name.
pub const MAX_TAG_CHARS: usize = 64;

/// Most tags one card may carry. Well past any real use; exists so a malicious
/// or buggy client cannot grow a card's audit snapshots without bound.
pub const MAX_TAGS_PER_CARD: usize = 32;

/// Why [`normalize`] rejected an input list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagError {
    /// A single tag exceeded [`MAX_TAG_CHARS`]; carries the offending tag.
    TooLong(String),
    /// More than [`MAX_TAGS_PER_CARD`] distinct tags after normalization.
    TooMany(usize),
}

impl std::fmt::Display for TagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TagError::TooLong(tag) => {
                write!(f, "tag {tag:?} is longer than {MAX_TAG_CHARS} characters")
            }
            TagError::TooMany(n) => write!(
                f,
                "{n} tags exceeds the limit of {MAX_TAGS_PER_CARD} per card"
            ),
        }
    }
}

/// Apply the tag rules above to a raw client-supplied list.
///
/// Returns the cleaned list in first-seen order, or a [`TagError`] when a limit
/// is exceeded.
pub fn normalize(raw: &[String]) -> Result<Vec<String>, TagError> {
    let mut out: Vec<String> = Vec::new();
    for candidate in raw {
        // One raw entry can yield several tags: whitespace is a separator, not
        // a character a tag is allowed to contain.
        for token in candidate.split_whitespace() {
            let tag = token.trim_start_matches('#');
            if tag.is_empty() {
                continue;
            }
            if tag.chars().count() > MAX_TAG_CHARS {
                return Err(TagError::TooLong(tag.to_string()));
            }
            // Case-insensitive dedup, first spelling wins.
            if out.iter().any(|existing| eq_ignore_case(existing, tag)) {
                continue;
            }
            out.push(tag.to_string());
        }
    }
    if out.len() > MAX_TAGS_PER_CARD {
        return Err(TagError::TooMany(out.len()));
    }
    Ok(out)
}

/// Case-insensitive tag comparison. Tags are compared the way users think of
/// them — `Bug` and `bug` are the same tag — using Unicode-aware lowercasing so
/// non-ASCII tags behave like ASCII ones.
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// True when `tag` starts with `prefix`, ignoring case. Powers both the search
/// filter (`#bu` narrows to `bug`) and the tag suggestion popups.
pub fn starts_with_ignore_case(tag: &str, prefix: &str) -> bool {
    let tag_lower: String = tag.chars().flat_map(char::to_lowercase).collect();
    let prefix_lower: String = prefix.chars().flat_map(char::to_lowercase).collect();
    tag_lower.starts_with(&prefix_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn trims_and_drops_empties() {
        assert_eq!(normalize(&v(&["  bug  ", "   ", ""])).unwrap(), v(&["bug"]));
    }

    #[test]
    fn strips_leading_hash() {
        assert_eq!(normalize(&v(&["#bug"])).unwrap(), v(&["bug"]));
    }

    #[test]
    fn splits_whitespace_into_separate_tags() {
        assert_eq!(
            normalize(&v(&["bug urgent"])).unwrap(),
            v(&["bug", "urgent"])
        );
    }

    #[test]
    fn dedups_case_insensitively_keeping_first_spelling() {
        assert_eq!(normalize(&v(&["Bug", "bug", "BUG"])).unwrap(), v(&["Bug"]));
    }

    #[test]
    fn preserves_order_of_first_appearance() {
        assert_eq!(
            normalize(&v(&["zeta", "alpha", "zeta"])).unwrap(),
            v(&["zeta", "alpha"])
        );
    }

    #[test]
    fn rejects_an_over_long_tag() {
        let long = "x".repeat(MAX_TAG_CHARS + 1);
        assert!(matches!(normalize(&v(&[&long])), Err(TagError::TooLong(_))));
        // Exactly at the limit is fine.
        let at_limit = "x".repeat(MAX_TAG_CHARS);
        assert!(normalize(&v(&[&at_limit])).is_ok());
    }

    #[test]
    fn rejects_too_many_tags() {
        let many: Vec<String> = (0..=MAX_TAGS_PER_CARD).map(|i| format!("t{i}")).collect();
        assert!(matches!(normalize(&many), Err(TagError::TooMany(_))));
    }

    #[test]
    fn duplicates_do_not_count_toward_the_limit() {
        // 40 entries, but only one distinct tag.
        let dupes: Vec<String> = std::iter::repeat_n("same".to_string(), 40).collect();
        assert_eq!(normalize(&dupes).unwrap(), v(&["same"]));
    }

    #[test]
    fn long_tag_is_measured_in_chars_not_bytes() {
        // Multi-byte characters must not shorten the effective limit.
        let emoji = "é".repeat(MAX_TAG_CHARS);
        assert!(normalize(&v(&[&emoji])).is_ok());
    }

    #[test]
    fn prefix_match_is_case_insensitive() {
        assert!(starts_with_ignore_case("Bug", "bu"));
        assert!(starts_with_ignore_case("bug", "BUG"));
        assert!(!starts_with_ignore_case("bug", "ug"));
        // An empty prefix matches everything — the "just typed #" state.
        assert!(starts_with_ignore_case("bug", ""));
    }
}
