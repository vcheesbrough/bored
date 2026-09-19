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
pub struct BoardLinkIndex {
    /// The links themselves, in creation order.
    pub links: RwSignal<Vec<shared::CardLink>>,
    /// False until `BoardView`'s initial `fetch_board_links` has resolved.
    ///
    /// Columns are painted as soon as `fetch_columns` returns, one round trip
    /// before the links land, so an empty `links` is ambiguous on its own: it
    /// means either "this board has no links" or "they have not arrived yet".
    /// Anything that would read a *missing* edge as a satisfied one — the
    /// column's sort-by-links button — has to wait for this flag rather than
    /// for a non-empty list. Travels in the same struct as `links` so the two
    /// can never be provided apart.
    pub loaded: RwSignal<bool>,
}

impl BoardLinkIndex {
    /// Links in which `card_id` is the successor — the cards that come before it.
    pub fn predecessors_of(&self, card_id: &str) -> Vec<shared::CardLink> {
        self.links
            .get()
            .into_iter()
            .filter(|link| link.successor_id == card_id)
            .collect()
    }

    /// Links in which `card_id` is the predecessor — the cards that come after it.
    pub fn successors_of(&self, card_id: &str) -> Vec<shared::CardLink> {
        self.links
            .get()
            .into_iter()
            .filter(|link| link.predecessor_id == card_id)
            .collect()
    }

    /// True when the two cards are already linked, in either direction.
    pub fn are_linked(&self, a: &str, b: &str) -> bool {
        self.links.get().iter().any(|link| {
            (link.predecessor_id == a && link.successor_id == b)
                || (link.predecessor_id == b && link.successor_id == a)
        })
    }

    /// The same verdict the server would reach for a new `predecessor →
    /// successor` edge, computed over the links the browser currently knows.
    /// Used to keep choices the server would refuse out of the picker.
    pub fn would_create_cycle(&self, predecessor_id: &str, successor_id: &str) -> bool {
        let links = self.links.get();
        let edges = links
            .iter()
            .map(|link| (link.predecessor_id.as_str(), link.successor_id.as_str()));
        shared::links::would_create_cycle(edges, predecessor_id, successor_id)
    }

    /// Add a link unless one with the same id is already present. A created
    /// link arrives twice — as the `201` body and as the SSE broadcast — and
    /// whichever lands second must not duplicate it.
    pub fn insert_absent(&self, link: shared::CardLink) {
        self.links.update(|links| {
            if !links.iter().any(|existing| existing.id == link.id) {
                links.push(link);
            }
        });
    }

    /// Swap in an updated link by id, or add it if it was not known.
    pub fn replace(&self, link: shared::CardLink) {
        self.links.update(
            |links| match links.iter().position(|existing| existing.id == link.id) {
                Some(i) => links[i] = link,
                None => links.push(link),
            },
        );
    }

    pub fn remove(&self, link_id: &str) {
        self.links
            .update(|links| links.retain(|link| link.id != link_id));
    }

    /// Drop every link that has `card_id` at either end.
    ///
    /// A link cannot outlive either of the cards it joins — the backend's
    /// `cascade_delete_card_links` removes them before it deletes the card — so
    /// pruning by card id can never discard a link the server still holds. That
    /// makes this safe to call from *any* path that learns a card is gone, and
    /// safe to call twice: the second call finds nothing left to retain out.
    ///
    /// It exists because the index was previously pruned **only** by the SSE
    /// `CardLinkDeleted` events, while the card itself is removed locally as
    /// soon as the DELETE returns. A tab whose stream is down, reconnecting, or
    /// lagged out of the backend's 128-slot broadcast channel therefore lost the
    /// card but kept its links, leaving the partner card showing a link badge
    /// and a bare `#N` chip — `card_label` degrades to the raw number once the
    /// deleted card is no longer in `BoardCardIndex` — until a full reload.
    ///
    /// The rule `on_sort_by_links` already documents applies: a local action
    /// applies its own result rather than waiting for SSE to relay it.
    ///
    /// The per-link verdict is [`shared::CardLink::touches`], which is where the
    /// rule is unit-tested — this method needs a reactive runtime to hold its
    /// signal, so what is worth asserting lives on the plain data instead.
    pub fn remove_touching(&self, card_id: &str) {
        self.links
            .update(|links| links.retain(|link| !link.touches(card_id)));
    }
}
