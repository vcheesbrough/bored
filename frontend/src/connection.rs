//! Liveness of this tab's link to the server, and the deploy-triggered reload.
//!
//! Two problems, one owner:
//!
//! 1. **A deploy leaves the tab on the old bundle.** The wasm the browser is
//!    running was compiled with a `RELEASE_TAG` burned in ([`shared::app_version`]),
//!    and after a redeploy `/api/info` starts answering with a different one.
//!    When those disagree, this tab is running code the server has moved past,
//!    so it reloads itself — same URL, so the user lands back where they were.
//! 2. **A dead link should not accept edits.** While the server is unreachable,
//!    or the board's SSE stream is down, the board on screen is stale: other
//!    people's changes are no longer arriving. Saving on top of that risks
//!    clobbering what this tab cannot see, so [`crate::api`] refuses every
//!    mutation while the state here is [`ConnectionState::Disconnected`], and
//!    the navbar says so.
//!
//! The previous attempt at (1) lived inside the SSE `onopen` handler in
//! `board_view`, and only ran when a prior `onerror` had fired. A redeploy
//! makes the proxy answer 502/503, which is *not* `text/event-stream`, so the
//! browser puts the `EventSource` in `CLOSED` and never retries — `onopen`
//! never fires again and the check never ran. That is why the version check now
//! lives on an independent `/api/info` heartbeat: it does not depend on the SSE
//! stream ever coming back, and it works on pages that have no stream at all
//! (home, boards list).
//!
//! The decision logic — [`reload_decision`], [`next_delay_ms`], [`combine`] —
//! is kept as pure functions over plain values, with no `web_sys` in sight, so
//! `cargo test -p frontend` can exercise it on the host target where there is
//! no DOM.

use leptos::prelude::*;
use std::cell::Cell;

/// How long to wait between heartbeats while everything is healthy.
///
/// The probe is a single unauthenticated GET that touches no database, so this
/// is cheap; it is the upper bound on how long a tab keeps running a version
/// the server has replaced.
const HEALTHY_INTERVAL_MS: u32 = 10_000;

/// Ceiling for the backoff while the server is unreachable. Deliberately the
/// same as the healthy interval: once a deploy is under way we want to notice
/// the new version promptly, and the request costs nothing when it fails fast.
const MAX_BACKOFF_MS: u32 = 10_000;

/// `sessionStorage` key recording the server version this tab last reloaded
/// for. Session-scoped on purpose: it must survive the reload it guards (which
/// `localStorage` would too) but must not outlive the tab, or a browser that
/// once saw a bad deploy would refuse to reload for that version ever again.
const RELOAD_MARKER_KEY: &str = "bored:reloaded-for-version";

/// Whether this tab currently has a live, current link to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// The heartbeat is answering and, on a board, the SSE stream is open.
    Connected,
    /// Either the server is unreachable or the board's event stream is dead.
    /// Mutations are refused in this state.
    Disconnected,
}

/// What to do about a version the server just reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadDecision {
    /// Versions agree (or the server reported nothing usable) — carry on.
    Stay,
    /// The server is on a different version: reload onto the new bundle.
    Reload,
    /// The versions disagree but this tab has *already* reloaded for exactly
    /// this server version and come back still mismatched. Reloading again
    /// would loop forever, so the tab stays where it is. In practice this means
    /// a runtime `APP_VERSION` that does not match the image's `RELEASE_TAG`,
    /// or a cached `index.html` re-serving the old bundle.
    Blocked,
}

