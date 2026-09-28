//! Every metric bored exports: the instruments, created once, and the typed
//! handles product code records through.
//!
//! This file uses only the `opentelemetry` **API** crate (the façade — no SDK
//! type appears here), so it could live anywhere; it sits beside the telemetry
//! module because the instruments are created there, after the meter provider
//! exists.
//!
//! ## Rules this file enforces by construction
//!
//! - **Every attribute value comes from an exhaustive `match` over an enum**
//!   (skill §4). Adding a variant fails to compile until it is given a label,
//!   so a label can never carry a value nobody predicted. The one numeric
//!   label, `http.response.status_code`, is bounded by the HTTP status space
//!   and set by our own handlers, never by the client.
//! - **No identifier is ever a label.** Board, card, user and session ids, and
//!   raw request paths, belong on span attributes where cardinality is free.
//!   `http.route` is the router's *template* (`/api/cards/:id`), never the
//!   path. The forbidden-label test in `observability::tests` checks every
//!   exported series by key.
//! - **Build identity is not a label on any working metric** — it appears on
//!   `bored.build.info` only, and queries join against it.
//!
//! ## Why a `OnceLock` and not a `lazy_static` per instrument
//!
//! OpenTelemetry's global meter is **not** a proxy: an instrument created
//! before the meter provider is installed stays bound to the no-op provider
//! forever. So instruments are created in one place, by
//! [`install`], after `observability::init` has built the provider. Until
//! then — and for the whole life of a process with telemetry off — every
//! recording function below is a cheap no-op.

use std::sync::OnceLock;

use axum::http::Method;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, ObservableGauge, UpDownCounter};
use opentelemetry_semantic_conventions::attribute::{
    ERROR_TYPE, HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE,
};
use opentelemetry_semantic_conventions::metric::HTTP_SERVER_REQUEST_DURATION;

/// Product-specific attribute key for [`AuthOutcome`]. `bored.`-prefixed
/// because semconv has no name for it.
pub(crate) const BORED_AUTH_OUTCOME: &str = "bored.auth.outcome";

/// The build-info metric's two attributes. Plain keys (not `bored.`-prefixed,
/// not semconv) because they are the estate's `<app>_build_info` convention
/// (`mini-config/monitoring-stack/OBSERVABILITY.md`), shared with every product
/// so one join works everywhere. They appear on this metric and no other.
pub(crate) const BUILD_INFO_VERSION: &str = "version";
pub(crate) const BUILD_INFO_REVISION: &str = "revision";

/// Instrument names, as constants so tests and the (future, #437) dashboard
/// check can refer to them without re-typing.
pub(crate) const SSE_SUBSCRIBERS: &str = "bored.sse.subscribers";
pub(crate) const SSE_EVENTS: &str = "bored.sse.events";
pub(crate) const SSE_LAGGED: &str = "bored.sse.lagged";
pub(crate) const DB_ERRORS: &str = "bored.db.errors";
pub(crate) const AUTH_OUTCOMES: &str = "bored.auth.outcomes";
pub(crate) const BUILD_INFO: &str = "bored.build.info";

/// The semantic-convention bucket boundaries for
/// `http.server.request.duration`, in **seconds**. The SDK's default
/// boundaries are sized for milliseconds (0, 5, 10, 25, …), so a seconds
/// histogram on them would put every request in the first bucket.
const HTTP_DURATION_BOUNDARIES: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Every instrument, created once from one meter.
///
/// Fields are private; product code records through the free functions below,
/// which take typed arguments (enums, not strings), so a call site cannot
/// invent a label value.
pub(crate) struct Instruments {
    http_server_request_duration: Histogram<f64>,
    sse_subscribers: UpDownCounter<i64>,
    sse_events: Counter<u64>,
    sse_lagged: Counter<u64>,
    db_errors: Counter<u64>,
    auth_outcomes: Counter<u64>,
    /// Never read: an observable gauge reports through its callback, and is
    /// kept here only so it lives as long as the other instruments.
    _build_info: ObservableGauge<u64>,
}

