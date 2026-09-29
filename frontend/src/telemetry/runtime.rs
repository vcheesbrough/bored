//! The exporter's state and its loop: what is buffered, whether telemetry is
//! on, the cached token, and the 5-second export tick (card #416).
//!
//! # One owner, never borrowed across an `await`
//!
//! All state lives in one `thread_local!` `RefCell` (a wasm SPA has exactly one
//! thread). Every access goes through [`with_state`], which uses
//! `try_borrow_mut` and simply does nothing when the state is already
//! borrowed. That matters in exactly one place — the panic hook — where a
//! panic raised *inside* telemetry code would otherwise meet a held borrow,
//! panic again, and lose even the console line. No borrow is ever held across
//! an `.await`: the export tick takes what it needs, releases the borrow,
//! awaits the network, then borrows again to record the outcome.

use std::cell::RefCell;

use super::config::{self, Decision};
use super::otlp::{self, LogRecord, Signal, Span, SpanContext};
use super::outbox::{Limits, Outbox};
use super::platform;
use super::policy::{ExportPolicy, StopReason, Transition, Verdict};

/// How often buffered telemetry is sent (`client-export.md`: a 5 s tick).
pub const EXPORT_INTERVAL_MS: u32 = 5_000;

/// Largest request body the exporter builds. The ingest caps decompressed
/// bodies at 4 MiB (`MAX_REQUEST_BODY_BYTES`); staying far below it means a
/// `413` can only come from a misconfigured ingest, never from us.
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Largest body per signal for the unload flush. A `keepalive` request shares
/// a 64 KiB budget with every other keepalive request still in flight from the
/// page: the flush sends two (traces and logs), and an ordinary export may be
/// in flight as a keepalive request too (see [`ROUTINE_KEEPALIVE_BYTES`]), so
/// 2 × 24 KiB + 16 KiB = 64 KiB at most. A request over the budget is refused
/// by the browser outright, which would silently lose the panic's log.
const KEEPALIVE_BATCH_BYTES: usize = 24 * 1024;

/// Largest ordinary export sent as a keepalive request (so leaving the page
/// mid-export does not cancel it). Larger ones are plain requests.
const ROUTINE_KEEPALIVE_BYTES: usize = 16 * 1024;

// The budget arithmetic above, checked at compile time.
const _: () = assert!(2 * KEEPALIVE_BATCH_BYTES + ROUTINE_KEEPALIVE_BYTES <= 64 * 1024);

/// The caps on each signal's outbox.
const OUTBOX_LIMITS: Limits = Limits {
    max_items: 1_000,
    max_bytes: 512 * 1024,
    // Must fit the smallest batch taken (the keepalive one), or an item could
    // never leave. The panic message is truncated well below this.
    max_item_bytes: 16 * 1024,
};

/// A cached token is refetched this long before the product says it expires,
/// so a request never leaves with a token that dies in flight.
const TOKEN_MARGIN_MS: f64 = 30_000.0;

/// Where telemetry stands for the session.
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// No configuration yet. Spans and records are buffered (bounded), for at
    /// most [`config::LAUNCH_BUFFER_MS`] from `since_ms`.
    Pending { since_ms: f64 },
    /// Export to `endpoint`.
    Enabled { endpoint: String },
    /// OTLP is not initialised for this session. Nothing is buffered.
    Disabled,
}

/// A batch waiting to be sent again after a retryable answer.
#[derive(Debug)]
struct RetryBatch {
    items: Vec<String>,
    /// The attempt number the *next* send will be.
    attempt: u32,
    not_before_ms: f64,
}

/// The bearer the ingest wants, as last handed out by `/api/telemetry/token`.
struct CachedToken {
    value: String,
    valid_until_ms: f64,
}

pub struct State {
    pub phase: Phase,
    spans: Outbox,
    logs: Outbox,
    policy: ExportPolicy,
    token: Option<CachedToken>,
    /// Index 0 for traces, 1 for logs.
    retry: [Option<RetryBatch>; 2],
    /// An export tick is in flight; a second one must not start.
    exporting: bool,
    /// The most recent screen-load span, which a panic's span is parented to
    /// so the panic lands in a trace with the screen it happened on.
    pub current_screen: Option<SpanContext>,
    /// The `service.version` the bundle states.
    version: &'static str,
}

