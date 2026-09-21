//! Shared mutations for the board's column list.
//!
//! The column list lives in one place — `pages::board_view` owns
//! `RwSignal<Vec<RwSignal<shared::Column>>>` and hands it out via context — but
//! it is written from two different places: the board chooser, when the user
//! creates a column, and the SSE handler, when the server broadcasts a create.
//! Both write the *same* new column, so the rules for inserting it belong here
//! rather than being duplicated (and drifting) at each call site.

use leptos::prelude::*;

/// Inserts `column` into `columns` at its sorted position, unless an entry with
/// the same id is already present.
///
/// The dedup is the important part. Creating a column produces two deliveries of
/// it — the `201` response body and the SSE `ColumnCreated` broadcast — and the
/// broadcast is sent before the response reaches the browser, so it usually
/// arrives *first*. If both paths insert unconditionally, `columns` ends up
/// holding two separate signals carrying the same `Column.id`. The board and the
/// chooser both render the list through a keyed `<For>` whose key is that id, and
/// keyed diffing with duplicate keys misrenders: it repeats a name it has already
/// drawn and silently drops columns created afterwards, so the board only looks
/// right again after a reload.
///
/// `components::column::on_card_created` guards the same race for cards.
///
/// `owner` must outlive the list itself — pass the owner of the view that holds
/// it. A signal belongs to whichever owner is active when it is created, and an
/// `Effect`'s owner is disposed every time that effect re-runs, so a column
/// signal created directly inside the SSE handler is dropped the moment the next
/// event arrives. Reading a disposed signal panics, which kills the reactive
/// runtime: the board then ignores its own events and only looks right again
/// after a reload.
pub fn insert_absent(
    owner: &Owner,
    columns: RwSignal<Vec<RwSignal<shared::Column>>>,
    column: shared::Column,
) {
    owner.with(|| {
        columns.update(|cs| {
            if cs
                .iter()
                .any(|existing| existing.get_untracked().id == column.id)
            {
                return;
            }
            // `list_columns` returns `ORDER BY position ASC`, so inserting before
            // the first column with a higher position keeps the client list in
            // the same order the server would send on a reload.
            let insert_at = cs
                .iter()
                .position(|existing| existing.get_untracked().position > column.position)
                .unwrap_or(cs.len());
            cs.insert(insert_at, RwSignal::new(column));
        });
    });
}

/// The position to request for a column appended after the current last one:
/// one past the highest position in use.
///
/// Counting the entries instead (`Vec::len()`) does not work. Positions are
/// sparse — reordering and older data leave gaps, and columns can even share a
/// position — so a count frequently names a slot another column already holds,
/// which makes the new column sort into the middle of the board.
pub fn next_position(existing: &[i32]) -> i32 {
    existing
        .iter()
        .copied()
        .max()
        // `saturating_add` so a column already parked at `i32::MAX` cannot wrap
        // round to a negative position and jump to the front of the board.
        .map_or(0, |highest| highest.saturating_add(1))
}

/// Where `id` sits in `order`, for use as a sort key. An id `order` does not
/// mention ranks last.
///
/// Pure, and over plain strings, so the ordering rule can be tested on the host
/// target without a reactive runtime.
pub fn rank_in(order: &[String], id: &str) -> usize {
    order
        .iter()
        .position(|candidate| candidate == id)
        .unwrap_or(usize::MAX)
}

/// Put the column list into `order` — the undo for an optimistic reorder the
/// server did not accept.
///
/// Dragging a column reorders the list locally *before* the request goes out, so
/// the drop feels instant. When that request is refused (the tab is
/// disconnected — see [`crate::connection`]) or fails (a 5xx), the list has to
/// go back, or the board keeps showing an order the server never agreed to
/// until the next reload. `order` is the server's own answer when it can be
/// fetched, and the pre-drag snapshot when it cannot (see the caller).
///
/// Only the *order* is changed, by sorting the list as it is now. Putting a
/// saved copy of a list back wholesale would be simpler and wrong: a request
/// can take a while to fail, and a column created or deleted over SSE in the
/// meantime would be dropped or resurrected by the stale copy. Sorting cannot
/// add or remove an entry, and the sort is stable, so a column that appeared
/// since simply keeps its place after the ones `order` knows about.
///
/// `try_update`, because this runs after an `await`: the board may have been
/// unmounted while the request was in flight, and writing a disposed signal
/// panics.
pub fn restore_order(columns: RwSignal<Vec<RwSignal<shared::Column>>>, order: &[String]) {
    let _ = columns.try_update(|cs| {
        cs.sort_by_key(|column| rank_in(order, &column.get_untracked().id));
    });
}

#[cfg(test)]
mod tests {
    use super::{next_position, rank_in};

    /// Sort `current` the way [`super::restore_order`] sorts the signal list —
    /// same key, same stable sort — without needing a reactive runtime.
    fn restored(current: &[&str], previous: &[&str]) -> Vec<String> {
        let previous: Vec<String> = previous.iter().map(|id| id.to_string()).collect();
        let mut current: Vec<String> = current.iter().map(|id| id.to_string()).collect();
        current.sort_by_key(|id| rank_in(&previous, id));
        current
    }

    #[test]
    fn a_refused_reorder_goes_back_to_the_previous_order() {
        // The user dragged `c` to the front; the server said no.
        assert_eq!(
            restored(&["c", "a", "b"], &["a", "b", "c"]),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn a_column_created_meanwhile_is_kept_and_sorts_last() {
        // `d` arrived over SSE while the request was in flight. It is not in
        // the snapshot, so it must survive the undo rather than vanish.
        assert_eq!(
            restored(&["c", "a", "d", "b"], &["a", "b", "c"]),
            ["a", "b", "c", "d"]
        );
    }

    #[test]
    fn a_column_deleted_meanwhile_is_not_resurrected() {
        // `b` was deleted over SSE while the request was in flight. The
        // snapshot still names it; the undo must not bring it back.
        assert_eq!(restored(&["c", "a"], &["a", "b", "c"]), ["a", "c"]);
    }

    #[test]
    fn columns_unknown_to_the_snapshot_keep_their_relative_order() {
        // Stable sort: `y` was ahead of `x` before the undo and still is.
        assert_eq!(
            restored(&["y", "b", "x", "a"], &["a", "b"]),
            ["a", "b", "y", "x"]
        );
    }

    #[test]
    fn first_column_starts_at_zero() {
        assert_eq!(next_position(&[]), 0);
    }

    #[test]
    fn appends_after_the_highest_position() {
        assert_eq!(next_position(&[0, 1, 2]), 3);
    }

    #[test]
    fn ignores_list_order() {
        assert_eq!(next_position(&[2, 0, 1]), 3);
    }

    #[test]
    fn skips_past_sparse_gaps() {
        // The count (2) would collide with nothing here, but it would place the
        // new column *before* the existing one at 99999.
        assert_eq!(next_position(&[0, 99999]), 100_000);
    }

    #[test]
    fn tolerates_repeated_positions() {
        assert_eq!(next_position(&[99999, 99999]), 100_000);
    }

    #[test]
    fn saturates_instead_of_wrapping() {
        assert_eq!(next_position(&[i32::MAX]), i32::MAX);
    }
}