impl Instruments {
    /// Create every instrument on `meter`. `revision` is the git commit the
    /// binary was built from (see [`build_revision`]).
    pub(crate) fn new(meter: &Meter) -> Self {
        // Each builder call below is `meter.<kind>(name)` → optional unit and
        // description → `.build()`. The unit uses UCUM notation, which is what
        // the collector's Prometheus translation reads to add `_seconds`.
        let http_server_request_duration = meter
            .f64_histogram(HTTP_SERVER_REQUEST_DURATION)
            .with_unit("s")
            .with_description("Duration of HTTP server requests.")
            .with_boundaries(HTTP_DURATION_BOUNDARIES.to_vec())
            .build();
        let sse_subscribers = meter
            .i64_up_down_counter(SSE_SUBSCRIBERS)
            .with_unit("{subscriber}")
            .with_description("Open SSE event streams.")
            .build();
        let sse_events = meter
            .u64_counter(SSE_EVENTS)
            .with_unit("{event}")
            .with_description("Board events delivered to SSE subscribers.")
            .build();
        let sse_lagged = meter
            .u64_counter(SSE_LAGGED)
            .with_unit("{event}")
            .with_description(
                "Board events dropped for an SSE subscriber that fell more than the \
                 broadcast capacity behind — the event channel's saturation signal.",
            )
            .build();
        let db_errors = meter
            .u64_counter(DB_ERRORS)
            .with_unit("{error}")
            .with_description("Database calls that returned an error, by class.")
            .build();
        let auth_outcomes = meter
            .u64_counter(AUTH_OUTCOMES)
            .with_unit("{request}")
            .with_description("Authentication decisions on protected requests, by outcome.")
            .build();

        // The build-info gauge's attributes are computed once and moved into
        // the callback (`move`), which the SDK calls at every collection.
        let build_attributes = [
            KeyValue::new(BUILD_INFO_VERSION, shared::app_version()),
            KeyValue::new(BUILD_INFO_REVISION, build_revision()),
        ];
        let build_info = meter
            .u64_observable_gauge(BUILD_INFO)
            .with_description("Always 1; carries the build's version and revision for joins.")
            .with_callback(move |observer| observer.observe(1, &build_attributes))
            .build();

        Self {
            http_server_request_duration,
            sse_subscribers,
            sse_events,
            sse_lagged,
            db_errors,
            auth_outcomes,
            _build_info: build_info,
        }
    }

    /// Record one finished HTTP request. `route` is the matched route
    /// *template*, or `None` for a request no route matched (the SPA
    /// fallback) — semconv omits the attribute rather than inventing a value.
    pub(crate) fn record_http_request(
        &self,
        method: HttpMethod,
        route: Option<&str>,
        status: u16,
        error: Option<ErrorType>,
        seconds: f64,
    ) {
        // A small Vec rather than an array because two attributes are optional.
        let mut attributes = Vec::with_capacity(4);
        attributes.push(KeyValue::new(HTTP_REQUEST_METHOD, method.label()));
        if let Some(route) = route {
            attributes.push(KeyValue::new(HTTP_ROUTE, route.to_string()));
        }
        // semconv types the status code as an int.
        attributes.push(KeyValue::new(HTTP_RESPONSE_STATUS_CODE, i64::from(status)));
        if let Some(error) = error {
            attributes.push(KeyValue::new(ERROR_TYPE, error.label()));
        }
        self.http_server_request_duration
            .record(seconds, &attributes);
    }
}

/// The process-wide instruments, set once by [`install`].
static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

/// Create the instruments on `meter`. Called once by `observability::init`
/// after the meter provider is installed; a second call is ignored (the first
/// set of instruments stays), which keeps `init` idempotent.
pub(crate) fn install(meter: &Meter) {
    // `get_or_init` runs the closure only if nothing is stored yet.
    INSTRUMENTS.get_or_init(|| Instruments::new(meter));
}

/// The installed instruments, if any.
///
/// Under `cfg(test)` the first call installs instruments on the test meter
/// provider (`observability::test_support`), so every test in the binary —
/// router tests included — records into one in-memory exporter the metric
/// tests can read. In a real process nothing is installed until `init` runs,
/// and nothing at all with telemetry off.
fn instruments() -> Option<&'static Instruments> {
    #[cfg(test)]
    {
        Some(INSTRUMENTS.get_or_init(|| Instruments::new(&super::test_support::meter())))
    }
    #[cfg(not(test))]
    {
        INSTRUMENTS.get()
    }
}

