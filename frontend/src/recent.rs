//! Per-browser record of what the user picked from a combo box, so the
//! suggestion lists can lead with most-recently-used entries.
//!
//! Two combo boxes offer suggestions: the navbar's `#` search popup (tags and
//! card numbers) and the link picker on a card (other cards). Both used to
//! order their list by an intrinsic property of the entry — alphabetical for
//! tags, numeric for cards — which never reflects what the user actually
//! reaches for. This module remembers the picks instead.
//!
//! The picks live in `localStorage`, keyed per board, exactly like the
//! collapsed-column state in [`crate::components::column`]. They are therefore
//! per-browser: a different device, or a cleared store, simply falls back to
//! the recency ordering the callers apply underneath the pick ranks.
//!
//! The ordering helpers here are deliberately pure functions over slices
//! rather than methods on [`RecentPicks`], so they can be unit-tested on the
//! host target — `cargo test -p frontend` runs without a DOM, so anything that
//! touches `window()` is untestable there.

use leptos::prelude::*;

/// Storage key prefixes. Each is suffixed with the board's ULID so two boards
/// open in the same browser keep separate histories.
const RECENT_CARDS_STORAGE_PREFIX: &str = "bored:recent-cards:";
const RECENT_TAGS_STORAGE_PREFIX: &str = "bored:recent-tags:";

/// How many picks are remembered per board, per kind.
///
/// The suggestion lists themselves show at most a handful of rows, so the cap
/// only has to be comfortably larger than that: it is the depth at which an
/// old pick stops influencing the order at all.
const MAX_REMEMBERED: usize = 20;

/// The rank given to an entry that was never picked.
///
/// Callers sort on `(rank, …fallbacks)`, so an unpicked entry sorts after
/// every picked one and is then ordered entirely by the fallbacks.
pub const UNRANKED: usize = usize::MAX;

/// Board-scoped history of the user's combo-box picks.
///
/// The signals — not `localStorage` — are the source of truth while the board
/// is open: a `Signal::derive` cannot observe a storage write, so a list built
/// by reading storage directly would not re-order until something else
/// happened to invalidate it. Storage is written through on every pick and
/// read back once per board, in [`Self::load`].
#[derive(Clone, Copy)]
pub struct RecentPicks {
    /// The board whose history these signals hold. Empty until the board's
    /// initial fetch resolves its ULID.
    pub board_id: RwSignal<String>,
    /// Card IDs, most recently picked first.
    pub cards: RwSignal<Vec<String>>,
    /// Tags, most recently picked first, in the case they were stored with.
    pub tags: RwSignal<Vec<String>>,
}

impl RecentPicks {
    /// A history with nothing remembered yet, for a board not yet resolved.
    pub fn new(board_id: RwSignal<String>) -> Self {
        Self {
            board_id,
            cards: RwSignal::new(Vec::new()),
            tags: RwSignal::new(Vec::new()),
        }
    }

    /// Replace the in-memory history with the one stored for `board_id`.
    ///
    /// Called once the board's ULID is known. A board with no stored history
    /// — or a browser that refuses storage entirely — yields empty lists,
    /// which is exactly the "no picks yet" state.
    ///
    /// Reads the ULID reactively: navigating between boards blanks it and then
    /// sets the new one, and the blank must clear the lists rather than leave
    /// the previous board's picks ordering this one's suggestions.
    pub fn load(&self) {
        let board_id = self.board_id.get();
        if board_id.is_empty() {
            self.cards.set(Vec::new());
            self.tags.set(Vec::new());
            return;
        }
        self.cards
            .set(load_list(&cards_key(&board_id)).unwrap_or_default());
        self.tags
            .set(load_list(&tags_key(&board_id)).unwrap_or_default());
    }

    /// Record that the user picked the card with this ID.
    pub fn record_card(&self, card_id: &str) {
        let board_id = self.board_id.get_untracked();
        if board_id.is_empty() {
            // No board resolved yet: recording would write to a shared,
            // board-less key that every board in that state would read.
            return;
        }
        self.cards.update(|list| {
            push_front(list, card_id, MAX_REMEMBERED, str::eq);
            persist_list(&cards_key(&board_id), list);
        });
    }

