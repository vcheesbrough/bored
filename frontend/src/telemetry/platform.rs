//! What the telemetry code needs from its host: two clocks (wall and
//! monotonic — [`super::clock`] explains why both), randomness, and nothing
//! else.
//!
//! Each has a browser implementation and a host implementation, chosen at
//! compile time with `#[cfg(target_arch = "wasm32")]`. The host versions exist
//! so `cargo test -p frontend` — which runs on the host, where calling into
//! `web_sys` or `js_sys` panics — can exercise the real span and outbox paths
//! end to end rather than only the pure helpers.

/// The wall clock: milliseconds since the Unix epoch, as `Date.now()` gives
/// them — in step with the server's clock, but free to jump when the OS
/// corrects it.
///
/// The exporter judges token expiry on this: a token's lifetime is wall time
/// — it expires on the issuer's clock, sleep or no sleep. Its *waits* (retry
/// back-off, the launch buffer) run on [`monotonic_nanos`] instead, which a
/// clock correction cannot stretch (see `runtime::Now`). Spans are *not*
/// stamped with this directly: see [`super::clock`].
#[cfg(target_arch = "wasm32")]
pub fn wall_ms() -> f64 {
    js_sys::Date::now()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn wall_ms() -> f64 {
    // In tests the fake clock, when set, wins (see `fake_clock`).
    #[cfg(test)]
    if let Some((wall_nanos, _)) = fake_clock::get() {
        return wall_nanos as f64 / 1_000_000.0;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64() * 1_000.0)
        .unwrap_or(0.0)
}

/// The wall clock in nanoseconds since the Unix epoch, as OTLP timestamps
/// want them. Log records are stamped with this (card #458): a log line is a
/// point in time, with no duration to keep monotonic.
#[cfg(target_arch = "wasm32")]
pub fn wall_nanos() -> u64 {
    ms_to_nanos(js_sys::Date::now())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn wall_nanos() -> u64 {
    #[cfg(test)]
    if let Some((wall_nanos, _)) = fake_clock::get() {
        return wall_nanos;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // `as u64`: a `u128` of nanoseconds fits a `u64` until the year 2554.
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
}

/// The monotonic clock, in nanoseconds from an arbitrary origin: only the
/// difference between two readings means anything. `performance.now()` in the
/// browser, which never jumps backwards.
///
/// Where there is no `performance` (no `window`, or a very old engine) it
/// falls back to `Date.now()`, which can jump backwards; [`never_backwards`]
/// holds such a jump at the last reading, so a child span can still never be
/// stamped before its parent.
#[cfg(target_arch = "wasm32")]
pub fn monotonic_nanos() -> u64 {
    use std::cell::Cell;

    thread_local! {
        /// The latest reading handed out. The browser runs this module on one
        /// thread, so this is simply a page-wide variable.
        static LAST: Cell<u64> = const { Cell::new(0) };
    }
    let ms = web_sys::window()
        .and_then(|window| window.performance())
        .map_or_else(js_sys::Date::now, |performance| performance.now());
    LAST.with(|last| never_backwards(last, ms_to_nanos(ms)))
}

/// `reading`, or the previous one if the clock went backwards — and remember
/// the result for next time. What makes the `Date.now()` fallback in
/// [`monotonic_nanos`] monotonic. `Cell` lets a shared (`&`) reference update
/// the value it holds, which is all a single-threaded high-water mark needs.
// Only the browser build calls it; the host tests still exercise it.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn never_backwards(last: &std::cell::Cell<u64>, reading: u64) -> u64 {
    let monotonic = reading.max(last.get());
    last.set(monotonic);
    monotonic
}

#[cfg(not(target_arch = "wasm32"))]
pub fn monotonic_nanos() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;

    #[cfg(test)]
    if let Some((_, monotonic_nanos)) = fake_clock::get() {
        return monotonic_nanos;
    }
    // `OnceLock` holds a value set exactly once, on first use: here, the
    // process's "time origin", standing in for the document's.
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Milliseconds as a browser clock gives them (an `f64`, possibly fractional)
/// to whole nanoseconds.
///
/// The whole milliseconds and the fraction are converted separately. Today's
/// epoch in nanoseconds (~1.7 × 10¹⁸) is beyond what an `f64` holds exactly
/// (integers up to 2⁵³ ≈ 9 × 10¹⁵), so `ms * 1e6` would land up to ~128 ns
/// off; multiplying the whole milliseconds as a `u64` is exact, and the
/// fraction is small enough for `f64` to carry precisely.
///
/// `.round()` on the fraction, so `0.0015` ms (1.5 µs, which `f64` stores as
/// a hair under) becomes `1_500` ns rather than `1_499`. `as u64` on a
/// negative or NaN `f64` gives 0 — Rust's float-to-int casts saturate — so a
/// nonsense reading becomes the epoch rather than undefined behaviour.
pub fn ms_to_nanos(ms: f64) -> u64 {
    let whole = ms.trunc();
    let fraction_nanos = ((ms - whole) * 1_000_000.0).round() as u64;
    (whole as u64)
        .saturating_mul(1_000_000)
        .saturating_add(fraction_nanos)
}

/// A clock the host tests set by hand, so span and log timestamps can be
/// checked against exact, hand-worked numbers — including a wall clock and a
/// monotonic clock that disagree, as they do in a browser.
///
/// `thread_local!`: each test runs on its own thread, so one test's fake time
/// never leaks into another's. Not compiled outside `cargo test`.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub mod fake_clock {
    use std::cell::Cell;

    thread_local! {
        /// `(wall_nanos, monotonic_nanos)`, or `None` for the real clocks.
        static NOW: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
    }

    /// Fix both clocks at the given readings until the next `set`.
    pub fn set(wall_nanos: u64, monotonic_nanos: u64) {
        NOW.with(|now| now.set(Some((wall_nanos, monotonic_nanos))));
    }

    pub fn get() -> Option<(u64, u64)> {
        NOW.with(Cell::get)
    }
}

/// `N` random bytes for a trace or span id.
///
/// `const N: usize` makes the byte count a compile-time parameter, so the
/// caller gets a fixed-size array (`[u8; 16]` for a trace id) with no heap
/// allocation.
#[cfg(target_arch = "wasm32")]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    // `crypto.getRandomValues` where it exists; ids are not secrets, so
    // `Math.random` is an acceptable fallback rather than a reason to fail.
    let filled = web_sys::window()
        .and_then(|window| window.crypto().ok())
        .is_some_and(|crypto| crypto.get_random_values_with_u8_array(&mut bytes).is_ok());
    if !filled {
        for byte in &mut bytes {
            *byte = (js_sys::Math::random() * 256.0) as u8;
        }
    }
    bytes
}

