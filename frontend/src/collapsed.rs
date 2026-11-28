//! Which of the board's columns are collapsed — one board-level record that
//! every part of the board can read (card #649).
//!
//! **Before this module** each `ColumnView` owned its own collapse flag: it
//! read `localStorage` once on mount into a private `RwSignal<bool>`, and its
//! collapse/expand buttons wrote that signal and the stored set. Nothing
//! outside the column could see whether it was collapsed. The board link graph
//! (card #303) needs exactly that, so the state is lifted here: `BoardView`
//! owns one [`CollapsedColumns`] and provides it as a context, and each column
//! *derives* its flag from it.
//!
//! **Storage is unchanged** — the same key (`bored:collapsed-columns:<board
//! ULID>`) and the same format (a JSON array of column ids) — so columns a user
//! collapsed before the upgrade are still collapsed after it.
//!
//! As in [`crate::recent`], the in-memory signal is the source of truth while
//! the board is open (a derived signal cannot observe a storage write), and
//! storage is read once per board in [`CollapsedColumns::load`] and written
//! through on every change. The rules themselves are pure functions over
//! plain sets and strings at the bottom of the file, so they can be
//! unit-tested on the host target, where there is no `window()` to call.

use std::collections::HashSet;

use leptos::prelude::*;

/// Storage key prefix, suffixed with the board's ULID so each board in the
/// browser keeps its own set. Must never change: it is what carries existing
/// users' collapsed columns across upgrades.
const STORAGE_PREFIX: &str = "bored:collapsed-columns:";

/// The board-level collapse state, provided as a context by `BoardView`.
///
/// Both fields are `RwSignal`s, which are `Copy` handles into Leptos's reactive
/// arena — so the struct itself is `Copy`, and every closure that needs it can
/// capture its own copy by `move` without any `.clone()` bookkeeping.
///
/// The card sketched this as a newtype over the set alone; it carries the
/// board id as well because persisting needs to know *which* board's key to
/// write, and the id is the same `board_ulid` signal `BoardView` already holds
/// (the same shape as [`crate::recent::RecentPicks`]).
#[derive(Clone, Copy)]
pub struct CollapsedColumns {
    /// The board these ids belong to. Empty until the board's initial fetch
    /// resolves its ULID, and blanked again on every navigation.
    pub board_id: RwSignal<String>,
    /// Ids of the collapsed columns on that board.
    pub ids: RwSignal<HashSet<String>>,
}

impl CollapsedColumns {
    /// No column collapsed yet, for a board not yet resolved.
    pub fn new(board_id: RwSignal<String>) -> Self {
        Self {
            board_id,
            ids: RwSignal::new(HashSet::new()),
        }
    }

    /// Replace the in-memory set with the one stored for the current board.
    ///
    /// Meant to run inside an `Effect`: it reads `board_id` *tracked*, so the
    /// effect re-runs whenever the board changes. That matters because
    /// `BoardView` is reused across `/boards/:slug` URLs — the component is not
    /// re-mounted when the user switches boards, so a load-on-mount would keep
    /// showing the first board's set. Navigation blanks the ULID before setting
    /// the new one, and the blank clears the set, so one board's collapsed ids
    /// never leak into the next board's columns even for a frame.
    pub fn load(&self) {
        let board_id = self.board_id.get();
        if board_id.is_empty() {
            self.ids.set(HashSet::new());
            return;
        }
        self.ids.set(parse_stored(
            read_storage(&storage_key(&board_id)).as_deref(),
        ));
    }

    /// Collapse (`true`) or expand (`false`) one column, and persist it.
    ///
    /// Every access is a `try_*` form: a click handler runs while its column is
    /// alive, but [`Self::forget`] reaches here from `columns::remove`, which
    /// the board chooser calls after an `await` — by then the board may have
    /// been unmounted and these signals disposed, and reading a disposed signal
    /// panics, which wedges the whole WASM tab.
    pub fn set_collapsed(&self, column_id: &str, collapsed: bool) {
        // Write the signal only when membership actually changes. An `update`
        // notifies every subscriber even when the closure changes nothing, and
        // each column subscribes through its `is_collapsed` memo — so a no-op
        // write would wake every column on the board.
        let changed = self
            .ids
            .try_with_untracked(|ids| ids.contains(column_id) != collapsed)
            .unwrap_or(false);
        if changed {
            let _ = self.ids.try_update(|ids| apply(ids, column_id, collapsed));
        }

        // No board resolved (or the board is gone): there is no key to write
        // to. Checked *before* touching storage, which also keeps `window()` —
        // unavailable on the host target — out of the unit tests.
        let board_id = self.board_id.try_get_untracked().unwrap_or_default();
        if board_id.is_empty() {
            return;
        }
        persist(&storage_key(&board_id), column_id, collapsed);
    }

