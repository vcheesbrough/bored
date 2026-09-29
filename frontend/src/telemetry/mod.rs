//! The SPA's own traces and logs, sent over OTLP to the environment's ingest
//! (card #416).
//!
//! The rest of the frontend knows three things about this module: open a span
//! with [`start_span`] (passing its parent **explicitly**), report a failure
//! with [`error`], and nothing else. Everything below that — encoding, the
//! bounded outbox, the token, retries, the unload flush — is private to it.
//!
//! # The contract, and where each part lives
//!
//! The rules are the `observability` skill's `client-export.md`:
//!
//! | Rule | Where |
//! | --- | --- |
//! | OTLP/HTTP JSON, hex ids, string int64s, numeric enums | [`otlp`] |
//! | Bounded buffer, drop-oldest, counted | [`outbox`] |
//! | Retry only 429/502/503/504, jitter, `Retry-After`, one refresh on 401 | [`policy`] |
//! | No configuration → OTLP never initialised | [`config`] |
//! | 5 s tick, token read per export, unload flush, console transitions | [`runtime`] |
//!
//! # Why parents are passed explicitly
//!
//! Tracing libraries usually keep an ambient "current span" that new spans
//! pick up as their parent. In a single-threaded browser app, many async tasks
//! interleave on one thread: a stack of current spans would be pushed by one
//! task and popped by another, and requests would end up parented to whatever
//! unrelated work happened to be suspended at the time. So there is no such
//! stack here. A span's parent is whatever [`SpanContext`] the caller hands
//! over — a screen's load span, or nothing (the span is then a root: one user
//! action, one trace).
//!
//! # Telemetry must never harm the product
//!
//! Nothing here returns an error to its caller or can block one. A disabled,
//! failing or refusing ingest changes what reaches the console at warning
//! level, and nothing else.

mod config;
pub mod otlp;
mod outbox;
mod platform;
mod policy;
mod runtime;

pub use otlp::{SpanContext, SpanKind};

use otlp::{ErrorStatus, KeyValue, Link, LogRecord, Severity, Span, SpanId, TraceId, UnixNanos};

/// Start the export loop. Call once, first thing in `main`.
pub fn start() {
    runtime::start();
}

/// Hand the product's telemetry configuration (the `telemetry` block of
/// `/api/info`) to the exporter. Called on every heartbeat answer; the first
/// decides the session and a later "off" is honoured.
pub fn configure(config: Option<&shared::ClientTelemetryConfig>) {
    if let Some(Some(line)) = runtime::with_state(|state| state.configure(config)) {
        leptos::logging::warn!("{line}");
    }
}

/// A span in progress. Ends — and is buffered for export — when dropped, so
/// an early `return` or `?` in the caller can never leak an unfinished span.
///
/// Holds plain data only: no borrow of the exporter's state, no JS object.
#[must_use = "a span measures until it is dropped; bind it to a variable"]
pub struct ActiveSpan {
    context: SpanContext,
    parent: Option<SpanId>,
    name: &'static str,
    kind: SpanKind,
    start: UnixNanos,
    attributes: Vec<KeyValue>,
    links: Vec<Link>,
    status: Option<ErrorStatus>,
    /// Whether telemetry was on (or still undecided) when the span started.
    /// A span started while disabled is inert: no `traceparent`, no export.
    live: bool,
}

/// Open a span. `parent` is the span it belongs under, or `None` for a new
/// trace.
pub fn start_span(name: &'static str, kind: SpanKind, parent: Option<SpanContext>) -> ActiveSpan {
    let live = runtime::with_state(|state| state.recording()).unwrap_or(false);
    // The trace id is inherited from the parent; only a root makes a new one.
    let trace_id = parent.map_or_else(
        || TraceId::from_random(platform::random_bytes()),
        |parent| parent.trace_id,
    );
    ActiveSpan {
        context: SpanContext {
            trace_id,
            span_id: SpanId::from_random(platform::random_bytes()),
        },
        parent: parent.map(|parent| parent.span_id),
        name,
        kind,
        start: UnixNanos(platform::now_nanos()),
        attributes: Vec::new(),
        links: Vec::new(),
        status: None,
        live,
    }
}

impl ActiveSpan {
    /// This span's identity, to parent children with — or `None` when
    /// telemetry is off, so nothing downstream is parented to a span that will
    /// never be exported.
    pub fn context(&self) -> Option<SpanContext> {
        self.live.then_some(self.context)
    }

    /// The `traceparent` header for a request made inside this span, or `None`
    /// when telemetry is off — the server then starts its own trace, exactly
    /// as it did before this module existed.
    pub fn traceparent(&self) -> Option<String> {
        self.context().map(|context| context.traceparent())
    }

    /// Add an attribute. `key` must be one of [`otlp::keys`].
    pub fn set_attribute(&mut self, key: &'static str, value: impl Into<otlp::AnyValue>) {
        self.attributes.push(KeyValue::new(key, value));
    }

    /// Mark the span failed with a low-cardinality `error.type`. The first
    /// failure wins: a request that failed with `404` and then could not be
    /// decoded is a `404`.
    pub fn fail(&mut self, error_type: impl Into<String>) {
        if self.status.is_some() {
            return;
        }
        let error_type = error_type.into();
        self.attributes
            .push(KeyValue::new(otlp::keys::ERROR_TYPE, error_type.clone()));
        self.status = Some(ErrorStatus {
            message: error_type,
        });
    }

