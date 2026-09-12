//! Board-level index of card links.
//!
//! Links are fetched once per board by `pages::board_view` and kept in one
//! signal that every card reads through this context. A link belongs to two
//! cards, so holding it in either card's own state would mean storing it
//! twice and keeping the copies agreed; one board-level list sidesteps that,
//! and gives SSE a single place to apply a change so both ends update at once.

use leptos::prelude::*;

/// Every link on the board, in creation order. Provided by `BoardView`.
#[derive(Clone, Copy)]
pub struct BoardLinkIndex(pub RwSignal<Vec<shared::CardLink>>);

impl BoardLinkIndex {
    /// Links in which `card_id` is the successor — the cards that come before it.
    pub fn predecessors_of(&self, card_id: &str) -> Vec<shared::CardLink> {
        self.0
            .get()
            .into_iter()
            .filter(|link| link.successor_id == card_id)
            .collect()
    }

    /// Links in which `card_id` is the predecessor — the cards that come after it.
    pub fn successors_of(&self, card_id: &str) -> Vec<shared::CardLink> {
        self.0
            .get()
            .into_iter()
            .filter(|link| link.predecessor_id == card_id)
            .collect()
    }

    /// True when the two cards are already linked, in either direction.
    pub fn are_linked(&self, a: &str, b: &str) -> bool {
        self.0.get().iter().any(|link| {
            (link.predecessor_id == a && link.successor_id == b)
                || (link.predecessor_id == b && link.successor_id == a)
        })
    }

    /// The same verdict the server would reach for a new `predecessor →
    /// successor` edge, computed over the links the browser currently knows.
    /// Used to keep choices the server would refuse out of the picker.
    pub fn would_create_cycle(&self, predecessor_id: &str, successor_id: &str) -> bool {
        let links = self.0.get();
        let edges = links
            .iter()
            .map(|link| (link.predecessor_id.as_str(), link.successor_id.as_str()));
        shared::links::would_create_cycle(edges, predecessor_id, successor_id)
    }

    /// Add a link unless one with the same id is already present. A created
    /// link arrives twice — as the `201` body and as the SSE broadcast — and
    /// whichever lands second must not duplicate it.
    pub fn insert_absent(&self, link: shared::CardLink) {
        self.0.update(|links| {
            if !links.iter().any(|existing| existing.id == link.id) {
                links.push(link);
            }
        });
    }

    /// Swap in an updated link by id, or add it if it was not known.
    pub fn replace(&self, link: shared::CardLink) {
        self.0.update(
            |links| match links.iter().position(|existing| existing.id == link.id) {
                Some(i) => links[i] = link,
                None => links.push(link),
            },
        );
    }

    pub fn remove(&self, link_id: &str) {
        self.0
            .update(|links| links.retain(|link| link.id != link_id));
    }
}
