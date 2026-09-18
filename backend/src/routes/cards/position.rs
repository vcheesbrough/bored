//! Sparse card ordering: where a card's `position` value comes from.
//!
//! Cards in a column are ordered by an `i32` position. Positions are *sparse*
//! (`POSITION_GAP` apart) so that inserting or moving a card normally writes
//! only that one card: it takes the midpoint of its new neighbours. When a gap
//! is used up the whole column is renumbered back onto the grid.
//!
//! The arithmetic (`midpoint_position`, `needs_rebalance`) is pure and unit
//! tested below; the `async` functions wrap it with the database reads/writes.

use surrealdb::{Surreal, engine::local::Db};

use crate::models::DbCard;

/// Gap between adjacent card positions in the sparse ordering scheme.
/// Large enough to allow ~10 bisections between any two cards before a rebalance
/// is needed, while fitting comfortably within i32.
pub(super) const POSITION_GAP: i32 = 1024;

/// Given the sorted card list for a column (with the moving card excluded),
/// compute the sparse position value for inserting at `idx`.
/// Uses sentinels: 0 at the top edge, last_pos + 2*GAP at the bottom edge.
fn midpoint_position(col_cards: &[DbCard], idx: usize) -> i32 {
    let left = if idx == 0 {
        0
    } else {
        col_cards[idx - 1].position
    };
    let right = if idx >= col_cards.len() {
        col_cards.last().map(|c| c.position).unwrap_or(0) + 2 * POSITION_GAP
    } else {
        col_cards[idx].position
    };
    (left + right) / 2
}

/// Returns true when the candidate position is not strictly between its
/// left and right neighbours — meaning the gap is exhausted and we must
/// rebalance before inserting.
fn needs_rebalance(col_cards: &[DbCard], idx: usize, new_pos: i32) -> bool {
    let left = if idx == 0 {
        0
    } else {
        col_cards[idx - 1].position
    };
    // Bottom edge: any positive value above `left` is always valid.
    let right = if idx >= col_cards.len() {
        i32::MAX
    } else {
        col_cards[idx].position
    };
    new_pos <= left || new_pos >= right
}

/// Reassign every card in `col_id` to evenly-spaced positions (GAP, 2*GAP, …).
/// Called only when the gap between two neighbouring cards drops to zero,
/// which happens after ~10 consecutive insertions at the same slot.
async fn rebalance_column(db: &Surreal<Db>, col_id: &str) -> Result<(), surrealdb::Error> {
    let cards: Vec<DbCard> = db
        .query(
            "SELECT * FROM cards \
             WHERE column = type::thing('columns', $col_id) \
             ORDER BY position ASC",
        )
        .bind(("col_id", col_id.to_string()))
        .await?
        .take(0)?;

    for (i, card) in cards.iter().enumerate() {
        // Start at GAP (not 0) so there is always room above the first card
        // for a top insert without immediately triggering another rebalance.
        db.query("UPDATE type::thing('cards', $id) SET position = $pos")
            .bind(("id", card.id.id.to_raw()))
            .bind(("pos", (i as i32 + 1) * POSITION_GAP))
            .await?;
    }
    Ok(())
}

/// Compute a sparse position for inserting a brand-new card at the TOP of
/// `col_id` (index 0 in the sorted sibling list).  Unlike
/// `compute_sparse_position` there is no card to exclude, so we query all
/// existing cards in the column.
pub(super) async fn compute_top_position(
    db: &Surreal<Db>,
    col_id: &str,
) -> Result<i32, surrealdb::Error> {
    let col_cards: Vec<DbCard> = db
        .query(
            "SELECT * FROM cards \
             WHERE column = type::thing('columns', $col_id) \
             ORDER BY position ASC",
        )
        .bind(("col_id", col_id.to_string()))
        .await?
        .take(0)?;

    let new_pos = midpoint_position(&col_cards, 0);

    // If the gap between the sentinel (0) and the current first card has been
    // exhausted, rebalance the whole column before computing the new position.
    if !col_cards.is_empty() && needs_rebalance(&col_cards, 0, new_pos) {
        rebalance_column(db, col_id).await?;

        let col_cards: Vec<DbCard> = db
            .query(
                "SELECT * FROM cards \
                 WHERE column = type::thing('columns', $col_id) \
                 ORDER BY position ASC",
            )
            .bind(("col_id", col_id.to_string()))
            .await?
            .take(0)?;

        return Ok(midpoint_position(&col_cards, 0));
    }

    Ok(new_pos)
}