    /// Record a span this one relates to without being its child — the
    /// server's SSE stream span.
    pub fn add_link(&mut self, other: SpanContext) {
        self.links.push(Link {
            trace_id: other.trace_id,
            span_id: other.span_id,
        });
    }

    /// End the span now. Equivalent to dropping it; reads better at the call
    /// site.
    pub fn end(self) {}

    fn finish(&mut self) -> Span {
        Span {
            trace_id: self.context.trace_id,
            span_id: self.context.span_id,
            parent_span_id: self.parent,
            name: self.name,
            kind: self.kind,
            start_time_unix_nano: self.start,
            end_time_unix_nano: UnixNanos(platform::now_nanos()),
            // `mem::take` moves the vectors out, leaving empty ones behind;
            // the span is being destroyed, so nothing reads them again.
            attributes: std::mem::take(&mut self.attributes),
            links: std::mem::take(&mut self.links),
            status: self.status.take(),
        }
    }
}

impl Drop for ActiveSpan {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let span = self.finish();
        runtime::with_state(|state| state.record_span(&span));
    }
}

/// Remember the screen that just started loading, so a panic can be filed in
/// the same trace. Presentation state, not an ambient span: nothing is
/// parented to it implicitly.
pub fn set_current_screen(context: Option<SpanContext>) {
    runtime::with_state(|state| state.current_screen = context);
}

/// A failure the SPA can report: how to classify it for `error.type`, and
/// which request's span it came from, if any.
///
/// `Display` is what goes to the console, in full, as before this module. It
/// is **never** exported: an error's text can quote a server response or a
/// value the user typed, and logs must not carry user content.
pub trait Reportable: std::fmt::Display {
    /// A fixed, low-cardinality class: `network`, `decode`, `offline`, a
    /// status code…
    fn error_type(&self) -> String;
    /// The HTTP status, when the failure had one.
    fn status(&self) -> Option<u16> {
        None
    }
    /// The span of the request that failed, so the log line links to it.
    fn trace(&self) -> Option<SpanContext> {
        None
    }
}

/// Report a failure: the full message to the console (unchanged from the
/// `leptos::logging::error!` this replaces), and a log record to the ingest
/// carrying only `what`, the error's class and status, and its trace.
pub fn error(what: &'static str, error: &impl Reportable) {
    leptos::logging::error!("{what}: {error}");
    let mut attributes = vec![KeyValue::new(otlp::keys::ERROR_TYPE, error.error_type())];
    if let Some(status) = error.status() {
        attributes.push(KeyValue::new(otlp::keys::HTTP_RESPONSE_STATUS_CODE, status));
    }
    emit(Severity::Error, what, attributes, error.trace());
}

/// Report a failure that is not a request's: the console gets `what` and
/// `detail`; the ingest gets `what` and `error_type` only.
pub fn error_detail(what: &'static str, detail: &dyn std::fmt::Display, error_type: &'static str) {
    leptos::logging::error!("{what}: {detail}");
    emit(
        Severity::Error,
        what,
        vec![KeyValue::new(otlp::keys::ERROR_TYPE, error_type)],
        None,
    );
}

/// Buffer one log record.
fn emit(
    severity: Severity,
    body: &'static str,
    attributes: Vec<KeyValue>,
    context: Option<SpanContext>,
) {
    let record = LogRecord {
        time: UnixNanos(platform::now_nanos()),
        severity,
        body,
        attributes,
        context,
    };
    runtime::with_state(|state| state.record_log(&record));
}

/// Where in the source a panic happened: file, line, column. The panic's own
/// *message* is deliberately not part of it (see [`record_panic`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanicLocation<'a> {
    pub file: &'a str,
    pub line: u32,
    pub column: u32,
}

/// Record a panic and send it immediately, before the module traps.
///
/// Called from the panic hook, where the reactive runtime is already broken
/// and the module is about to hit `unreachable`. So: a zero-length `panic`
/// span (child of the current screen's load span, which puts the panic in a
/// trace with the screen it happened on), a log record inside it with
/// `exception.type=panic` and the source location, then a keepalive flush —
/// the one request that can still leave once the module has trapped.
///
/// **The panic message is not exported.** It is free text that can format a
/// value the user typed (`expect("…{title}…")`, a failed `assert_eq!` on a
/// card body), and every exported record is stamped with the user's identity
/// by the ingest. The location — a compiled-in source path and line — says
/// where to look; the message stays in the console for whoever reproduces it.
pub fn record_panic(location: Option<PanicLocation<'_>>) {
    let screen = runtime::with_state(|state| state.current_screen).flatten();
    let mut span = start_span("panic", SpanKind::Internal, screen);
    span.fail("panic");
    let mut attributes = vec![KeyValue::new(otlp::keys::EXCEPTION_TYPE, "panic")];
    if let Some(location) = location {
        attributes.push(KeyValue::new(otlp::keys::CODE_FILE_PATH, location.file));
        attributes.push(KeyValue::new(otlp::keys::CODE_LINE_NUMBER, location.line));
        attributes.push(KeyValue::new(
            otlp::keys::CODE_COLUMN_NUMBER,
            location.column,
        ));
    }
    let context = span.context();
    emit(Severity::Error, "wasm panic", attributes, context);
    span.end();
    runtime::flush_now();
}

#[cfg(test)]
mod tests;