impl State {
    fn new(now_ms: f64) -> Self {
        Self {
            phase: Phase::Pending { since_ms: now_ms },
            spans: Outbox::new(OUTBOX_LIMITS),
            logs: Outbox::new(OUTBOX_LIMITS),
            policy: ExportPolicy::default(),
            token: None,
            retry: [None, None],
            exporting: false,
            current_screen: None,
            version: shared::app_version(),
        }
    }

    fn outbox(&mut self, signal: Signal) -> &mut Outbox {
        match signal {
            Signal::Traces => &mut self.spans,
            Signal::Logs => &mut self.logs,
        }
    }

    /// Whether spans and records are accepted at all right now.
    pub fn recording(&self) -> bool {
        !matches!(self.phase, Phase::Disabled) && self.policy.stopped().is_none()
    }

    /// Total items lost to caps, refusals and give-ups — the figure every
    /// console transition line reports.
    fn dropped(&self) -> u64 {
        self.spans.dropped() + self.logs.dropped()
    }

    fn endpoint(&self) -> &str {
        match &self.phase {
            Phase::Enabled { endpoint } => endpoint,
            _ => "(none)",
        }
    }

    /// Buffer one finished span.
    pub fn record_span(&mut self, span: &Span) {
        if !self.recording() {
            return;
        }
        // Serializing our own plain structs cannot fail; a failure would be a
        // bug, and dropping the span is the right response to it anyway.
        if let Ok(encoded) = serde_json::to_string(span) {
            self.spans.push(encoded);
        }
    }

    /// Buffer one log record.
    pub fn record_log(&mut self, record: &LogRecord) {
        if !self.recording() {
            return;
        }
        if let Ok(encoded) = serde_json::to_string(record) {
            self.logs.push(encoded);
        }
    }

    /// Throw everything buffered away and forget any retry, counting it all.
    fn discard_everything(&mut self) {
        self.spans.discard_all();
        self.logs.discard_all();
        for (index, slot) in self.retry.iter_mut().enumerate() {
            if let Some(batch) = slot.take() {
                let outbox = if index == 0 {
                    &mut self.spans
                } else {
                    &mut self.logs
                };
                outbox.count_dropped(batch.items.len());
            }
        }
    }

    /// Apply the product's configuration. Returns the console line to print,
    /// if the decision changed anything worth one.
    ///
    /// The first answer decides the session. A later answer can only switch
    /// telemetry *off* ("honour the newest, including off"); it never switches
    /// a disabled session on, because what was buffered has already gone.
    pub fn configure(&mut self, config: Option<&shared::ClientTelemetryConfig>) -> Option<String> {
        let decision = config::decide(config);
        match (&self.phase, decision) {
            (Phase::Pending { .. }, Decision::Enabled { endpoint }) => {
                self.phase = Phase::Enabled { endpoint };
                None
            }
            (Phase::Pending { .. } | Phase::Enabled { .. }, Decision::Disabled) => {
                self.phase = Phase::Disabled;
                self.discard_everything();
                Some(NOT_INITIALISED.to_string())
            }
            // Already decided; nothing new.
            _ => None,
        }
    }

    /// Housekeeping at each tick: give up waiting for configuration once the
    /// launch buffer's time is up. Returns a console line when that happens.
    pub fn expire_launch_buffer(&mut self, now_ms: f64) -> Option<String> {
        if let Phase::Pending { since_ms } = self.phase
            && now_ms - since_ms >= config::LAUNCH_BUFFER_MS
        {
            self.phase = Phase::Disabled;
            self.discard_everything();
            return Some(NOT_INITIALISED.to_string());
        }
        None
    }

    /// Record a verdict for a batch of `signal` that was sent as `attempt`,
    /// and return the console line for any transition.
    fn apply_verdict(
        &mut self,
        signal: Signal,
        items: Vec<String>,
        attempt: u32,
        verdict: Verdict,
        transition: Option<Transition>,
        now_ms: f64,
    ) -> Option<String> {
        let index = signal_index(signal);
        match verdict {
            Verdict::Sent => {}
            Verdict::Retry { delay_ms } => {
                self.retry[index] = Some(RetryBatch {
                    items,
                    attempt: attempt + 1,
                    not_before_ms: now_ms + f64::from(delay_ms),
                });
            }
            Verdict::Dropped { refresh_token } => {
                self.outbox(signal).count_dropped(items.len());
                if refresh_token {
                    self.token = None;
                }
            }
            Verdict::Stop(_) => {
                self.outbox(signal).count_dropped(items.len());
                self.discard_everything();
                self.token = None;
            }
        }
        transition.map(|transition| self.describe(transition))
    }