    /// Record that the user picked this tag.
    ///
    /// Tags are matched case-insensitively — `#Bug` and `#bug` are the same
    /// tag to every other part of the board — so re-picking one under a
    /// different case moves the stored entry rather than adding a second.
    pub fn record_tag(&self, tag: &str) {
        let board_id = self.board_id.get_untracked();
        if board_id.is_empty() {
            // No board resolved yet: recording would write to a shared,
            // board-less key that every board in that state would read.
            return;
        }
        self.tags.update(|list| {
            push_front(list, tag, MAX_REMEMBERED, shared::tags::eq_ignore_case);
            persist_list(&tags_key(&board_id), list);
        });
    }
}

/// Move `key` to the front of `list`, dropping any earlier occurrence (as
/// judged by `same`) and trimming the tail to `cap`.
///
/// Generic over the equality test so tags can be compared case-insensitively
/// while card IDs are compared exactly.
pub fn push_front<F>(list: &mut Vec<String>, key: &str, cap: usize, same: F)
where
    F: Fn(&str, &str) -> bool,
{
    list.retain(|existing| !same(existing, key));
    list.insert(0, key.to_owned());
    list.truncate(cap);
}

/// Position of `key` in `recent` (0 = picked most recently), or [`UNRANKED`].
pub fn rank_of(recent: &[String], key: &str) -> usize {
    recent
        .iter()
        .position(|existing| existing == key)
        .unwrap_or(UNRANKED)
}

/// [`rank_of`] for tags, which compare case-insensitively.
pub fn tag_rank_of(recent: &[String], tag: &str) -> usize {
    recent
        .iter()
        .position(|existing| shared::tags::eq_ignore_case(existing, tag))
        .unwrap_or(UNRANKED)
}

/// A sortable key for an API timestamp, ordering chronologically as a string.
///
/// Timestamps reach the frontend as whatever `surrealdb::sql::Datetime`'s
/// `Display` produced — an RFC 3339 instant, possibly wrapped as `d'…'`. Two
/// such strings do *not* reliably compare chronologically on their own: the
/// fractional part is printed without trailing zeros, so `…45.6Z` sorts
/// *after* `…45.679Z` because `'Z'` outranks `'7'` in ASCII. Unwrapping the
/// quotes and padding the fraction to nine digits removes both hazards, and
/// leaves anything unrecognised to compare as itself rather than vanishing.
///
/// Instants carrying a numeric UTC offset are split correctly but still not
/// *converted*: `12:00:00+01:00` and `12:00:00Z` compare by their wall-clock
/// text, not by the moment they name. Ordering only stays chronological among
/// timestamps sharing one offset — which every timestamp from this API does,
/// since SurrealDB renders UTC.
pub fn recency_key(timestamp: &str) -> String {
    let trimmed = timestamp
        .trim()
        .strip_prefix("d'")
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(timestamp.trim());

    // Split off the zone suffix so the fraction can be padded in isolation.
    // The search starts after the `T`, because a negative offset's `-` is the
    // same character the date is full of: scanning the whole string would
    // "find" the zone at `2026-09-12`.
    let time_at = trimmed.find(['T', 't']).map_or(0, |at| at + 1);
    let (instant, zone) = match trimmed[time_at..].find(['Z', 'z', '+', '-']) {
        Some(at) => trimmed.split_at(time_at + at),
        None => (trimmed, ""),
    };
    let Some((seconds, fraction)) = instant.split_once('.') else {
        // No fractional part: pad with zeros so it sorts against those that
        // have one, rather than before every one of them.
        return format!("{instant}.000000000{zone}");
    };
    format!("{seconds}.{fraction:0<9}{zone}")
}

fn cards_key(board_id: &str) -> String {
    format!("{RECENT_CARDS_STORAGE_PREFIX}{board_id}")
}

fn tags_key(board_id: &str) -> String {
    format!("{RECENT_TAGS_STORAGE_PREFIX}{board_id}")
}

/// Read a stored list, or `None` when storage is unavailable, the key is
/// unset, or the value is not the JSON array we wrote.
fn load_list(key: &str) -> Option<Vec<String>> {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|storage| storage.get_item(key).ok().flatten())
        .and_then(|value| serde_json::from_str(&value).ok())
}

