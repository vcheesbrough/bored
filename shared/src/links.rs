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

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

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

/// Why [`order_by_dependency`] could not produce an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderError {
    /// The edges restricted to the given cards contain a loop, so no order can
    /// satisfy them. Carries the ids that could not be placed — every card
    /// that is on, or downstream of, the loop.
    Cycle(Vec<String>),
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderError::Cycle(ids) => write!(
                f,
                "links form a loop; {} card(s) could not be ordered: {}",
                ids.len(),
                ids.join(", ")
            ),
        }
    }
}

/// Re-order `current` so that every card comes after all of its predecessors.
///
/// `current` is one column's card ids **in their present top-to-bottom order**,
/// and `edges` is every `(predecessor_id, successor_id)` pair the caller knows
/// about — typically the whole board's links. Edges with either end outside
/// `current` are dropped, which is exactly the "a link to a card in another
/// column is ignored" rule: a column is ordered against its own cards only.
///
/// # What "moves as little as possible" means here
///
/// This is Kahn's algorithm with the ready set held in a min-heap keyed by each
/// card's *current* index, so of all the orders that satisfy the links it picks
/// the one that is lexicographically smallest by current position. That is a
/// precise guarantee, and a weaker one than it may sound:
///
/// * a column that already satisfies its links comes back unchanged, so the
///   caller can compare and write nothing — the operation is idempotent;
/// * a card with no links never moves out of the way of a pair that is
///   swapping around it; it only shifts when a linked card must cross it.
///
/// It is deliberately **not** a claim of minimum total displacement or of the
/// fewest cards moved. Both of those are optimisation problems with no
/// tractable exact solution, and neither is what a user watching a column
/// re-settle actually expects.
///
/// # Errors
///
/// [`OrderError::Cycle`] when the restricted graph contains a loop. The API
/// refuses to create a link that would close one, so this is unreachable
/// through normal use — but the ordering is computed from stored rows, and
/// vouching for rows written by older code is not this function's job, so the
/// impossible case is returned rather than papered over.
pub fn order_by_dependency<'a, I>(current: &[&'a str], edges: I) -> Result<Vec<&'a str>, OrderError>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    // Work in terms of *indices* into `current`, never ids. A column should
    // never hold the same card twice, but if it somehow did, id-keyed nodes
    // would merge the duplicates and could report a loop where there is none;
    // index-keyed nodes make the second copy an isolated node that simply
    // keeps its place.
    let index_of: HashMap<&str, usize> = current
        .iter()
        .enumerate()
        // `.rev()` so that with a duplicate id the *first* occurrence wins,
        // matching "the copy nearest the top is the one links refer to".
        .rev()
        .map(|(i, id)| (*id, i))
        .collect();

    // Resolve and dedupe edges before counting in-degrees. A repeated link
    // (the same pair stored twice, or a pair reachable from both ends of the
    // caller's iterator) would otherwise inflate the successor's in-degree,
    // and a node whose in-degree never reaches zero is dropped from the output
    // — a silently wrong answer rather than a visible failure.
    let mut resolved: HashSet<(usize, usize)> = HashSet::new();
    for (from, to) in edges {
        let (Some(&from_idx), Some(&to_idx)) = (index_of.get(from), index_of.get(to)) else {
            // One or both ends live in another column (or nowhere).
            continue;
        };
        if from_idx == to_idx {
            // A self-edge cannot be satisfied and the API rejects one; ignore
            // it rather than reporting the whole column as a loop.
            continue;
        }
        resolved.insert((from_idx, to_idx));
    }

    let mut successors: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut in_degree: Vec<usize> = vec![0; current.len()];
    for &(from_idx, to_idx) in &resolved {
        successors.entry(from_idx).or_default().push(to_idx);
        in_degree[to_idx] += 1;
    }

    // Ready set: every card whose predecessors have all been placed. Holding
    // it in a min-heap of current indices — `Reverse` turns Rust's max-heap
    // into a min-heap — is the whole of the "stay put where you can" rule.
    let mut ready: BinaryHeap<Reverse<usize>> = (0..current.len())
        .filter(|&i| in_degree[i] == 0)
        .map(Reverse)
        .collect();

    let mut ordered: Vec<&'a str> = Vec::with_capacity(current.len());
    while let Some(Reverse(idx)) = ready.pop() {
        ordered.push(current[idx]);
        for &next in successors.get(&idx).into_iter().flatten() {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                ready.push(Reverse(next));
            }
        }
    }

    if ordered.len() != current.len() {
        // Whatever never reached in-degree zero is on a loop or downstream of
        // one. Report those ids so a caller logging the error can point at the
        // links that need untangling.
        let unplaced: Vec<String> = (0..current.len())
            .filter(|&i| in_degree[i] > 0)
            .map(|i| current[i].to_string())
            .collect();
        return Err(OrderError::Cycle(unplaced));
    }

    Ok(ordered)
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

    // ── dependency ordering ──────────────────────────────────────────────

    /// `order_by_dependency` over `&str` slices, returning owned `String`s so a
    /// test can compare against a plain vec literal.
    fn order(current: &[&str], links: &[(&str, &str)]) -> Result<Vec<String>, OrderError> {
        order_by_dependency(current, links.iter().copied())
            .map(|ids| ids.into_iter().map(str::to_string).collect())
    }

    #[test]
    fn empty_column_orders_to_nothing() {
        assert_eq!(order(&[], &[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn column_without_links_keeps_its_order() {
        assert_eq!(order(&["a", "b", "c"], &[]).unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn already_satisfied_order_is_unchanged() {
        // The idempotence guarantee the button depends on: nothing to write.
        let links = [("a", "b"), ("b", "c")];
        assert_eq!(order(&["a", "b", "c"], &links).unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn successor_above_predecessor_is_swapped() {
        assert_eq!(order(&["b", "a"], &[("a", "b")]).unwrap(), ["a", "b"]);
    }

    #[test]
    fn unlinked_card_above_the_pair_does_not_move() {
        // The minimal-movement case: x is unrelated and sits at the top, so the
        // swap of a and b must happen *below* it rather than shuffling x down.
        assert_eq!(
            order(&["x", "b", "a"], &[("a", "b")]).unwrap(),
            ["x", "a", "b"]
        );
    }

    #[test]
    fn unlinked_card_keeps_its_slot_when_the_pair_swaps_below_it() {
        assert_eq!(
            order(&["c", "b", "a"], &[("a", "b")]).unwrap(),
            ["c", "a", "b"]
        );
    }

    #[test]
    fn edge_to_a_card_outside_the_column_is_ignored() {
        // "zulu" lives in another column; the link must not move anything.
        assert_eq!(
            order(&["b", "a"], &[("zulu", "a"), ("b", "zulu")]).unwrap(),
            ["b", "a"]
        );
    }

    #[test]
    fn fully_inverted_chain_is_reversed() {
        let links = [("a", "b"), ("b", "c"), ("c", "d")];
        assert_eq!(
            order(&["d", "c", "b", "a"], &links).unwrap(),
            ["a", "b", "c", "d"]
        );
    }

    #[test]
    fn independent_successors_keep_their_relative_order() {
        // b and c both depend only on a, so their existing order decides theirs.
        let links = [("a", "b"), ("a", "c")];
        assert_eq!(
            order(&["c", "b", "a"], &links).unwrap(),
            ["a", "c", "b"],
            "c was above b before, so it stays above b"
        );
    }

    #[test]
    fn repeated_edge_is_counted_once() {
        // A doubled in-degree would leave "b" stuck and silently dropped.
        let links = [("a", "b"), ("a", "b")];
        assert_eq!(order(&["b", "a"], &links).unwrap(), ["a", "b"]);
    }

    #[test]
    fn self_edge_is_ignored_rather_than_treated_as_a_loop() {
        let links = [("a", "a"), ("a", "b")];
        assert_eq!(order(&["b", "a"], &links).unwrap(), ["a", "b"]);
    }

    #[test]
    fn loop_within_the_column_is_an_error() {
        let links = [("a", "b"), ("b", "a")];
        let Err(OrderError::Cycle(mut unplaced)) = order(&["a", "b"], &links) else {
            panic!("a loop must not produce an order");
        };
        unplaced.sort();
        assert_eq!(unplaced, ["a", "b"]);
    }

    #[test]
    fn sorting_an_already_sorted_column_is_a_no_op() {
        // Running the button twice must not shuffle anything the second time.
        let links = [("a", "c"), ("d", "b")];
        let once = order(&["c", "b", "a", "d"], &links).unwrap();
        let once_refs: Vec<&str> = once.iter().map(String::as_str).collect();
        let twice = order(&once_refs, &links).unwrap();
        assert_eq!(once, twice);
    }
}