    /// The console line for a transition. Status, endpoint and the dropped
    /// count — never the token, never anything personal.
    fn describe(&self, transition: Transition) -> String {
        let endpoint = self.endpoint();
        let dropped = self.dropped();
        match transition {
            Transition::FirstFailure { status: 0 } => format!(
                "telemetry: export to {endpoint} failing (no response); {dropped} dropped so far"
            ),
            Transition::FirstFailure { status } => format!(
                "telemetry: export to {endpoint} failing (HTTP {status}); {dropped} dropped so far"
            ),
            Transition::Recovered => {
                format!("telemetry: export to {endpoint} working again; {dropped} dropped in total")
            }
            Transition::GaveUp(reason) => {
                let why = match reason {
                    StopReason::Unauthorized => {
                        "the ingest refused a freshly refreshed token (HTTP 401)".to_string()
                    }
                    StopReason::RepeatedFailure => "repeated failures".to_string(),
                    StopReason::TokenUnavailable(status) => {
                        format!("no telemetry token from the app (HTTP {status})")
                    }
                };
                format!(
                    "telemetry: giving up on {endpoint} for this session: {why}; {dropped} dropped"
                )
            }
        }
    }

    /// Take the next batch of `signal` to send, if one is due: a waiting retry
    /// first (in order), else fresh items. Returns `(items, attempt)`.
    fn next_batch(
        &mut self,
        signal: Signal,
        now_ms: f64,
        max_bytes: usize,
    ) -> Option<(Vec<String>, u32)> {
        let index = signal_index(signal);
        if let Some(retry) = &self.retry[index] {
            if retry.not_before_ms > now_ms {
                // A retry is waiting and not yet due: send nothing newer
                // ahead of it, and nothing at all to an ingest that asked us
                // to back off.
                return None;
            }
            return self.retry[index]
                .take()
                .map(|retry| (retry.items, retry.attempt));
        }
        let version = self.version;
        let items = self.outbox(signal).take_batch(max_bytes, |count| {
            otlp::envelope_overhead(signal, version, count)
        });
        (!items.is_empty()).then_some((items, 1))
    }

    /// The batch of `signal` to send on the way out, sized for a keepalive
    /// request. A waiting retry goes first whether or not it is due — there is
    /// no later — provided it fits the keepalive budget (it was taken for an
    /// ordinary request, which may be far larger); otherwise fresh items go.
    fn unload_batch(&mut self, signal: Signal) -> Option<Vec<String>> {
        let index = signal_index(signal);
        let version = self.version;
        if let Some(retry) = &self.retry[index] {
            let size = retry.items.iter().map(String::len).sum::<usize>()
                + otlp::envelope_overhead(signal, version, retry.items.len());
            if size <= KEEPALIVE_BATCH_BYTES {
                return self.retry[index].take().map(|retry| retry.items);
            }
        }
        let items = self
            .outbox(signal)
            .take_batch(KEEPALIVE_BATCH_BYTES, |count| {
                otlp::envelope_overhead(signal, version, count)
            });
        (!items.is_empty()).then_some(items)
    }

    /// The cached token, if it is still good at `now_ms`.
    fn valid_token(&self, now_ms: f64) -> Option<String> {
        self.token
            .as_ref()
            .filter(|token| token.valid_until_ms > now_ms)
            .map(|token| token.value.clone())
    }
}

/// The line that answers "why are there no spans from this build".
pub const NOT_INITIALISED: &str =
    "telemetry: no telemetry configuration from the server, OTLP not initialised";

fn signal_index(signal: Signal) -> usize {
    match signal {
        Signal::Traces => 0,
        Signal::Logs => 1,
    }
}

thread_local! {
    // `RefCell` gives checked, run-time borrowing of the one `State`; the
    // `thread_local!` makes it a per-thread global without `unsafe`.
    static STATE: RefCell<State> = RefCell::new(State::new(platform::now_ms()));
}

/// Run `f` against the state, or return `None` if it is already borrowed (see
/// the module doc for why that is not an error).
pub fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok().map(|mut state| f(&mut state)))
}