/// Write a list back, removing the key entirely when the list is empty so an
/// exhausted history leaves no stale entry behind.
fn persist_list(key: &str, list: &[String]) {
    let Ok(Some(storage)) = window().local_storage() else {
        return;
    };
    if list.is_empty() {
        let _ = storage.remove_item(key);
    } else if let Ok(value) = serde_json::to_string(list) {
        let _ = storage.set_item(key, &value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn push_front_puts_a_new_pick_first() {
        let mut recent = list(&["a"]);
        push_front(&mut recent, "b", 10, str::eq);
        assert_eq!(recent, list(&["b", "a"]));
    }

    #[test]
    fn push_front_moves_an_existing_pick_rather_than_duplicating_it() {
        let mut recent = list(&["a", "b", "c"]);
        push_front(&mut recent, "c", 10, str::eq);
        assert_eq!(recent, list(&["c", "a", "b"]));
    }

    #[test]
    fn push_front_drops_the_oldest_pick_past_the_cap() {
        let mut recent = list(&["a", "b", "c"]);
        push_front(&mut recent, "d", 3, str::eq);
        assert_eq!(recent, list(&["d", "a", "b"]));
    }

    #[test]
    fn push_front_folds_tags_that_differ_only_in_case() {
        let mut recent = list(&["Bug", "chore"]);
        push_front(&mut recent, "bug", 10, shared::tags::eq_ignore_case);
        // One entry, in the case it was just picked with.
        assert_eq!(recent, list(&["bug", "chore"]));
    }

    #[test]
    fn rank_of_reports_pick_order_and_marks_unpicked_entries() {
        let recent = list(&["a", "b"]);
        assert_eq!(rank_of(&recent, "a"), 0);
        assert_eq!(rank_of(&recent, "b"), 1);
        assert_eq!(rank_of(&recent, "c"), UNRANKED);
    }

    #[test]
    fn tag_rank_of_ignores_case() {
        let recent = list(&["Bug"]);
        assert_eq!(tag_rank_of(&recent, "bug"), 0);
        assert_eq!(tag_rank_of(&recent, "BUG"), 0);
        assert_eq!(tag_rank_of(&recent, "chore"), UNRANKED);
    }

    #[test]
    fn recency_key_unwraps_the_surreal_datetime_literal() {
        assert_eq!(
            recency_key("d'2026-09-12T12:17:45.679420467Z'"),
            "2026-09-12T12:17:45.679420467Z"
        );
    }

    #[test]
    fn recency_key_orders_a_short_fraction_before_a_longer_later_one() {
        // The hazard this function exists for: compared raw, `.6Z` > `.679Z`.
        let earlier = recency_key("2026-09-12T12:17:45.6Z");
        let later = recency_key("2026-09-12T12:17:45.679Z");
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn recency_key_orders_a_missing_fraction_before_any_fraction() {
        let whole = recency_key("2026-09-12T12:17:45Z");
        let fractional = recency_key("2026-09-12T12:17:45.000000001Z");
        assert!(
            whole < fractional,
            "{whole} should sort before {fractional}"
        );
    }

    #[test]
    fn recency_key_still_orders_plain_instants_chronologically() {
        let keys: Vec<String> = [
            "2026-09-12T12:17:45.5Z",
            "2026-09-11T23:59:59.999999999Z",
            "2026-09-12T12:17:46Z",
        ]
        .iter()
        .map(|t| recency_key(t))
        .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![keys[1].clone(), keys[0].clone(), keys[2].clone()]
        );
    }

    #[test]
    fn recency_key_splits_a_numeric_offset_off_the_fraction() {
        // The date is full of `-`, so the zone search has to start after the
        // `T` or the offset's `-` is indistinguishable from the date's.
        assert_eq!(
            recency_key("2026-09-12T12:17:45.6-05:00"),
            "2026-09-12T12:17:45.600000000-05:00"
        );
        assert_eq!(
            recency_key("2026-09-12T12:17:45+01:00"),
            "2026-09-12T12:17:45.000000000+01:00"
        );
    }

    #[test]
    fn recency_key_orders_offset_instants_among_themselves() {
        // Offsets are split, not converted: ordering holds within one offset.
        let earlier = recency_key("2026-09-12T12:17:45.6-05:00");
        let later = recency_key("2026-09-12T12:17:45.679-05:00");
        assert!(earlier < later, "{earlier} should sort before {later}");
    }

    #[test]
    fn recency_key_passes_through_an_unrecognised_timestamp() {
        // Not something the API produces, but it must stay comparable rather
        // than collapse to a constant that scrambles the order.
        assert_ne!(recency_key("not-a-timestamp"), recency_key("also-not-one"));
    }
}