    /// A column has been deleted: drop its id so the stored set does not grow
    /// with ids that can never be shown again. Exactly an expand — including
    /// that it writes nothing when the id was not there.
    pub fn forget(&self, column_id: &str) {
        self.set_collapsed(column_id, false);
    }
}

/// Read-modify-write one column's membership in the stored set.
///
/// It re-reads storage rather than writing the in-memory set back wholesale,
/// which is what the per-column code did before this module and keeps two
/// tabs on the same board from clobbering each other: collapsing column A in
/// one tab and column B in another leaves both collapsed in storage, where
/// writing each tab's whole set would keep only the last writer's.
fn persist(key: &str, column_id: &str, collapsed: bool) {
    // `let … else`: bail out quietly when the browser refuses storage
    // (private mode, a sandboxed iframe). Collapse then simply lasts until the
    // page is reloaded, which is the most a storage-less browser can offer.
    let Ok(Some(storage)) = window().local_storage() else {
        return;
    };
    let mut stored = parse_stored(storage.get_item(key).ok().flatten().as_deref());
    if !apply(&mut stored, column_id, collapsed) {
        // Already as asked (an expand of a column never collapsed, a delete
        // of one never collapsed): leave storage alone.
        return;
    }
    match serialise(&stored) {
        Some(value) => {
            let _ = storage.set_item(key, &value);
        }
        // An empty set removes the key rather than storing `[]`, so a board
        // with nothing collapsed leaves no entry behind.
        None => {
            let _ = storage.remove_item(key);
        }
    }
}

/// The stored value for `key`, if storage is available and the key is set.
fn read_storage(key: &str) -> Option<String> {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|storage| storage.get_item(key).ok().flatten())
}

// ── Pure helpers (host-testable) ────────────────────────────────────────────

/// The `localStorage` key holding `board_id`'s collapsed columns.
pub fn storage_key(board_id: &str) -> String {
    format!("{STORAGE_PREFIX}{board_id}")
}

/// Parse a stored value into a set of column ids.
///
/// `None` (no entry) and anything that is not a JSON array of strings — a
/// hand-edited value, a format some future version wrote — both mean "nothing
/// collapsed". Failing safe matters here: a stored value that panicked or
/// errored would take the whole board down over a cosmetic preference.
pub fn parse_stored(value: Option<&str>) -> HashSet<String> {
    value
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_default()
}

/// The value to store for `ids`, or `None` when the set is empty (the caller
/// removes the key instead).
///
/// The ids are sorted first. A `HashSet` iterates in an arbitrary order, so
/// serialising it directly would write a different string for the same set
/// from one run to the next; sorting makes the output stable (and testable)
/// while staying the same JSON-array format older versions wrote and read.
pub fn serialise(ids: &HashSet<String>) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    // `Vec<&String>` borrows the ids rather than cloning them; serde
    // serialises a `&String` exactly like a `String`.
    let mut sorted: Vec<&String> = ids.iter().collect();
    sorted.sort();
    serde_json::to_string(&sorted).ok()
}