/// Print a transition line at warning level — the console is the one place a
/// broken export can report itself.
fn warn(line: Option<String>) {
    if let Some(line) = line {
        leptos::logging::warn!("{line}");
    }
}

// ── The export loop (browser only in practice) ───────────────────────────

/// Start the tick and the unload flush. Call once, from `main`.
pub fn start() {
    // `Interval` repeats until dropped; `forget` hands it to the browser for
    // the life of the page.
    gloo_timers::callback::Interval::new(EXPORT_INTERVAL_MS, || {
        wasm_bindgen_futures::spawn_local(tick());
    })
    .forget();
    install_unload_flush();
}

/// One export pass: at most one batch per signal.
async fn tick() {
    let now = platform::now_ms();
    // Decide, under one short borrow, whether there is anything to do.
    let proceed = with_state(|state| {
        warn(state.expire_launch_buffer(now));
        let idle = state.spans.is_empty()
            && state.logs.is_empty()
            && state.retry.iter().all(Option::is_none);
        let ready = matches!(state.phase, Phase::Enabled { .. })
            && state.policy.stopped().is_none()
            && !state.exporting
            && !idle;
        if ready {
            state.exporting = true;
        }
        ready
    })
    .unwrap_or(false);
    if !proceed {
        return;
    }
    export_pass(now).await;
    with_state(|state| state.exporting = false);
}

async fn export_pass(now: f64) {
    for signal in [Signal::Traces, Signal::Logs] {
        // The token is read per request, not once per pass: a `401` on the
        // traces batch drops the cached token, and the logs batch must then
        // go out with a fresh one — that is the one refresh a `401` gets.
        // Reusing the refused token would spend it on a token that never
        // changed. Nothing is sent before the session is known to be good: a
        // token handed out by the product is that proof.
        let Some(token) = ensure_token(now).await else {
            return;
        };
        let batch = with_state(|state| {
            let endpoint = match &state.phase {
                Phase::Enabled { endpoint } => endpoint.clone(),
                _ => return None,
            };
            if state.policy.stopped().is_some() {
                return None;
            }
            state
                .next_batch(signal, now, MAX_BATCH_BYTES)
                .map(|(items, attempt)| (endpoint, items, attempt, state.version))
        })
        .flatten();
        let Some((endpoint, items, attempt, version)) = batch else {
            continue;
        };
        let body = otlp::envelope(signal, &items, version);
        let (status, retry_after) = post(&endpoint, signal, &token, body).await;
        with_state(|state| {
            let (verdict, transition) = state.policy.on_response(
                status,
                attempt,
                retry_after.as_deref(),
                platform::unit_random(),
            );
            warn(state.apply_verdict(
                signal,
                items,
                attempt,
                verdict,
                transition,
                platform::now_ms(),
            ));
        });
    }
}

/// The token for this export, fetched from the product when the cached one is
/// missing or near expiry.
async fn ensure_token(now: f64) -> Option<String> {
    if let Some(token) = with_state(|state| state.valid_token(now)).flatten() {
        return Some(token);
    }
    match fetch_token().await {
        Ok(fetched) => {
            let value = fetched.access_token.clone();
            with_state(|state| {
                state.token = Some(CachedToken {
                    value: fetched.access_token,
                    // Relative lifetime from the server, so a wrong browser
                    // clock cannot make us hold a dead token.
                    valid_until_ms: now + (fetched.expires_in as f64) * 1_000.0 - TOKEN_MARGIN_MS,
                });
            });
            Some(value)
        }
        Err(status) => {
            with_state(|state| {
                let transition = state.policy.on_token_failure(status);
                if state.policy.stopped().is_some() {
                    state.discard_everything();
                }
                warn(transition.map(|transition| state.describe(transition)));
            });
            None
        }
    }
}