/// The pieces of connection state, kept in one thread-local because the wasm
/// SPA is single-threaded and because [`crate::api`]'s plain `async fn`s need
/// to read this without being handed a signal from a component.
///
/// `state` is an [`ArcRwSignal`] rather than an [`RwSignal`] on purpose: an
/// `RwSignal` lives in the reactive arena owned by whichever component created
/// it, and would be disposed when that component unmounted. This one has to
/// outlive every component, so it is reference-counted instead.
struct Conn {
    /// Is `/api/info` answering?
    heartbeat_ok: Cell<bool>,
    /// Is the board's SSE stream open? Always `true` on pages that have no
    /// stream, so those pages track the heartbeat alone.
    stream_ok: Cell<bool>,
    /// `heartbeat_ok && stream_ok`, as a signal the UI can subscribe to.
    state: ArcRwSignal<ConnectionState>,
    /// The version `/api/info` reported on the last successful heartbeat, so
    /// the navbar watermark cannot go stale.
    server_version: ArcRwSignal<Option<String>>,
    /// Consecutive failed heartbeats, which drives the backoff.
    failures: Cell<u32>,
    /// Set once [`start`] has spawned the heartbeat, so a second call (a
    /// remount, say) does not start a second one.
    started: Cell<bool>,
    /// Guards against piling up probes when several SSE errors arrive at once.
    probing: Cell<bool>,
    /// Whether the "refusing to reload again" warning has already been logged,
    /// so a [`ReloadDecision::Blocked`] heartbeat does not spam the console
    /// every ten seconds.
    warned_blocked: Cell<bool>,
}

impl Default for Conn {
    fn default() -> Self {
        Self {
            // Optimistic on both counts: the document the browser is running
            // was served by this server moments ago, so starting in
            // `Disconnected` would flash an offline badge on every load and
            // refuse the user's first edit for no reason.
            heartbeat_ok: Cell::new(true),
            stream_ok: Cell::new(true),
            state: ArcRwSignal::new(ConnectionState::Connected),
            server_version: ArcRwSignal::new(None),
            failures: Cell::new(0),
            started: Cell::new(false),
            probing: Cell::new(false),
            warned_blocked: Cell::new(false),
        }
    }
}

thread_local! {
    static CONN: Conn = Conn::default();
}

/// The connection state as a signal, for components that want to re-render
/// when it changes.
pub fn state() -> ArcRwSignal<ConnectionState> {
    CONN.with(|c| c.state.clone())
}

/// The server version from the most recent successful heartbeat, or `None`
/// before the first one lands.
pub fn server_version() -> ArcRwSignal<Option<String>> {
    CONN.with(|c| c.server_version.clone())
}

/// Untracked read for non-reactive callers — [`crate::api`]'s mutation guard.
///
/// Untracked matters: an `async fn` reading a signal inside an effect's scope
/// would otherwise subscribe that effect to the connection state and re-run it
/// on every reconnect.
pub fn is_connected() -> bool {
    CONN.with(|c| c.state.get_untracked()) == ConnectionState::Connected
}

/// Fold the two independent health signals into the state the UI shows.
///
/// Pure so it can be tested on the host target.
pub(crate) fn combine(heartbeat_ok: bool, stream_ok: bool) -> ConnectionState {
    if heartbeat_ok && stream_ok {
        ConnectionState::Connected
    } else {
        ConnectionState::Disconnected
    }
}

/// Recompute the published state from the two health flags.
///
/// Writes only on an actual change: a `set` notifies subscribers whether or not
/// the value differs, and a heartbeat every ten seconds would otherwise
/// re-render the navbar forever.
fn publish() {
    CONN.with(|c| {
        let next = combine(c.heartbeat_ok.get(), c.stream_ok.get());
        if c.state.get_untracked() != next {
            c.state.set(next);
        }
    });
}

/// Record that the board's SSE stream went down (called from `board_view`'s
/// `onerror`) and probe the server immediately rather than waiting out the
/// heartbeat interval — during a deploy this is the first hint that a new
/// version may already be up.
pub fn stream_down() {
    CONN.with(|c| c.stream_ok.set(false));
    publish();
    probe_now();
}

/// Record that the board's SSE stream is open again.
pub fn stream_up() {
    CONN.with(|c| c.stream_ok.set(true));
    publish();
}

/// Forget about the stream entirely — called when a board view unmounts, so a
/// page with no SSE (home, boards list) is not left permanently "offline" by
/// the last board's dead stream.
pub fn stream_reset() {
    stream_up();
}