/// Make sure the test instruments exist, so a collection includes every
/// instrument (the observable build-info gauge in particular) even if no test
/// has recorded anything yet.
#[cfg(test)]
pub(crate) fn ensure_installed() {
    let _ = instruments();
}

// ── Typed handles for product code ──────────────────────────────────────────
// Each is a free function so a call site reads `metrics::sse_lagged(n)` and
// needs no handle in scope. All are no-ops while no instruments are installed.

/// One HTTP request finished; see [`Instruments::record_http_request`].
pub(crate) fn http_request(
    method: HttpMethod,
    route: Option<&str>,
    status: u16,
    error: Option<ErrorType>,
    seconds: f64,
) {
    if let Some(instruments) = instruments() {
        instruments.record_http_request(method, route, status, error, seconds);
    }
}

/// An SSE stream opened (`+1`) or closed (`-1`). Use [`SseSubscription`]
/// rather than calling this directly, so the decrement cannot be forgotten.
fn sse_subscribers_add(delta: i64) {
    if let Some(instruments) = instruments() {
        instruments.sse_subscribers.add(delta, &[]);
    }
}

/// One board event was delivered to one SSE subscriber.
pub(crate) fn sse_event_delivered() {
    if let Some(instruments) = instruments() {
        instruments.sse_events.add(1, &[]);
    }
}

/// `dropped` events were skipped for a subscriber that lagged behind.
pub(crate) fn sse_lagged(dropped: u64) {
    if let Some(instruments) = instruments() {
        instruments.sse_lagged.add(dropped, &[]);
    }
}

/// A database call failed, classified by [`DbErrorClass`].
pub(crate) fn db_error(class: DbErrorClass) {
    if let Some(instruments) = instruments() {
        instruments
            .db_errors
            .add(1, &[KeyValue::new(ERROR_TYPE, class.label())]);
    }
}

/// One authentication decision on a protected request.
pub(crate) fn auth_outcome(outcome: AuthOutcome) {
    if let Some(instruments) = instruments() {
        instruments
            .auth_outcomes
            .add(1, &[KeyValue::new(BORED_AUTH_OUTCOME, outcome.label())]);
    }
}

/// Counts one open SSE stream for as long as it lives.
///
/// An RAII guard: `new` adds one to `bored.sse.subscribers`, and `Drop` takes
/// it away again — whether the client disconnected, the server shut down, or
/// the stream was dropped for any other reason. That makes the up-down counter
/// impossible to leak by an early return.
pub(crate) struct SseSubscription(());

impl SseSubscription {
    pub(crate) fn new() -> Self {
        sse_subscribers_add(1);
        SseSubscription(())
    }
}

impl Drop for SseSubscription {
    fn drop(&mut self) {
        sse_subscribers_add(-1);
    }
}

/// The git commit this binary was built from.
///
/// `GIT_REVISION` is passed as a build argument by `.woodpecker/build.yml`
/// (`CI_COMMIT_SHA`) and read at **compile** time with `option_env!`, so the
/// binary carries it and no deployment can mis-state it. A local build has
/// none and reports `unknown`.
pub(crate) fn build_revision() -> &'static str {
    match option_env!("GIT_REVISION") {
        Some(revision) if !revision.is_empty() => revision,
        _ => "unknown",
    }
}

// ── Label enums ─────────────────────────────────────────────────────────────
// Each has `ALL` (every variant, so tests iterate instead of listing cases)
// and `label()` (an exhaustive match — the only place a label value exists).

/// The request method, as semconv's `http.request.method` wants it: one of the
/// known methods, or `_OTHER` for anything else. A client can send any token
/// as a method; without the `_OTHER` fold that would be an unbounded label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Connect,
    Options,
    Trace,
    Patch,
    Other,
}

impl HttpMethod {
    pub(crate) const ALL: [HttpMethod; 10] = [
        HttpMethod::Get,
        HttpMethod::Head,
        HttpMethod::Post,
        HttpMethod::Put,
        HttpMethod::Delete,
        HttpMethod::Connect,
        HttpMethod::Options,
        HttpMethod::Trace,
        HttpMethod::Patch,
        HttpMethod::Other,
    ];