/// `GET /api/telemetry/token`. `Err` carries the status, `0` for no answer.
///
/// Deliberately not through `crate::api`: it must not open a span of its own
/// (that would be telemetry about telemetry), and a `401` here must not
/// navigate the page to the login route — that is the app's decision, made by
/// its own requests.
async fn fetch_token() -> Result<shared::TelemetryToken, u16> {
    // The same 10 s deadline as an export: a backend that accepts the request
    // and never answers must not hold `exporting` — and with it every later
    // tick — for the browser's own multi-minute timeout. The body is covered
    // too, since the signal stays attached until `json()` completes.
    let controller = web_sys::AbortController::new().ok();
    let signal = controller.as_ref().map(web_sys::AbortController::signal);
    let _deadline = controller
        .map(|controller| gloo_timers::callback::Timeout::new(10_000, move || controller.abort()));
    let response = gloo_net::http::Request::get("/api/telemetry/token")
        .abort_signal(signal.as_ref())
        .send()
        .await
        .map_err(|_| 0u16)?;
    if !response.ok() {
        return Err(response.status());
    }
    response
        .json::<shared::TelemetryToken>()
        .await
        .map_err(|_| response.status())
}

/// POST one batch. Returns the status (`0` for no answer) and `Retry-After`.
async fn post(endpoint: &str, signal: Signal, token: &str, body: String) -> (u16, Option<String>) {
    let url = format!("{endpoint}{}", signal.path());
    // An abort after 10 s: a hung ingest must not keep `exporting` set — and
    // with it every later tick — for the browser's own multi-minute timeout.
    let controller = web_sys::AbortController::new().ok();
    let signal_handle = controller.as_ref().map(web_sys::AbortController::signal);
    let _deadline = controller
        .map(|controller| gloo_timers::callback::Timeout::new(10_000, move || controller.abort()));
    // A batch small enough goes out as a keepalive request too: the page can
    // be left while an ordinary export is in flight, and a plain `fetch` is
    // cancelled with the page — taking with it the items the tick had already
    // taken from the outbox, which the unload flush can then no longer see.
    // Larger batches cannot (keepalive bodies share a 64 KiB budget) and take
    // that small risk.
    let keepalive = body.len() <= ROUTINE_KEEPALIVE_BYTES;
    let Some(promise) = start_fetch(&url, &body, token, keepalive, signal_handle.as_ref()) else {
        return (0, None);
    };
    // `dyn_into` checks at run time that the resolved JS value really is a
    // `Response` before treating it as one.
    use wasm_bindgen::JsCast;
    match wasm_bindgen_futures::JsFuture::from(promise).await {
        Ok(value) => match value.dyn_into::<web_sys::Response>() {
            Ok(response) => (
                response.status(),
                response.headers().get("retry-after").ok().flatten(),
            ),
            Err(_) => (0, None),
        },
        // A rejected fetch: no HTTP answer at all.
        Err(_) => (0, None),
    }
}

/// Start one POST of an OTLP JSON body with the bearer, optionally as a
/// keepalive request. `None` if the browser refused to build it.
fn start_fetch(
    url: &str,
    body: &str,
    token: &str,
    keepalive: bool,
    abort: Option<&web_sys::AbortSignal>,
) -> Option<js_sys::Promise> {
    let window = web_sys::window()?;
    let headers = web_sys::Headers::new().ok()?;
    headers.set("Content-Type", "application/json").ok()?;
    headers
        .set("Authorization", &format!("Bearer {token}"))
        .ok()?;
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_headers(&headers);
    init.set_body(&wasm_bindgen::JsValue::from_str(body));
    // The ingest reads only the bearer; bored's session cookies have no
    // business reaching another container.
    init.set_credentials(web_sys::RequestCredentials::Omit);
    if let Some(abort) = abort {
        init.set_signal(Some(abort));
    }
    if keepalive {
        // `keepalive` has no typed setter in this web-sys version, so it is
        // set as a plain property on the init dictionary.
        let _ = js_sys::Reflect::set(
            &init,
            &wasm_bindgen::JsValue::from_str("keepalive"),
            &wasm_bindgen::JsValue::TRUE,
        );
    }
    Some(window.fetch_with_str_and_init(url, &init))
}

// ── The unload flush ─────────────────────────────────────────────────────