/// How long to wait before the next heartbeat, given the number of consecutive
/// failures so far.
///
/// Healthy (`0` failures) polls on the steady interval; after that it backs off
/// 1s → 2s → 4s → 8s and holds at [`MAX_BACKOFF_MS`], so a server that is down
/// for a while is not hammered, while a quick restart is noticed within a
/// second or two.
///
/// Pure so it can be tested on the host target.
pub(crate) fn next_delay_ms(failures: u32) -> u32 {
    if failures == 0 {
        return HEALTHY_INTERVAL_MS;
    }
    // `1_000 * 2^(failures - 1)`, saturating rather than wrapping — a tab left
    // open against a dead server will happily reach failure counts that would
    // overflow the shift.
    let step = 1_000u32.saturating_mul(1u32.checked_shl(failures - 1).unwrap_or(u32::MAX));
    step.min(MAX_BACKOFF_MS)
}

/// Decide what a reported server version means for this tab.
///
/// `client` is the version compiled into this bundle, `server` is what
/// `/api/info` just said, and `attempted` is the server version this tab has
/// already reloaded for (if any).
///
/// Pure so it can be tested on the host target.
pub(crate) fn reload_decision(
    client: &str,
    server: &str,
    attempted: Option<&str>,
) -> ReloadDecision {
    // An empty version is not evidence of anything — treat it as agreement
    // rather than reloading the user out of their work on a malformed answer.
    if server.is_empty() || client == server {
        return ReloadDecision::Stay;
    }
    if attempted == Some(server) {
        return ReloadDecision::Blocked;
    }
    ReloadDecision::Reload
}

/// Start the heartbeat. Idempotent: only the first call spawns the loop.
///
/// Called once from the root `App` component, so every page has it — including
/// the ones with no SSE stream.
pub fn start() {
    // `replace` returns the previous value, so this both tests and sets the
    // flag in one go.
    if CONN.with(|c| c.started.replace(true)) {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            probe().await;
            let delay = CONN.with(|c| next_delay_ms(c.failures.get()));
            gloo_timers::future::TimeoutFuture::new(delay).await;
        }
    });
}

/// Run one heartbeat out of band, unless one is already in flight.
///
/// The scheduled loop keeps running either way; this only shortens the wait.
fn probe_now() {
    if CONN.with(|c| c.probing.get()) {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        probe().await;
    });
}

/// One heartbeat: ask the server who it is, publish what that implies, and
/// reload the tab if the server has moved to a different version.
async fn probe() {
    CONN.with(|c| c.probing.set(true));
    let result = crate::api::fetch_app_info().await;
    CONN.with(|c| c.probing.set(false));

    let Ok(info) = result else {
        // Any failure counts: a refused connection, a 502 from the proxy while
        // the container restarts, or a body that is not the JSON we expect.
        CONN.with(|c| {
            c.failures.set(c.failures.get().saturating_add(1));
            c.heartbeat_ok.set(false);
        });
        publish();
        return;
    };

    CONN.with(|c| {
        c.failures.set(0);
        c.heartbeat_ok.set(true);
        if c.server_version.get_untracked().as_deref() != Some(info.version.as_str()) {
            c.server_version.set(Some(info.version.clone()));
        }
    });
    publish();

    match reload_decision(
        shared::app_version(),
        &info.version,
        read_marker().as_deref(),
    ) {
        ReloadDecision::Stay => {
            // Clear any marker from an earlier mismatch: the next deploy must
            // be free to reload this tab even if a previous one could not.
            clear_marker();
            CONN.with(|c| c.warned_blocked.set(false));
        }
        ReloadDecision::Reload => {
            // Write the marker *before* reloading. If the reload brings back
            // the same mismatched bundle, the next heartbeat reads this and
            // stops, so a misconfiguration costs one reload instead of a loop.
            write_marker(&info.version);
            leptos::logging::log!(
                "server is on {} (this tab is {}) — reloading",
                info.version,
                shared::app_version()
            );
            let _ = window().location().reload();
        }
        ReloadDecision::Blocked => {
            if !CONN.with(|c| c.warned_blocked.replace(true)) {
                leptos::logging::warn!(
                    "server reports {} but this tab is {} after reloading for it already — \
                     not reloading again (check APP_VERSION matches the image's RELEASE_TAG)",
                    info.version,
                    shared::app_version()
                );
            }
        }
    }
}

