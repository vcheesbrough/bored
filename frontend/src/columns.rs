//! Shared mutations for the board's column list.
//!
//! The column list lives in one place — `pages::board_view` owns
//! `RwSignal<Vec<RwSignal<shared::Column>>>` and hands it out via context — but
//! it is written from two different places: the board chooser, when the user
//! creates a column, and the SSE handler, when the server broadcasts a create.
//! Both write the *same* new column, so the rules for inserting it belong here
//! rather than being duplicated (and drifting) at each call site. The same
//! holds for deleting one: see [`remove`].

use leptos::prelude::*;

use crate::links::BoardLinkIndex;
use crate::search::BoardCardIndex;

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

/// Removes the column `column_id` from `columns`, and first drops from the
/// board's link index every link touching one of that column's cards.
///
/// Like [`insert_absent`], this is the one rule for two writers: the board
/// chooser, once its own `DELETE` has answered, and the SSE `ColumnDeleted`
/// handler. Both used to retain the column out of the list and nothing more.
///
/// **Why the links need pruning here (card #371).** Deleting a column cascades
/// on the server to its cards and every link touching them. The server does
/// broadcast one `CardLinkDeleted` per link before `ColumnDeleted` — but a tab
/// whose stream is lagged out of the backend's 128-slot broadcast channel (or
/// is silently missing events for any other reason) never sees them. Before
/// this, such a tab lost the column and its cards but kept their links, so a
/// partner card in a *surviving* column went on showing a link badge and a
/// bare `#N` chip — `card_label` degrades to the raw number once the card is
/// in no column — until a full reload. It is the column-sized version of card
/// #313, which fixed the same thing for a single card with
/// [`BoardLinkIndex::remove_touching`].
///
/// No `CardDeleted` is broadcast for the column's cards (the column event
/// stands for all of them), so the SSE `CardDeleted` arm that heals a lagged
/// tab after a card delete never runs for these cards — this is the only
/// place that can.
///
/// **Order matters.** The card ids come from [`BoardCardIndex`], which holds
/// each mounted column's own card list. That entry is withdrawn in the
/// column's `on_cleanup`, i.e. the moment the column leaves `columns`, so the
/// ids must be read *before* the retain below — afterwards there is nothing
/// left to ask. And the link index is written before the column is unmounted,
/// not after, so the write notifies only components that are still alive: a
/// reactive write landing on a component mid-unmount is exactly the shape of
/// the disposal panics iteration 54 chased.
///
/// Pruning by card id discards only links the server has also dropped — a
/// link cannot outlive either of its cards — *as long as this tab's view of
/// which cards are in the column is current*. A lagged tab that also missed a
/// `CardMoved` can mis-attribute a card: one moved out of the column keeps its
/// links on the server but loses them here, and one moved in keeps stale
/// links here. Either way that card is already wrong on this tab's screen,
/// and a reload heals both. The prune is idempotent, so in a
/// healthy stream, where the `CardLinkDeleted` events have already emptied
/// the index of these links, it finds nothing and writes nothing. Likewise a
/// second call for the same column (the local delete *and* its broadcast)
/// finds no index entry and no column, and is harmless.
///
/// Both indexes are looked up as contexts on `owner` — the `BoardView` owner
/// that provides them — rather than taken as parameters, so the SSE path
/// (inside an effect) and the chooser (after an `await`, with no reactive
/// owner of its own) resolve them identically. When either is missing (a unit
/// test of the column list alone) the column is still removed.
///
/// `try_update`, because the chooser calls this after an `await`: the board
/// may have been unmounted while the request was in flight, and writing a
/// disposed signal panics.
pub fn remove(owner: &Owner, columns: RwSignal<Vec<RwSignal<shared::Column>>>, column_id: &str) {
    // `owner.with` runs the closure with `owner` as the current reactive
    // owner, so `use_context` searches the board's contexts. Each lookup is an
    // `Option`: `None` when that context was never provided.
    let (card_index, link_index) = owner.with(|| {
        (
            use_context::<BoardCardIndex>(),
            use_context::<BoardLinkIndex>(),
        )
    });
    // A let-chain: the body runs only when both lookups found something.
    if let Some(card_index) = card_index
        && let Some(link_index) = link_index
    {
        // Snapshot the ids first (a plain `Vec<String>`), so no borrow of the
        // card index is still held while the link index is written below —
        // an `update` runs subscribed effects synchronously, and one of those
        // re-reading a still-borrowed signal aborts the tab.
        let card_ids = card_index.card_ids_in_column_untracked(column_id);
        link_index.remove_touching_any(&card_ids);
    }
    // `get_untracked` inside the retain: this is a write, and must not
    // subscribe whatever effect happens to be running to every column.
    let _ = columns.try_update(|cs| cs.retain(|s| s.get_untracked().id != column_id));
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

    // ── `remove` — card #371 ───────────────────────────────────────────────
    //
    // Real signals on the host target, with the two indexes provided as
    // contexts on an `Owner` the way `BoardView` provides them. Expected
    // values are written out literally rather than computed.

    use super::remove;
    use crate::links::BoardLinkIndex;
    use crate::search::BoardCardIndex;
    use leptos::prelude::*;

    fn column(id: &str) -> shared::Column {
        shared::Column {
            id: id.to_string(),
            board_id: "board".to_string(),
            name: id.to_string(),
            position: 0,
            last_edited_by: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn card(id: &str, column_id: &str) -> shared::Card {
        // Only `id` matters to `remove`; the rest is filler.
        shared::Card {
            id: id.to_string(),
            column_id: column_id.to_string(),
            body: String::new(),
            position: 0,
            number: 0,
            tags: Vec::new(),
            last_edited_by: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn link(id: &str, from: &str, to: &str) -> shared::CardLink {
        shared::CardLink {
            id: id.to_string(),
            predecessor_id: from.to_string(),
            successor_id: to.to_string(),
            predecessor_number: 0,
            successor_number: 0,
            reason: None,
            last_edited_by: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// A board of two columns: `doomed` holds cards `x` and `y`, `kept` holds
    /// `z` and `w`. Returns the column list and both indexes, all provided as
    /// contexts on `owner`.
    fn board(
        owner: &Owner,
        links: Vec<shared::CardLink>,
    ) -> (
        RwSignal<Vec<RwSignal<shared::Column>>>,
        BoardCardIndex,
        BoardLinkIndex,
    ) {
        owner.with(|| {
            let columns = RwSignal::new(vec![
                RwSignal::new(column("doomed")),
                RwSignal::new(column("kept")),
            ]);
            let cards = |ids: &[&str], col: &str| {
                RwSignal::new(
                    ids.iter()
                        .map(|id| RwSignal::new(card(id, col)))
                        .collect::<Vec<_>>(),
                )
            };
            let card_index = BoardCardIndex(RwSignal::new(vec![
                ("doomed".to_string(), cards(&["x", "y"], "doomed")),
                ("kept".to_string(), cards(&["z", "w"], "kept")),
            ]));
            let link_index = BoardLinkIndex {
                links: RwSignal::new(links),
                loaded: RwSignal::new(true),
            };
            provide_context(card_index);
            provide_context(link_index);
            (columns, card_index, link_index)
        })
    }

    fn column_ids(columns: RwSignal<Vec<RwSignal<shared::Column>>>) -> Vec<String> {
        columns
            .get_untracked()
            .iter()
            .map(|c| c.get_untracked().id)
            .collect()
    }

    fn link_ids(index: BoardLinkIndex) -> Vec<String> {
        index
            .links
            .get_untracked()
            .into_iter()
            .map(|l| l.id)
            .collect()
    }

    #[test]
    fn removing_a_column_prunes_the_links_of_every_card_in_it() {
        let owner = Owner::new();
        let (columns, _, links) = board(
            &owner,
            vec![
                // Into the doomed column, from a surviving card.
                link("z-x", "z", "x"),
                // Out of the doomed column, to a surviving card — the *other*
                // doomed card, so pruning has to cover every card, not one.
                link("y-w", "y", "w"),
                // Both ends doomed.
                link("x-y", "x", "y"),
                // Neither end doomed: must survive.
                link("z-w", "z", "w"),
            ],
        );
        remove(&owner, columns, "doomed");
        assert_eq!(column_ids(columns), ["kept"]);
        assert_eq!(link_ids(links), ["z-w"]);
    }

    #[test]
    fn removing_a_column_with_no_links_changes_no_links() {
        let owner = Owner::new();
        let (columns, _, links) = board(&owner, vec![link("z-w", "z", "w")]);
        remove(&owner, columns, "doomed");
        assert_eq!(column_ids(columns), ["kept"]);
        assert_eq!(link_ids(links), ["z-w"]);
    }

    /// A reader of `links` that counts how often it has had to recompute.
    ///
    /// A `Memo` is lazy: a write to a signal it read only marks it stale, and
    /// the next `get` re-runs its closure. So "the closure ran again" is
    /// exactly "`links` notified its subscribers since the last read" — the
    /// thing every `LinkBadges` and `LinkChip` on the board would react to —
    /// observed synchronously, with no effect scheduler needed on the host.
    /// The counter is an `Arc<AtomicUsize>` because a `Memo` closure must be
    /// `Send + Sync`.
    fn notification_counter(
        owner: &Owner,
        links: BoardLinkIndex,
    ) -> (Memo<usize>, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let memo = owner.with(|| {
            Memo::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                links.links.with(|l| l.len())
            })
        });
        // The first read subscribes the memo and runs it once.
        memo.get_untracked();
        (memo, runs)
    }

    #[test]
    fn a_prune_that_finds_nothing_notifies_no_link_reader() {
        // The healthy-stream case: `CardLinkDeleted` has already emptied the
        // index of the column's links, so the prune must not write at all — a
        // no-op write would still wake every link reader on the board, just as
        // the column is unmounting.
        use std::sync::atomic::Ordering;
        let owner = Owner::new();
        let (columns, _, links) = board(&owner, vec![link("z-w", "z", "w")]);
        let (memo, runs) = notification_counter(&owner, links);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        remove(&owner, columns, "doomed");
        memo.get_untracked();
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "no link was removed, so no reader may be woken"
        );
    }

    #[test]
    fn a_prune_that_removes_links_notifies_once() {
        // And the positive side, which shows the counter can see a write at
        // all: two doomed links go in a single notification, not one per card.
        use std::sync::atomic::Ordering;
        let owner = Owner::new();
        let (columns, _, links) = board(
            &owner,
            vec![
                link("z-x", "z", "x"),
                link("y-w", "y", "w"),
                link("z-w", "z", "w"),
            ],
        );
        let (memo, runs) = notification_counter(&owner, links);
        remove(&owner, columns, "doomed");
        assert_eq!(memo.get_untracked(), 1, "the memo saw the pruned list");
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn removing_a_column_twice_is_harmless() {
        // The local delete and its SSE broadcast both arrive. In the real board
        // the column's index entry is withdrawn on unmount in between; here it
        // is not, so this also shows a repeat prunes nothing it should keep.
        let owner = Owner::new();
        let (columns, _, links) = board(&owner, vec![link("z-x", "z", "x"), link("z-w", "z", "w")]);
        remove(&owner, columns, "doomed");
        remove(&owner, columns, "doomed");
        assert_eq!(column_ids(columns), ["kept"]);
        assert_eq!(link_ids(links), ["z-w"]);
    }

    #[test]
    fn a_column_the_card_index_does_not_know_prunes_no_links() {
        // A column whose cards are not indexed (not mounted yet — a load
        // replay) cannot say which links are its own, so it must prune none.
        let owner = Owner::new();
        let (columns, card_index, links) = board(&owner, vec![link("z-x", "z", "x")]);
        card_index
            .0
            .update(|entries| entries.retain(|(id, _)| id != "doomed"));
        remove(&owner, columns, "doomed");
        assert_eq!(column_ids(columns), ["kept"]);
        assert_eq!(link_ids(links), ["z-x"]);
    }

    #[test]
    fn without_the_indexes_the_column_is_still_removed() {
        let owner = Owner::new();
        let columns = owner.with(|| {
            RwSignal::new(vec![
                RwSignal::new(column("doomed")),
                RwSignal::new(column("kept")),
            ])
        });
        remove(&owner, columns, "doomed");
        assert_eq!(column_ids(columns), ["kept"]);
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