#[cfg(not(target_arch = "wasm32"))]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    for byte in &mut bytes {
        *byte = host_rng::next() as u8;
    }
    bytes
}

/// A number in `[0, 1)` for backoff jitter.
#[cfg(target_arch = "wasm32")]
pub fn unit_random() -> f64 {
    js_sys::Math::random()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn unit_random() -> f64 {
    // The top 53 bits of a 64-bit value, scaled into [0, 1).
    (host_rng::next() >> 11) as f64 / (1u64 << 53) as f64
}

/// A tiny xorshift generator for host tests only — ids must differ, nothing
/// more. Not compiled into the browser bundle at all.
#[cfg(not(target_arch = "wasm32"))]
mod host_rng {
    use std::cell::Cell;

    // `thread_local!` gives each test thread its own generator, so parallel
    // tests never share (or race on) state.
    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
                | 1,
        );
    }

    pub fn next() -> u64 {
        STATE.with(|state| {
            let mut x = state.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            state.set(x);
            x
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_milliseconds_become_whole_nanoseconds() {
        // Hand-worked: 1 ms = 1_000_000 ns.
        assert_eq!(ms_to_nanos(1.5), 1_500_000);
        // 1.5 µs, which an `f64` holds as 0.00149999…: rounded, not truncated.
        assert_eq!(ms_to_nanos(0.0015), 1_500);
        // `Date.now()` today: an integer millisecond count.
        assert_eq!(ms_to_nanos(1_700_000_000_123.0), 1_700_000_000_123_000_000);
        // Nonsense readings become 0 rather than wrapping.
        assert_eq!(ms_to_nanos(-1.0), 0);
        assert_eq!(ms_to_nanos(f64::NAN), 0);
    }

    #[test]
    fn a_clock_that_steps_back_is_held_at_its_last_reading() {
        let last = std::cell::Cell::new(0);
        assert_eq!(never_backwards(&last, 5_000), 5_000);
        // The wall-clock fallback steps back 2 µs: held at 5_000.
        assert_eq!(never_backwards(&last, 3_000), 5_000);
        // …and moves on again once the clock passes it.
        assert_eq!(never_backwards(&last, 6_000), 6_000);
    }
}