// ── `sessionStorage` marker ──────────────────────────────────────────────
//
// Wrapped rather than inlined because every access can fail in three ways
// (no window, storage disabled by the browser, quota) and none of them is
// worth failing a heartbeat over: a missing marker only means the loop guard
// is unavailable, which is how it behaved before this existed.

fn session_storage() -> Option<web_sys::Storage> {
    window().session_storage().ok().flatten()
}

fn read_marker() -> Option<String> {
    session_storage()?
        .get_item(RELOAD_MARKER_KEY)
        .ok()
        .flatten()
}

fn write_marker(version: &str) {
    if let Some(store) = session_storage() {
        let _ = store.set_item(RELOAD_MARKER_KEY, version);
    }
}

fn clear_marker() {
    if let Some(store) = session_storage() {
        let _ = store.remove_item(RELOAD_MARKER_KEY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── reload_decision ──────────────────────────────────────────────────

    #[test]
    fn matching_versions_stay() {
        assert_eq!(
            reload_decision("1.58.0", "1.58.0", None),
            ReloadDecision::Stay
        );
    }

    #[test]
    fn a_different_server_version_reloads() {
        assert_eq!(
            reload_decision("1.58.0", "1.59.0", None),
            ReloadDecision::Reload
        );
    }

    #[test]
    fn a_downgrade_reloads_too() {
        // Rolling back a bad deploy moves the server *backwards*; the tab is
        // just as wrong as it is after an upgrade, so this is not a `>` test.
        assert_eq!(
            reload_decision("1.59.0", "1.58.0", None),
            ReloadDecision::Reload
        );
    }

    #[test]
    fn an_empty_server_version_is_not_evidence() {
        assert_eq!(reload_decision("1.58.0", "", None), ReloadDecision::Stay);
    }

    #[test]
    fn reloading_twice_for_the_same_version_is_blocked() {
        assert_eq!(
            reload_decision("1.58.0", "1.59.0", Some("1.59.0")),
            ReloadDecision::Blocked
        );
    }

    #[test]
    fn a_marker_for_another_version_does_not_block() {
        // The tab gave up on 1.59.0, but 1.60.0 is a fresh deploy and deserves
        // its own attempt.
        assert_eq!(
            reload_decision("1.58.0", "1.60.0", Some("1.59.0")),
            ReloadDecision::Reload
        );
    }

    // ── next_delay_ms ────────────────────────────────────────────────────

    #[test]
    fn a_healthy_heartbeat_polls_on_the_steady_interval() {
        assert_eq!(next_delay_ms(0), HEALTHY_INTERVAL_MS);
    }

    #[test]
    fn failures_back_off_by_doubling() {
        assert_eq!(next_delay_ms(1), 1_000);
        assert_eq!(next_delay_ms(2), 2_000);
        assert_eq!(next_delay_ms(3), 4_000);
        assert_eq!(next_delay_ms(4), 8_000);
    }

    #[test]
    fn backoff_holds_at_the_ceiling() {
        assert_eq!(next_delay_ms(5), MAX_BACKOFF_MS);
        assert_eq!(next_delay_ms(20), MAX_BACKOFF_MS);
        // Far enough out that a naive `1 << (failures - 1)` would have
        // overflowed and panicked in debug.
        assert_eq!(next_delay_ms(u32::MAX), MAX_BACKOFF_MS);
    }

    // ── combine ──────────────────────────────────────────────────────────

    #[test]
    fn both_healthy_is_connected() {
        assert_eq!(combine(true, true), ConnectionState::Connected);
    }

    #[test]
    fn either_failure_is_disconnected() {
        // A dead stream alone counts: the board is stale even though HTTP
        // still works, which is exactly when an edit would clobber unseen
        // changes.
        assert_eq!(combine(true, false), ConnectionState::Disconnected);
        assert_eq!(combine(false, true), ConnectionState::Disconnected);
        assert_eq!(combine(false, false), ConnectionState::Disconnected);
    }
}
