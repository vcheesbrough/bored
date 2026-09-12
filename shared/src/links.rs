//! Card link rules — shared so the server and the browser agree on what a
//! link *may* be before one of them stores it and the other offers it.
//!
//! A link is one stored fact with two ends: a **predecessor** card that comes
//! before a **successor** card. Adding "A is a predecessor of B" and "B is a
//! successor of A" are the same operation and must produce the same row, so
//! nothing in here cares which end the user started from.
//!
//! The rules, in one place:
//!
//! * the optional **reason** is trimmed, and an empty reason is the same as no
//!   reason at all — there is no distinct "present but blank" state;
//! * a reason longer than [`MAX_REASON_CHARS`] is an error, never a silent
//!   truncation of something the user typed;
//! * a link may never form a **loop**, including a card linked to itself and a
//!   reciprocal pair. "Comes before" means nothing once it can circle back.
//!
//! The cycle check is deliberately a plain function over `(predecessor,
//! successor)` id pairs rather than over any concrete link type: the backend
//! feeds it database rows, the frontend feeds it the board's in-memory link
//! index, and both must reach the same verdict.

use std::collections::{HashMap, HashSet, VecDeque};

/// Longest reason, in characters. Room for a sentence, not a paragraph — the
/// reason is a tooltip-sized note on *why* one card precedes another, and the
/// card body is the place for anything longer.
pub const MAX_REASON_CHARS: usize = 200;

/// Why [`normalize_reason`] rejected an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasonError {
    /// The trimmed reason exceeded [`MAX_REASON_CHARS`]; carries its length.
    TooLong(usize),
}

impl std::fmt::Display for ReasonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReasonError::TooLong(n) => write!(
                f,
                "reason is {n} characters, longer than the limit of {MAX_REASON_CHARS}"
            ),
        }
    }
}

/// Apply the reason rules above to a raw client-supplied value.
///
/// Returns `Ok(None)` for an absent, empty, or whitespace-only reason, and
/// `Ok(Some(trimmed))` otherwise. The length limit is measured in characters,
/// not bytes, so multi-byte text is not shortchanged.
pub fn normalize_reason(raw: Option<&str>) -> Result<Option<String>, ReasonError> {
    let trimmed = raw.map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let len = trimmed.chars().count();
    if len > MAX_REASON_CHARS {
        return Err(ReasonError::TooLong(len));
    }
    Ok(Some(trimmed.to_string()))
}

/// True when adding the edge `predecessor → successor` to the graph described
/// by `edges` would close a loop.
///
/// `edges` is every existing `(predecessor_id, successor_id)` pair on the
/// board. The new edge creates a cycle exactly when the successor can already
/// reach the predecessor by following existing edges forward — including the
/// trivial case where the two ids are the same card. The walk is a breadth-first
/// search from the successor along successor edges; it stops early the moment
/// the predecessor is reached, and a `visited` set keeps it from looping on a
/// graph that somehow already contains a cycle.
pub fn would_create_cycle<'a, I>(edges: I, predecessor: &str, successor: &str) -> bool
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    if predecessor == successor {
        return true;
    }

    // Adjacency list keyed by predecessor: "which cards come directly after
    // this one". Built once so the BFS below is linear in the number of edges.
    let mut next: HashMap<&str, Vec<&str>> = HashMap::new();
    for (from, to) in edges {
        next.entry(from).or_default().push(to);
    }

    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    queue.push_back(successor);
    visited.insert(successor);

    while let Some(current) = queue.pop_front() {
        if current == predecessor {
            return true;
        }
        for &following in next.get(current).into_iter().flatten() {
            if visited.insert(following) {
                queue.push_back(following);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── reason ───────────────────────────────────────────────────────────

    #[test]
    fn absent_reason_is_none() {
        assert_eq!(normalize_reason(None).unwrap(), None);
    }

    #[test]
    fn empty_and_whitespace_reasons_are_none() {
        assert_eq!(normalize_reason(Some("")).unwrap(), None);
        assert_eq!(normalize_reason(Some("   \n\t ")).unwrap(), None);
    }

    #[test]
    fn reason_is_trimmed() {
        assert_eq!(
            normalize_reason(Some("  needs the API first  ")).unwrap(),
            Some("needs the API first".to_string())
        );
    }

    #[test]
    fn over_long_reason_is_rejected() {
        let long = "x".repeat(MAX_REASON_CHARS + 1);
        assert_eq!(
            normalize_reason(Some(&long)),
            Err(ReasonError::TooLong(MAX_REASON_CHARS + 1))
        );
        // Exactly at the limit is fine.
        let at_limit = "x".repeat(MAX_REASON_CHARS);
        assert!(normalize_reason(Some(&at_limit)).is_ok());
    }

    #[test]
    fn reason_length_is_measured_in_chars_not_bytes() {
        // Multi-byte characters must not shorten the effective limit.
        let accented = "é".repeat(MAX_REASON_CHARS);
        assert!(normalize_reason(Some(&accented)).is_ok());
    }

    #[test]
    fn surrounding_whitespace_does_not_count_toward_the_limit() {
        let padded = format!("   {}   ", "x".repeat(MAX_REASON_CHARS));
        assert!(normalize_reason(Some(&padded)).is_ok());
    }

    // ── cycles ───────────────────────────────────────────────────────────

    fn edges<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Iterator<Item = (&'a str, &'a str)> {
        pairs.iter().copied()
    }

    #[test]
    fn empty_graph_never_cycles() {
        assert!(!would_create_cycle(edges(&[]), "a", "b"));
    }

    #[test]
    fn self_link_is_a_cycle() {
        assert!(would_create_cycle(edges(&[]), "a", "a"));
    }

    #[test]
    fn reciprocal_pair_is_a_cycle() {
        // a → b exists; adding b → a closes the loop.
        assert!(would_create_cycle(edges(&[("a", "b")]), "b", "a"));
    }

    #[test]
    fn duplicate_edge_is_not_a_cycle() {
        // Re-adding a → b is a uniqueness problem, not a cycle — the caller
        // handles it separately so the two get different error codes.
        assert!(!would_create_cycle(edges(&[("a", "b")]), "a", "b"));
    }

    #[test]
    fn longer_loop_is_detected() {
        // a → b → c; adding c → a closes a three-card loop.
        assert!(would_create_cycle(
            edges(&[("a", "b"), ("b", "c")]),
            "c",
            "a"
        ));
    }

    #[test]
    fn transitive_forward_edge_is_fine() {
        // a → b → c; a → c is redundant but points the same way.
        assert!(!would_create_cycle(
            edges(&[("a", "b"), ("b", "c")]),
            "a",
            "c"
        ));
    }

    #[test]
    fn unrelated_component_is_ignored() {
        assert!(!would_create_cycle(
            edges(&[("x", "y"), ("y", "z")]),
            "a",
            "b"
        ));
    }

    #[test]
    fn diamond_is_not_a_cycle() {
        // a → b, a → c, b → d, c → d: two paths to d, none back to a.
        let g = [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")];
        assert!(!would_create_cycle(edges(&g), "a", "d"));
        assert!(would_create_cycle(edges(&g), "d", "a"));
    }

    #[test]
    fn terminates_on_a_graph_that_already_loops() {
        // Defensive: a pre-existing loop elsewhere must not hang the search.
        let g = [("x", "y"), ("y", "x")];
        assert!(!would_create_cycle(edges(&g), "a", "x"));
        assert!(would_create_cycle(edges(&g), "y", "x"));
    }
}
