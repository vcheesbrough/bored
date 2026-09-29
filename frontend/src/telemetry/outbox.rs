//! The bounded queue that holds encoded spans or log records until they are
//! exported (card #416).
//!
//! `client-export.md`: "A buffer that grows until the export succeeds is a
//! memory leak on a device you do not control and cannot debug." So the outbox
//! is capped twice — by item count and by bytes — and when either cap is hit
//! the **oldest** item goes. Every item it throws away is counted, so the
//! console's transition lines can say how much was lost.
//!
//! Items are stored already encoded (one JSON text per span or record). That
//! makes the byte cap exact — it measures what would actually be sent — and
//! makes building a request a string join.

use std::collections::VecDeque;

/// The caps for one outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most items held at once.
    pub max_items: usize,
    /// Most bytes held at once (sum of the encoded items).
    pub max_bytes: usize,
    /// Largest single item accepted. An item bigger than this is dropped on
    /// arrival rather than allowed to evict everything else; it must also be
    /// no bigger than the smallest batch the exporter ever takes, or it could
    /// never be sent.
    pub max_item_bytes: usize,
}

/// A bounded, drop-oldest FIFO of encoded items.
#[derive(Debug)]
pub struct Outbox {
    /// `VecDeque` is a ring buffer: pushing at the back and popping at the
    /// front are both O(1), which is exactly a FIFO's access pattern.
    items: VecDeque<String>,
    /// Running total of `items`' lengths, so the byte cap never needs a scan.
    bytes: usize,
    limits: Limits,
    /// Items thrown away by the caps since the outbox was created.
    dropped: u64,
}

