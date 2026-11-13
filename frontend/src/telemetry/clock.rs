//! The anchored clock that stamps SPA spans (card #458).
//!
//! # Why not just `performance.timeOrigin + performance.now()`
//!
//! A browser has two clocks, and neither is right on its own:
//!
//! - **`Date.now()`** is the wall clock: milliseconds since the Unix epoch, in
//!   step with the server's clock (both follow NTP). But it can jump — the OS
//!   corrects it, the user changes it — so the gap between two readings is not
//!   a trustworthy duration.
//! - **`performance.now()`** is monotonic: it never jumps and has sub-
//!   millisecond resolution, so the gap between two readings *is* a duration.
//!   But it is measured from `performance.timeOrigin` (when the document was
//!   created), and the sum `timeOrigin + now()` drifts away from the wall
//!   clock: headless Chromium was seen running 1.09–1.19 s behind (card
//!   #454), and on a real device `now()` can stop while the machine sleeps,
//!   so a tab left open overnight stamps its spans hours in the past.
//!
//! The SPA used that sum, so its spans could sit seconds (or hours) before the
//! server spans they parent in Tempo.
//!
//! # The fix: take each clock for what it is good at
//!
//! OpenTelemetry JS solved this with an *anchored clock*, and this is the same
//! idea. When a trace's root span starts, read **both** clocks once and keep
//! the pair — the [`ClockAnchor`]. Every timestamp in that trace is then
//!
//! ```text
//! anchor.wall + (performance.now() − anchor.monotonic)
//! ```
//!
//! — the wall clock's *epoch*, plus a *duration* measured on the monotonic
//! clock. The root starts at the wall clock's time; durations never jump.
//!
//! # Why the anchor is shared across the trace
//!
//! A child span reuses its parent's anchor (it travels inside the parent's
//! [`SpanContext`](super::otlp::SpanContext)) instead of taking a fresh one.
//! With one anchor and one monotonic clock, timestamps inside a trace are
//! ordered exactly as the events were: a child that started after its parent
//! can never be stamped before it, and a child that ended before its parent
//! can never be stamped after it. Two independent `Date.now()` readings could
//! not promise that — the wall clock may have been corrected between them.
//!
//! The one cost, the same one OpenTelemetry JS accepts: if the device sleeps
//! *while a trace is open*, the monotonic clock stops, and the sleep is
//! missing from that trace's durations. The next trace re-anchors, so the
//! error never outlives one trace — unlike the old clock, where it outlived
//! the whole page.

use super::otlp::UnixNanos;
use super::platform;

/// A trace's timeline: where the monotonic clock's zero falls on the wall
/// clock, worked out once, when the trace's root span starts.
///
/// Reading both clocks at the same moment gives `wall` and `monotonic`; the
/// epoch time at which the monotonic clock read zero is then
/// `origin = wall − monotonic`, and any later monotonic reading `m` is the
/// epoch time `origin + m`. That is the formula in the module doc —
/// `wall + (m − monotonic)` — with the two constants folded into one.
///
/// It is `performance.timeOrigin` again, in effect, but *re-measured against
/// `Date.now()` for every trace* rather than fixed when the document was
/// created — which is exactly what removes the drift.
///
/// Stored as one integer: integer nanoseconds make the arithmetic exact, with
/// no float rounding to reason about, and let the type derive `Eq` and `Hash`,
/// which the [`SpanContext`](super::otlp::SpanContext) it lives in derives.
///
/// `#[derive(...)]` asks the compiler to write those trait implementations:
/// `Copy` makes the anchor a plain value that is duplicated on assignment
/// rather than moved, which is what a number should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClockAnchor {
    /// Nanoseconds since the Unix epoch at which this trace's monotonic clock
    /// read zero.
    origin_nanos: u64,
}