/// Compute a single sparse position value for moving `card_id` to index
/// `target_index` within `col_id`.  Only the moved card is ever written;
/// no other cards are modified in the happy path.
pub(super) async fn compute_sparse_position(
    db: &Surreal<Db>,
    card_id: &str,
    col_id: &str,
    target_index: i32,
) -> Result<i32, surrealdb::Error> {
    // Fetch sibling cards (the moving card excluded) so we see the column
    // as it will look after the move.
    let col_cards: Vec<DbCard> = db
        .query(
            "SELECT * FROM cards \
             WHERE column = type::thing('columns', $col_id) \
               AND id != type::thing('cards', $card_id) \
             ORDER BY position ASC",
        )
        .bind(("col_id", col_id.to_string()))
        .bind(("card_id", card_id.to_string()))
        .await?
        .take(0)?;

    let idx = (target_index as usize).min(col_cards.len());
    let new_pos = midpoint_position(&col_cards, idx);

    if needs_rebalance(&col_cards, idx, new_pos) {
        // Gap exhausted — renumber the column then recompute.  After a
        // rebalance every gap is exactly POSITION_GAP, so the second
        // midpoint_position call is guaranteed to succeed.
        rebalance_column(db, col_id).await?;

        let col_cards: Vec<DbCard> = db
            .query(
                "SELECT * FROM cards \
                 WHERE column = type::thing('columns', $col_id) \
                   AND id != type::thing('cards', $card_id) \
                 ORDER BY position ASC",
            )
            .bind(("col_id", col_id.to_string()))
            .bind(("card_id", card_id.to_string()))
            .await?
            .take(0)?;

        let idx = (target_index as usize).min(col_cards.len());
        return Ok(midpoint_position(&col_cards, idx));
    }

    Ok(new_pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::sql::{Datetime, Thing};

    /// Build a column's worth of cards with the given positions. Only
    /// `position` matters to the functions under test; everything else is
    /// filler so the struct can be constructed.
    fn cards_at(positions: &[i32]) -> Vec<DbCard> {
        positions
            .iter()
            .enumerate()
            .map(|(i, &position)| DbCard {
                // `Thing` is SurrealDB's record id: a (table, id) pair.
                id: Thing::from(("cards", format!("c{i}").as_str())),
                column: Thing::from(("columns", "col")),
                body: String::new(),
                position,
                number: None,
                tags: Vec::new(),
                last_edited_by: None,
                created_at: Datetime::default(),
                updated_at: Datetime::default(),
            })
            .collect()
    }

    #[test]
    fn midpoint_in_an_empty_column_is_one_gap() {
        // Sentinels: 0 above, `0 + 2 * GAP` below — the midpoint is GAP.
        assert_eq!(midpoint_position(&[], 0), POSITION_GAP);
    }

    #[test]
    fn midpoint_at_the_top_bisects_zero_and_the_first_card() {
        let cards = cards_at(&[1024, 2048]);
        assert_eq!(midpoint_position(&cards, 0), 512);
    }

    #[test]
    fn midpoint_between_two_cards_bisects_them() {
        let cards = cards_at(&[1024, 2048]);
        assert_eq!(midpoint_position(&cards, 1), 1536);
    }

    #[test]
    fn midpoint_at_the_bottom_lands_one_gap_below_the_last_card() {
        let cards = cards_at(&[1024, 2048]);
        assert_eq!(midpoint_position(&cards, 2), 2048 + POSITION_GAP);
        // `idx == len` is the largest index callers may pass: they clamp with
        // `.min(col_cards.len())` first, so anything larger is out of contract.
    }

    #[test]
    fn a_position_strictly_between_its_neighbours_needs_no_rebalance() {
        let cards = cards_at(&[1024, 2048]);
        assert!(!needs_rebalance(&cards, 1, 1536));
        assert!(!needs_rebalance(&cards, 0, 512));
    }

    #[test]
    fn an_exhausted_gap_needs_a_rebalance() {
        // Adjacent integers leave no room: the midpoint equals the left card.
        let cards = cards_at(&[10, 11]);
        let candidate = midpoint_position(&cards, 1);
        assert_eq!(candidate, 10);
        assert!(needs_rebalance(&cards, 1, candidate));
    }

    #[test]
    fn a_first_card_at_position_one_exhausts_the_top_gap() {
        let cards = cards_at(&[1, 1024]);
        let candidate = midpoint_position(&cards, 0);
        assert_eq!(candidate, 0);
        assert!(needs_rebalance(&cards, 0, candidate));
    }

    #[test]
    fn the_bottom_edge_never_needs_a_rebalance() {
        let cards = cards_at(&[1024, 2048]);
        let candidate = midpoint_position(&cards, 2);
        assert!(!needs_rebalance(&cards, 2, candidate));
    }

    #[test]
    fn bisecting_the_same_slot_survives_about_ten_inserts() {
        // The doc comment on `POSITION_GAP` promises ~10 bisections before a
        // rebalance; pin that so a change to the gap is a conscious one.
        let mut cards = cards_at(&[1024, 2048]);
        let mut inserts = 0;
        loop {
            let candidate = midpoint_position(&cards, 1);
            if needs_rebalance(&cards, 1, candidate) {
                break;
            }
            // Keep inserting directly below the first card.
            cards.insert(1, cards_at(&[candidate]).remove(0));
            inserts += 1;
        }
        assert_eq!(inserts, 10);
    }
}