/// Send what is buffered now, without waiting for an answer, on requests the
/// browser keeps alive past page unload.
///
/// `fetch(…, { keepalive: true })` rather than `navigator.sendBeacon`: a beacon
/// cannot carry an `Authorization` header, and the ingest reads nothing else,
/// so every beacon would be `401 no token` (card #416, decision D3).
///
/// Only with a token already in hand: there is no time to fetch one while the
/// page is going away, and never before the session has proved itself.
pub fn flush_now() {
    let now = platform::now_ms();
    let requests = with_state(|state| {
        let Phase::Enabled { endpoint } = &state.phase else {
            return Vec::new();
        };
        let endpoint = endpoint.clone();
        if state.policy.stopped().is_some() {
            return Vec::new();
        }
        // The refresh margin is waived here: it exists so an ordinary export
        // never leaves with a token about to die, but on the way out there is
        // no chance to fetch a fresh one, and a token with seconds left still
        // gets through. Only a genuinely expired token is not sent.
        let Some(token) = state.valid_token(now - TOKEN_MARGIN_MS) else {
            return Vec::new();
        };
        let version = state.version;
        let mut requests = Vec::new();
        for signal in [Signal::Traces, Signal::Logs] {
            if let Some(items) = state.unload_batch(signal) {
                requests.push((
                    format!("{endpoint}{}", signal.path()),
                    otlp::envelope(signal, &items, version),
                ));
            }
        }
        requests
            .into_iter()
            .map(|(url, body)| (url, body, token.clone()))
            .collect()
    })
    .unwrap_or_default();
    for (url, body, token) in requests {
        send_keepalive(&url, &body, &token);
    }
}

/// Fire one keepalive POST and ignore the outcome.
fn send_keepalive(url: &str, body: &str, token: &str) {
    let Some(promise) = start_fetch(url, body, token, true, None) else {
        return;
    };
    // Await it off to the side, so a failure is swallowed here rather than
    // printed by the browser as an unhandled rejection.
    wasm_bindgen_futures::spawn_local(async move {
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    });
}

/// Flush when the page is hidden or going away. `visibilitychange` to
/// `hidden` is the reliable signal on mobile, where `pagehide` often never
/// fires; both are wired, and a flush of an empty outbox sends nothing.
fn install_unload_flush() {
    use wasm_bindgen::JsCast;
    let Some(window) = web_sys::window() else {
        return;
    };
    let on_pagehide = wasm_bindgen::closure::Closure::<dyn Fn()>::new(flush_now);
    let _ =
        window.add_event_listener_with_callback("pagehide", on_pagehide.as_ref().unchecked_ref());
    on_pagehide.forget();

    if let Some(document) = window.document() {
        let doc = document.clone();
        let on_visibility = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
            if doc.visibility_state() == web_sys::VisibilityState::Hidden {
                flush_now();
            }
        });
        let _ = document.add_event_listener_with_callback(
            "visibilitychange",
            on_visibility.as_ref().unchecked_ref(),
        );
        on_visibility.forget();
    }
}

#[cfg(test)]
pub mod test_support {
    //! Host-test access to the one `State`.
    use super::*;

    /// Put the state back to "just loaded", as at page start.
    pub fn reset() {
        STATE.with(|cell| *cell.borrow_mut() = State::new(platform::now_ms()));
    }

    /// Take everything buffered for `signal`, decoded back to JSON values.
    pub fn drain(signal: Signal) -> Vec<serde_json::Value> {
        with_state(|state| {
            state
                .outbox(signal)
                .take_batch(usize::MAX, |_| 0)
                .iter()
                .map(|item| serde_json::from_str(item).expect("outbox holds JSON"))
                .collect()
        })
        .unwrap_or_default()
    }

    pub fn phase() -> Phase {
        with_state(|state| state.phase.clone()).expect("state free")
    }

    pub fn dropped() -> u64 {
        with_state(|state| state.dropped()).expect("state free")
    }

    /// Drive a verdict through `apply_verdict` as the export loop would.
    pub fn respond(signal: Signal, status: u16, attempt: u32) -> Option<String> {
        with_state(|state| {
            let now = platform::now_ms();
            let items = state
                .next_batch(signal, now + 1e12, usize::MAX)
                .map(|(items, _)| items)
                .unwrap_or_default();
            let (verdict, transition) = state.policy.on_response(status, attempt, None, 0.0);
            state.apply_verdict(signal, items, attempt, verdict, transition, now)
        })
        .flatten()
    }

    pub fn set_token(value: &str, valid_until_ms: f64) {
        with_state(|state| {
            state.token = Some(CachedToken {
                value: value.to_string(),
                valid_until_ms,
            })
        });
    }

    pub fn token(now_ms: f64) -> Option<String> {
        with_state(|state| state.valid_token(now_ms)).flatten()
    }

    pub fn has_retry(signal: Signal) -> bool {
        with_state(|state| state.retry[signal_index(signal)].is_some()).unwrap_or(false)
    }
}