impl ClockAnchor {
    /// An anchor from a wall-clock and a monotonic reading taken together, in
    /// nanoseconds. Public so tests can build one from hand-picked numbers.
    ///
    /// `saturating_sub` stops at zero instead of wrapping around to a time in
    /// the year 2554. The subtraction can only go below zero in the
    /// no-`performance` fallback, where the "monotonic" clock *is* the wall
    /// clock (see [`platform::monotonic_nanos`]) and was read a tick after
    /// the wall reading; the true origin there is the epoch itself, so zero
    /// is the right answer anyway.
    pub const fn new(wall_nanos: u64, monotonic_nanos: u64) -> Self {
        Self {
            origin_nanos: wall_nanos.saturating_sub(monotonic_nanos),
        }
    }

    /// A fresh anchor: read both clocks now. This is what a root span does.
    pub fn now() -> Self {
        Self::new(platform::wall_nanos(), platform::monotonic_nanos())
    }

    /// The epoch time of the moment the monotonic clock read
    /// `monotonic_nanos`, on this anchor's timeline. `saturating_add` caps at
    /// the largest `u64` rather than wrapping round to 1970.
    pub const fn at(self, monotonic_nanos: u64) -> UnixNanos {
        UnixNanos(self.origin_nanos.saturating_add(monotonic_nanos))
    }

    /// The current time on this anchor's timeline.
    pub fn read(self) -> UnixNanos {
        self.at(platform::monotonic_nanos())
    }
}

#[cfg(test)]
mod tests {
    //! The arithmetic, with hand-worked numbers. Each expected value below is
    //! written out as a literal, worked from the formula in the module doc,
    //! never computed by calling the code under test.
    use super::*;

    /// 2023-11-14T22:13:20Z, a round epoch time in nanoseconds.
    const WALL: u64 = 1_700_000_000_000_000_000;

    #[test]
    fn a_reading_is_the_wall_epoch_plus_the_monotonic_duration() {
        // Anchored when the monotonic clock read 5 s; read again at 5.25 s.
        // 0.25 s later than the anchor's wall time:
        // 1_700_000_000_000_000_000 + 250_000_000.
        let anchor = ClockAnchor::new(WALL, 5_000_000_000);
        assert_eq!(
            anchor.at(5_250_000_000),
            UnixNanos(1_700_000_000_250_000_000)
        );
    }

    #[test]
    fn the_anchor_moment_is_the_wall_time_itself() {
        // Read at the very moment of anchoring: exactly the wall time, not
        // the monotonic clock's own (document-relative) value.
        let anchor = ClockAnchor::new(WALL, 5_000_000_000);
        assert_eq!(anchor.at(5_000_000_000), UnixNanos(WALL));
    }

    #[test]
    fn sub_millisecond_durations_survive() {
        // 1.5 µs after the anchor: the monotonic clock's resolution is kept,
        // not rounded to the wall clock's whole milliseconds.
        let anchor = ClockAnchor::new(WALL, 7_000);
        assert_eq!(anchor.at(8_500), UnixNanos(1_700_000_000_000_001_500));
    }

    #[test]
    fn the_fallback_clock_reads_as_the_wall_clock() {
        // No `performance`: both readings are `Date.now()`, so the origin is
        // the epoch and a later reading of 1_700_000_000_250 ms is exactly
        // that epoch time.
        let anchor = ClockAnchor::new(WALL, WALL);
        assert_eq!(
            anchor.at(1_700_000_000_250_000_000),
            UnixNanos(1_700_000_000_250_000_000)
        );
        // The "monotonic" reading taken a millisecond tick after the wall
        // reading: the origin would be −1 ms, and is held at the epoch rather
        // than wrapping round to the year 2554.
        let anchor = ClockAnchor::new(WALL, 1_700_000_000_001_000_000);
        assert_eq!(
            anchor.at(1_700_000_000_002_000_000),
            UnixNanos(1_700_000_000_002_000_000)
        );
    }

    #[test]
    fn an_overflowing_reading_caps_instead_of_wrapping() {
        let anchor = ClockAnchor::new(u64::MAX - 10, 0);
        assert_eq!(anchor.at(100), UnixNanos(u64::MAX));
    }
}