    /// Classify a request's method. `Method` is a struct with associated
    /// constants rather than an enum, so this is an `if` chain, with every
    /// unknown method landing in `Other`.
    pub(crate) fn from_method(method: &Method) -> Self {
        match *method {
            Method::GET => HttpMethod::Get,
            Method::HEAD => HttpMethod::Head,
            Method::POST => HttpMethod::Post,
            Method::PUT => HttpMethod::Put,
            Method::DELETE => HttpMethod::Delete,
            Method::CONNECT => HttpMethod::Connect,
            Method::OPTIONS => HttpMethod::Options,
            Method::TRACE => HttpMethod::Trace,
            Method::PATCH => HttpMethod::Patch,
            _ => HttpMethod::Other,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Head => "HEAD",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Connect => "CONNECT",
            HttpMethod::Options => "OPTIONS",
            HttpMethod::Trace => "TRACE",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Other => "_OTHER",
        }
    }
}

/// `error.type` on a failed request: what went wrong, as a class.
///
/// A server-side failure (5xx) is labelled by its status code, which is what
/// semconv prescribes when nothing more specific is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorType {
    /// 500 — an internal failure.
    InternalServerError,
    /// 502/503/504 or any other 5xx.
    OtherServerError,
}

impl ErrorType {
    pub(crate) const ALL: [ErrorType; 2] =
        [ErrorType::InternalServerError, ErrorType::OtherServerError];

    /// The error class for a response status, or `None` when the response is
    /// not a server error. 4xx responses are the client's, not errors of this
    /// service, and carry no `error.type` on a server span (semconv).
    pub(crate) fn from_status(status: u16) -> Option<Self> {
        match status {
            500 => Some(ErrorType::InternalServerError),
            501..=599 => Some(ErrorType::OtherServerError),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            ErrorType::InternalServerError => "500",
            ErrorType::OtherServerError => "5xx",
        }
    }
}

/// Why a database call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbErrorClass {
    /// A unique index rejected the write — a client conflict (409), counted
    /// because a spike of them is still worth seeing.
    UniqueViolation,
    /// Anything else: a broken query, a deserialisation mismatch, a storage
    /// failure — a 500.
    Internal,
}

impl DbErrorClass {
    pub(crate) const ALL: [DbErrorClass; 2] =
        [DbErrorClass::UniqueViolation, DbErrorClass::Internal];

    pub(crate) fn label(self) -> &'static str {
        match self {
            DbErrorClass::UniqueViolation => "unique_violation",
            DbErrorClass::Internal => "internal",
        }
    }
}

/// The result of authenticating one protected request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthOutcome {
    /// A valid `Authorization: Bearer` token (MCP, scripts).
    BearerAccepted,
    /// A bearer token that failed validation.
    BearerRejected,
    /// A valid browser access cookie, no refresh needed.
    CookieAccepted,
    /// A browser access cookie that failed validation.
    CookieRejected,
    /// The browser session was refreshed with its refresh token.
    Refreshed,
    /// The refresh attempt failed.
    RefreshFailed,
    /// No usable credential at all.
    Missing,
    /// The refresh chain was invalidated by a logout.
    Invalidated,
    /// Auth is disabled (local dev); the anonymous claim was injected.
    Anonymous,
}

impl AuthOutcome {
    pub(crate) const ALL: [AuthOutcome; 9] = [
        AuthOutcome::BearerAccepted,
        AuthOutcome::BearerRejected,
        AuthOutcome::CookieAccepted,
        AuthOutcome::CookieRejected,
        AuthOutcome::Refreshed,
        AuthOutcome::RefreshFailed,
        AuthOutcome::Missing,
        AuthOutcome::Invalidated,
        AuthOutcome::Anonymous,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            AuthOutcome::BearerAccepted => "bearer_accepted",
            AuthOutcome::BearerRejected => "bearer_rejected",
            AuthOutcome::CookieAccepted => "cookie_accepted",
            AuthOutcome::CookieRejected => "cookie_rejected",
            AuthOutcome::Refreshed => "refreshed",
            AuthOutcome::RefreshFailed => "refresh_failed",
            AuthOutcome::Missing => "missing",
            AuthOutcome::Invalidated => "invalidated",
            AuthOutcome::Anonymous => "anonymous",
        }
    }
}