/// Put `column_id` in (`collapsed`) or take it out of (`!collapsed`) `ids`.
/// Returns whether the set changed, so callers can skip a pointless write.
pub fn apply(ids: &mut HashSet<String>, column_id: &str, collapsed: bool) -> bool {
    if collapsed {
        // `insert` takes an owned `String`, hence the `to_owned`; it returns
        // `false` when the id was already present.
        ids.insert(column_id.to_owned())
    } else {
        // `remove` accepts a `&str` for a `HashSet<String>` (via `Borrow`),
        // so no allocation is needed to take an id out.
        ids.remove(column_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set from literals, so expected values are written out rather than
    /// computed by the code under test.
    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_storage_key_is_the_one_older_versions_used() {
        // Changing it would silently expand every column every user collapsed.
        assert_eq!(
            storage_key("01KPHF6GVRMRSHJKCQ3FSEPCZJ"),
            "bored:collapsed-columns:01KPHF6GVRMRSHJKCQ3FSEPCZJ"
        );
    }

    #[test]
    fn a_stored_array_parses_into_its_ids() {
        assert_eq!(parse_stored(Some(r#"["a","b"]"#)), set(&["a", "b"]));
    }

    #[test]
    fn no_stored_value_is_an_empty_set() {
        assert_eq!(parse_stored(None), set(&[]));
    }

    #[test]
    fn a_malformed_stored_value_is_an_empty_set() {
        assert_eq!(parse_stored(Some("not json")), set(&[]));
        assert_eq!(parse_stored(Some(r#"{"a":true}"#)), set(&[]));
        assert_eq!(parse_stored(Some("[1,2]")), set(&[]));
        assert_eq!(parse_stored(Some("")), set(&[]));
    }

    #[test]
    fn serialising_writes_a_sorted_json_array() {
        assert_eq!(
            serialise(&set(&["c", "a", "b"])).as_deref(),
            Some(r#"["a","b","c"]"#)
        );
    }

    #[test]
    fn an_empty_set_serialises_to_nothing() {
        assert_eq!(serialise(&set(&[])), None);
    }

    #[test]
    fn collapsing_adds_the_id() {
        let mut ids = set(&["a"]);
        assert!(apply(&mut ids, "b", true));
        assert_eq!(ids, set(&["a", "b"]));
    }

    #[test]
    fn collapsing_an_already_collapsed_column_changes_nothing() {
        let mut ids = set(&["a"]);
        assert!(!apply(&mut ids, "a", true));
        assert_eq!(ids, set(&["a"]));
    }

    #[test]
    fn expanding_removes_the_id() {
        let mut ids = set(&["a", "b"]);
        assert!(apply(&mut ids, "a", false));
        assert_eq!(ids, set(&["b"]));
    }

    #[test]
    fn expanding_a_column_never_collapsed_changes_nothing() {
        let mut ids = set(&["a"]);
        assert!(!apply(&mut ids, "zzz", false));
        assert_eq!(ids, set(&["a"]));
    }

    #[test]
    fn forgetting_a_deleted_column_drops_its_id() {
        // No board id, so nothing reaches storage (there is no `window()` on
        // the host) — this is the in-memory half of a delete.
        let owner = Owner::new();
        let collapsed = owner.with(|| CollapsedColumns::new(RwSignal::new(String::new())));
        collapsed.ids.set(set(&["doomed", "kept"]));
        collapsed.forget("doomed");
        assert_eq!(collapsed.ids.get_untracked(), set(&["kept"]));
    }

    #[test]
    fn toggling_through_the_context_updates_the_set() {
        let owner = Owner::new();
        let collapsed = owner.with(|| CollapsedColumns::new(RwSignal::new(String::new())));
        collapsed.set_collapsed("a", true);
        assert_eq!(collapsed.ids.get_untracked(), set(&["a"]));
        collapsed.set_collapsed("a", false);
        assert_eq!(collapsed.ids.get_untracked(), set(&[]));
    }

    #[test]
    fn a_no_op_toggle_does_not_wake_readers() {
        // Every column reads the set through a memo; an expand of a column
        // that was never collapsed must not make all of them recompute. A
        // lazy `Memo` re-runs on its next read only if a signal it read was
        // written, so its run count observes notifications synchronously.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let owner = Owner::new();
        let collapsed = owner.with(|| CollapsedColumns::new(RwSignal::new(String::new())));
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let memo = owner.with(|| {
            Memo::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                collapsed.ids.with(|ids| ids.len())
            })
        });
        memo.get_untracked();
        collapsed.set_collapsed("never-collapsed", false);
        memo.get_untracked();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no write, no wake-up");
        // Positive control: a real change is seen.
        collapsed.set_collapsed("a", true);
        assert_eq!(memo.get_untracked(), 1);
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn toggling_after_the_board_is_gone_does_not_panic() {
        // `columns::remove` calls `forget` after the chooser's `await`; the
        // board's signals may be disposed by then.
        let owner = Owner::new();
        let collapsed = owner.with(|| CollapsedColumns::new(RwSignal::new("board".to_string())));
        owner.cleanup();
        collapsed.forget("doomed");
        collapsed.set_collapsed("other", true);
    }
}