impl Outbox {
    pub fn new(limits: Limits) -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            limits,
            dropped: 0,
        }
    }

    /// Add one encoded item, evicting the oldest until both caps hold.
    pub fn push(&mut self, item: String) {
        if item.len() > self.limits.max_item_bytes {
            // Too big to ever fit a batch: drop it rather than let it push out
            // everything older.
            self.dropped += 1;
            return;
        }
        self.bytes += item.len();
        self.items.push_back(item);
        // `while` rather than `if`: one large arrival can need several small
        // items evicted to get back under the byte cap.
        while self.items.len() > self.limits.max_items || self.bytes > self.limits.max_bytes {
            match self.items.pop_front() {
                Some(oldest) => {
                    self.bytes -= oldest.len();
                    self.dropped += 1;
                }
                None => break,
            }
        }
    }

    /// Remove and return the oldest items whose encoded bytes, plus
    /// `overhead(count)` for the envelope around them, fit in `max_bytes`.
    ///
    /// `overhead` is a closure (`impl Fn(usize) -> usize`) because the envelope
    /// cost depends on how many items it wraps (one comma between each), so
    /// only the caller's encoder knows it.
    pub fn take_batch(
        &mut self,
        max_bytes: usize,
        overhead: impl Fn(usize) -> usize,
    ) -> Vec<String> {
        let mut batch = Vec::new();
        let mut batch_bytes = 0;
        // `front()` peeks without removing, so an item that would overflow the
        // batch stays queued for the next one.
        while let Some(next) = self.items.front() {
            let would_be = batch_bytes + next.len();
            if would_be + overhead(batch.len() + 1) > max_bytes {
                break;
            }
            batch_bytes = would_be;
            // `expect` cannot fire: `front()` just returned `Some`.
            let item = self.items.pop_front().expect("front() was Some");
            self.bytes -= item.len();
            batch.push(item);
        }
        batch
    }

    /// Throw everything away, counting it — used when telemetry turns out to
    /// be off, or for the session after giving up.
    pub fn discard_all(&mut self) {
        self.dropped += self.items.len() as u64;
        self.items.clear();
        self.bytes = 0;
    }

    /// Record items lost outside the outbox (a batch dropped after a refused
    /// export), so one counter covers every loss.
    pub fn count_dropped(&mut self, count: usize) {
        self.dropped += count as u64;
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_items: usize, max_bytes: usize) -> Limits {
        Limits {
            max_items,
            max_bytes,
            max_item_bytes: max_bytes,
        }
    }

    /// An item of exactly `len` bytes, labelled so order can be checked.
    fn item(label: char, len: usize) -> String {
        std::iter::repeat_n(label, len).collect()
    }

    #[test]
    fn keeps_fifo_order() {
        let mut outbox = Outbox::new(limits(10, 1000));
        outbox.push(item('a', 3));
        outbox.push(item('b', 3));
        let batch = outbox.take_batch(1000, |_| 0);
        assert_eq!(batch, vec![item('a', 3), item('b', 3)]);
        assert!(outbox.is_empty());
    }

    #[test]
    fn count_cap_drops_the_oldest_and_counts_it() {
        let mut outbox = Outbox::new(limits(2, 1000));
        outbox.push(item('a', 1));
        outbox.push(item('b', 1));
        outbox.push(item('c', 1));
        assert_eq!(outbox.len(), 2);
        assert_eq!(outbox.dropped(), 1);
        // 'a', the oldest, is the one that went.
        assert_eq!(
            outbox.take_batch(1000, |_| 0),
            vec![item('b', 1), item('c', 1)]
        );
    }

    #[test]
    fn byte_cap_drops_as_many_old_items_as_needed() {
        let mut outbox = Outbox::new(limits(100, 10));
        outbox.push(item('a', 4));
        outbox.push(item('b', 4));
        // 4 + 4 + 8 = 16 > 10: both older items must go for the new one to fit.
        outbox.push(item('c', 8));
        assert_eq!(outbox.bytes(), 8);
        assert_eq!(outbox.dropped(), 2);
        assert_eq!(outbox.take_batch(1000, |_| 0), vec![item('c', 8)]);
    }

    #[test]
    fn byte_accounting_tracks_every_push_and_take() {
        let mut outbox = Outbox::new(limits(100, 1000));
        for len in [5, 7, 11] {
            outbox.push(item('x', len));
        }
        assert_eq!(outbox.bytes(), 23);
        let _ = outbox.take_batch(12, |_| 0);
        assert_eq!(outbox.bytes(), 11);
    }

    #[test]
    fn an_item_larger_than_the_item_cap_is_dropped_on_arrival() {
        let mut outbox = Outbox::new(Limits {
            max_items: 10,
            max_bytes: 100,
            max_item_bytes: 5,
        });
        outbox.push(item('a', 3));
        outbox.push(item('b', 6));
        assert_eq!(outbox.len(), 1, "the older, small item must survive");
        assert_eq!(outbox.dropped(), 1);
    }

    #[test]
    fn batches_respect_the_byte_limit_including_overhead() {
        let mut outbox = Outbox::new(limits(100, 1000));
        for _ in 0..10 {
            outbox.push(item('x', 10));
        }
        // 20 bytes of envelope plus 1 per item: 3 items = 30 + 23 = 53 ≤ 55,
        // 4 items = 40 + 24 = 64 > 55.
        let batch = outbox.take_batch(55, |count| 20 + count);
        assert_eq!(batch.len(), 3);
        assert_eq!(outbox.len(), 7);
    }

    #[test]
    fn a_batch_never_skips_ahead_of_an_item_that_does_not_fit() {
        let mut outbox = Outbox::new(limits(100, 1000));
        outbox.push(item('a', 50));
        outbox.push(item('b', 1));
        // 'a' does not fit a 10-byte batch; 'b' would, but taking it first
        // would reorder the queue.
        assert!(outbox.take_batch(10, |_| 0).is_empty());
        assert_eq!(outbox.len(), 2);
    }

    #[test]
    fn discard_all_empties_and_counts() {
        let mut outbox = Outbox::new(limits(100, 1000));
        outbox.push(item('a', 1));
        outbox.push(item('b', 1));
        outbox.discard_all();
        assert!(outbox.is_empty());
        assert_eq!(outbox.bytes(), 0);
        assert_eq!(outbox.dropped(), 2);
        outbox.count_dropped(3);
        assert_eq!(outbox.dropped(), 5);
    }
}
