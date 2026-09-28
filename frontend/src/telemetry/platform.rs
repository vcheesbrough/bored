//! The three things the telemetry code needs from its host: a wall clock,
//! randomness, and nothing else.
//!
//! Each has a browser implementation and a host implementation, chosen at
//! compile time with `#[cfg(target_arch = "wasm32")]`. The host versions exist
//! so `cargo test -p frontend` — which runs on the host, where calling into
//! `web_sys` or `js_sys` panics — can exercise the real span and outbox paths
//! end to end rather than only the pure helpers.

/// Milliseconds since the Unix epoch, with sub-millisecond precision where the
/// browser offers it.
#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> f64 {
    // `performance.timeOrigin + performance.now()` is a high-resolution epoch
    // time; `Date.now()` is the fallback where there is no `performance`.
    web_sys::window()
        .and_then(|window| window.performance())
        .map(|performance| performance.time_origin() + performance.now())
        .unwrap_or_else(js_sys::Date::now)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64() * 1_000.0)
        .unwrap_or(0.0)
}

/// Nanoseconds since the Unix epoch, as OTLP timestamps want them. An `f64`
/// holds today's epoch in nanoseconds to within a few hundred nanoseconds,
/// far finer than a browser's clock resolution anyway.
pub fn now_nanos() -> u64 {
    (now_ms() * 1_000_000.0) as u64
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
