//! Holding back events that race the snapshot they apply to (card #452).

/// Buffers events that arrive while a snapshot is in flight, so they can be
/// replayed on top of it rather than lost to it (card #452).
///
/// The board view opens its event stream as soon as it knows the board, in
/// parallel with the requests that load it — its columns, its links, each
/// column's cards. An event can land before the snapshot it concerns: applied
/// then, it is dropped (an update for something not listed yet) or wiped by
/// the snapshot, read before the change, replacing the list. Each such
/// snapshot has a gate; its events go through `offer`, and `land` hands back
/// what was held so the caller replays it on the fresh list.
///
/// Generic over the event so its rules can be tested without Leptos or
/// real events. A fresh gate is closed: nothing has loaded yet, so an event
/// seen before the first fetch starts is held for it too.
#[derive(Debug)]
pub(crate) struct SnapshotGate<E> {
    /// The latest fetch started; only its response may open the gate.
    fetch: u64,
    /// `Some` while a snapshot is awaited — the events held for it, in order.
    pending: Option<Vec<E>>,
}

impl<E> SnapshotGate<E> {
    pub(crate) fn new() -> Self {
        Self {
            fetch: 0,
            pending: Some(Vec::new()),
        }
    }

    /// A fetch is starting: close the gate (keeping anything already held,
    /// which the new snapshot may predate) and return its ticket.
    pub(crate) fn begin(&mut self) -> u64 {
        self.fetch += 1;
        self.pending.get_or_insert_with(Vec::new);
        self.fetch
    }

    /// An event arrived. `Some` gives it back to apply now; `None` means it
    /// was held for the snapshot.
    pub(crate) fn offer(&mut self, event: E) -> Option<E> {
        match &mut self.pending {
            Some(held) => {
                held.push(event);
                None
            }
            None => Some(event),
        }
    }

    /// Fetch `fetch` has answered. For the latest fetch, open the gate and
    /// return the held events to replay; for a superseded one, `None` — its
    /// snapshot is older than the one still coming, and must not be applied.
    pub(crate) fn land(&mut self, fetch: u64) -> Option<Vec<E>> {
        if fetch != self.fetch {
            return None;
        }
        Some(self.pending.take().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::SnapshotGate;

    #[test]
    fn events_during_the_fetch_are_held_and_handed_back_in_order() {
        let mut gate = SnapshotGate::new();
        let fetch = gate.begin();
        assert_eq!(gate.offer("created"), None);
        assert_eq!(gate.offer("updated"), None);
        assert_eq!(gate.land(fetch), Some(vec!["created", "updated"]));
    }

    #[test]
    fn once_landed_events_pass_straight_through() {
        let mut gate = SnapshotGate::new();
        let fetch = gate.begin();
        assert_eq!(gate.land(fetch), Some(Vec::<&str>::new()));
        assert_eq!(gate.offer("live"), Some("live"));
    }

    #[test]
    fn an_event_before_the_first_fetch_starts_is_held_for_it() {
        // A fresh gate is closed: nothing has loaded, so an event offered
        // before the first fetch starts waits for it rather than going onto
        // an empty, unloaded list. (`ColumnView` itself ignores the stale
        // value its SSE effect sees at mount; this is the gate's own rule.)
        let mut gate = SnapshotGate::new();
        assert_eq!(gate.offer("early"), None);
        let fetch = gate.begin();
        assert_eq!(gate.land(fetch), Some(vec!["early"]));
    }

    #[test]
    fn a_superseded_fetch_neither_opens_the_gate_nor_takes_the_events() {
        let mut gate = SnapshotGate::new();
        let first = gate.begin();
        assert_eq!(gate.offer("a"), None);
        let second = gate.begin();
        assert_eq!(gate.offer("b"), None);
        // The first response is older than the one still coming.
        assert_eq!(gate.land(first), None);
        assert_eq!(gate.offer("c"), None, "still closed for the second fetch");
        assert_eq!(gate.land(second), Some(vec!["a", "b", "c"]));
    }

    #[test]
    fn a_refetch_closes_an_open_gate_again() {
        let mut gate = SnapshotGate::new();
        let first = gate.begin();
        assert_eq!(gate.land(first), Some(Vec::<&str>::new()));
        let second = gate.begin();
        assert_eq!(gate.offer("during refetch"), None);
        assert_eq!(gate.land(second), Some(vec!["during refetch"]));
    }
}
