//! The OTLP/HTTP **JSON** encoding of the two signals the SPA exports: spans
//! and log records (card #416).
//!
//! Pure data and `serde` — no browser API is touched in this file, which is
//! what lets `cargo test -p frontend` exercise it on the host target.
//!
//! # Why hand-written rather than an OpenTelemetry SDK
//!
//! The Rust SDK's exporters need tonic or a Tokio runtime and do not build for
//! `wasm32-unknown-unknown`; the JavaScript SDK would cost bundle size and a
//! `wasm-bindgen` shim for every call. OTLP defines a JSON encoding, the ingest
//! (`otlp-collector-oidc`) accepts it, and `serde_json` is already in the
//! bundle — so the whole encoder is the types below. The shape is ported from
//! v-note's browser exporter; everything around it follows the
//! `observability` skill's `client-export.md` instead.
//!
//! # Three rules that are easy to get wrong
//!
//! Each produces a request the collector rejects or, worse, silently misreads:
//!
//! - trace and span ids are **lowercase hex**, not the base64 that protobuf's
//!   canonical JSON mapping would give a `bytes` field;
//! - 64-bit integers — every timestamp, and `intValue` — are **strings**;
//! - enums (`kind`, `status.code`, `severityNumber`) are **numbers**.

use serde::{Serialize, Serializer};

/// Attribute keys, kept in one place so no call site types a key as a string
/// literal. Every key is either an OpenTelemetry semantic-convention name or
/// carries the product prefix `bored.` (skill §4); the unit test
/// `every_attribute_key_is_semconv_or_prefixed` enforces that.
pub mod keys {
    /// Resource: which service the telemetry belongs to. The ingest bounds it
    /// against `ALLOWED_SERVICE_NAMES` but otherwise keeps what we send.
    pub const SERVICE_NAME: &str = "service.name";
    /// Resource: the build — the bundle's `RELEASE_TAG`.
    pub const SERVICE_VERSION: &str = "service.version";
    pub const TELEMETRY_SDK_NAME: &str = "telemetry.sdk.name";
    pub const TELEMETRY_SDK_LANGUAGE: &str = "telemetry.sdk.language";
    pub const TELEMETRY_SDK_VERSION: &str = "telemetry.sdk.version";
    /// `GET`, `POST`, … on an `http.client` span.
    pub const HTTP_REQUEST_METHOD: &str = "http.request.method";
    /// The route *template* (`/api/cards/{id}`), never the concrete URL: ids
    /// belong in spans, but a template keeps span names and any derived
    /// metric bounded.
    pub const URL_TEMPLATE: &str = "url.template";
    pub const HTTP_RESPONSE_STATUS_CODE: &str = "http.response.status_code";
    /// A low-cardinality failure class (`network`, `decode`, `offline`, a
    /// status code…). Never a message.
    pub const ERROR_TYPE: &str = "error.type";
    /// `panic` on the panic hook's log record.
    pub const EXCEPTION_TYPE: &str = "exception.type";
    /// The panic message — the one piece of free text the SPA ever exports,
    /// accepted on the card as the exception to "no user content".
    pub const EXCEPTION_MESSAGE: &str = "exception.message";
    /// Which screen a screen-load span loaded (`board`, `home`).
    pub const BORED_SCREEN: &str = "bored.screen";

    /// Every key above, for the naming test.
    #[cfg(test)]
    pub const ALL: &[&str] = &[
        SERVICE_NAME,
        SERVICE_VERSION,
        TELEMETRY_SDK_NAME,
        TELEMETRY_SDK_LANGUAGE,
        TELEMETRY_SDK_VERSION,
        HTTP_REQUEST_METHOD,
        URL_TEMPLATE,
        HTTP_RESPONSE_STATUS_CODE,
        ERROR_TYPE,
        EXCEPTION_TYPE,
        EXCEPTION_MESSAGE,
        BORED_SCREEN,
    ];
}

/// The `service.name` this SPA states for itself. The ingest's
/// `ALLOWED_SERVICE_NAMES` must match it exactly, or every span and record is
/// dropped after a `200` (the collector counts it; the client is never told).
pub const SERVICE_NAME: &str = "bored-spa";

/// The instrumentation scope named on every batch.
const SCOPE_NAME: &str = "bored-spa";

/// A W3C trace id: 16 random bytes.
///
/// A newtype (a one-field tuple struct) rather than a bare `[u8; 16]` so a
/// trace id can never be passed where a span id is expected — the compiler
/// keeps them apart. `Copy` because it is 16 bytes: cheaper to copy than to
/// borrow and track a lifetime for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId([u8; 16]);

/// A W3C span id: 8 random bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId([u8; 8]);

impl TraceId {
    /// Build from random bytes. All-zero is reserved by the W3C spec as
    /// "invalid", and a collector drops a span carrying it, so the one
    /// constructor repairs that (vanishingly unlikely) case rather than trust
    /// every caller to.
    pub fn from_random(mut bytes: [u8; 16]) -> Self {
        if bytes == [0; 16] {
            bytes[15] = 1;
        }
        Self(bytes)
    }

    /// Parse 32 lowercase-or-uppercase hex digits — how the server hands its
    /// SSE trace id over. `None` for anything else, including all-zero.
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes: [u8; 16] = parse_hex(text)?;
        (bytes != [0; 16]).then_some(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        hex(&self.0)
    }
}

impl SpanId {
    /// As [`TraceId::from_random`]: all-zero is invalid, so it is repaired.
    pub fn from_random(mut bytes: [u8; 8]) -> Self {
        if bytes == [0; 8] {
            bytes[7] = 1;
        }
        Self(bytes)
    }

    /// Parse 16 hex digits; `None` for anything else, including all-zero.
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes: [u8; 8] = parse_hex(text)?;
        (bytes != [0; 8]).then_some(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        hex(&self.0)
    }
}

/// Lowercase hex, two digits per byte.
fn hex(bytes: &[u8]) -> String {
    // `Write as _` brings the `write!` target trait into scope without
    // binding its name, which we never use directly.
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            // Writing into a `String` cannot fail, so the `Result` is dropped.
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Parse exactly `N * 2` hex digits into `N` bytes. The `const N: usize`
/// generic lets one function serve both id widths, with the array length
/// checked at compile time at each call site.
fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        // Slicing by byte index is safe because we checked `is_ascii` above:
        // every char is exactly one byte.
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// `Serialize` by hand so an id goes on the wire as its hex string.
impl Serialize for TraceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl Serialize for SpanId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

/// The identity of one span: which trace, and which span within it. This is
/// what is passed around *explicitly* to parent a new span — the SPA keeps no
/// ambient "current span" (see `telemetry/mod.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanContext {
    pub trace_id: TraceId,
    pub span_id: SpanId,
}

impl SpanContext {
    /// The `traceparent` header for a request made inside this span.
    ///
    /// Always flagged sampled (`-01`). The SPA does not head-sample, and must
    /// not start to without care: Traefik's tracer and the server's sampler are
    /// parent-based, so a `-00` here would not merely drop the browser's span —
    /// it would switch off Traefik's and the server's spans for that request.
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", self.trace_id.to_hex(), self.span_id.to_hex())
    }
}

/// Read the server's `trace` SSE event — `{"trace_id":"<32 hex>","span_id":"<16
/// hex>"}`, naming the server's span for this stream — into a context to link
/// to. `None` for anything malformed: a link is a nicety, never worth an error.
pub fn parse_stream_trace(data: &str) -> Option<SpanContext> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    Some(SpanContext {
        trace_id: TraceId::from_hex(value.get("trace_id")?.as_str()?)?,
        span_id: SpanId::from_hex(value.get("span_id")?.as_str()?)?,
    })
}

/// Nanoseconds since the Unix epoch. A string on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixNanos(pub u64);

impl Serialize for UnixNanos {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // `collect_str` formats via `Display` straight into the output, which
        // turns the number into a JSON string without an intermediate
        // allocation.
        serializer.collect_str(&self.0)
    }
}

/// One attribute. `key` is `&'static str` so only a compile-time constant
/// (from [`keys`]) can be used — never a string built from runtime data.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyValue {
    pub key: &'static str,
    pub value: AnyValue,
}

impl KeyValue {
    pub fn new(key: &'static str, value: impl Into<AnyValue>) -> Self {
        Self {
            key,
            value: value.into(),
        }
    }
}

/// OTLP's `AnyValue`. serde's default "externally tagged" enum form is exactly
/// OTLP's JSON shape: `{"stringValue":"…"}`, `{"intValue":"42"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum AnyValue {
    #[serde(rename = "stringValue")]
    String(String),
    /// 64-bit, so a string on the wire (see the module doc).
    #[serde(rename = "intValue", serialize_with = "int_as_string")]
    Int(i64),
    #[serde(rename = "boolValue")]
    Bool(bool),
}

fn int_as_string<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

// `From` impls let call sites write `KeyValue::new(key, 404u16)` and have the
// right variant chosen by the argument's type.
impl From<&str> for AnyValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<String> for AnyValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<u16> for AnyValue {
    fn from(value: u16) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<bool> for AnyValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

/// A span's kind. Only the two the SPA produces exist, so a match over this
/// enum is exhaustive and adding a kind forces its wire number to be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// Work inside the tab: a screen load, the SSE connect.
    Internal,
    /// An outbound request: the span a server's span is the child of.
    Client,
}

impl Serialize for SpanKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // OTLP's `SpanKind` enum: 1 = INTERNAL, 3 = CLIENT.
        serializer.serialize_u8(match self {
            Self::Internal => 1,
            Self::Client => 3,
        })
    }
}

/// A span's error status. Only ever serialized for a failure: an unset status
/// is the absence of the field, which is what "fine" looks like to every
/// backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorStatus {
    /// A low-cardinality class — the same value as `error.type` (`network`,
    /// `decode`, a status code) — never a message that could carry user
    /// content. A `String` only because a status code is formatted at runtime.
    pub message: String,
}

impl Serialize for ErrorStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            // `STATUS_CODE_ERROR` is 2.
            code: u8,
            message: &'a str,
        }
        Wire {
            code: 2,
            message: &self.message,
        }
        .serialize(serializer)
    }
}

/// A span link: "this span is related to that one" without being its child.
/// Used for the SSE stream, whose server span cannot be parented from the
/// browser (an `EventSource` cannot send `traceparent`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub trace_id: TraceId,
    pub span_id: SpanId,
}

/// A finished span, ready to encode.
///
/// `#[serde(rename_all = "camelCase")]` maps `trace_id` → `traceId` and so on,
/// which is OTLP JSON's field naming.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    /// Absent for a root span — the field is omitted, not sent as `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<SpanId>,
    pub name: &'static str,
    pub kind: SpanKind,
    pub start_time_unix_nano: UnixNanos,
    pub end_time_unix_nano: UnixNanos,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ErrorStatus>,
}

/// The levels the SPA exports logs at, with OTLP's `severityNumber` for each
/// (the first number of the level's range, as the SDKs emit). Only `Error`
/// today: the exported records are the migrated `error!` sites and the panic.
/// A new level is a new variant, and the exhaustive `match`es below then refuse
/// to build until it is given its number and text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
}

impl Severity {
    fn number(self) -> u8 {
        match self {
            Self::Error => 17,
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
        }
    }
}

/// A log record, ready to encode.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    pub time: UnixNanos,
    pub severity: Severity,
    /// A fixed message chosen by the code ("failed to fetch columns"), never
    /// text derived from what the user typed.
    pub body: &'static str,
    pub attributes: Vec<KeyValue>,
    /// The span the record belongs to — what lets Loki's line jump to Tempo.
    pub context: Option<SpanContext>,
}

impl Serialize for LogRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // A private "wire" struct with serde's derive does the field naming
        // and omission; the outer type keeps a friendlier shape for callers.
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire<'a> {
            time_unix_nano: UnixNanos,
            severity_number: u8,
            severity_text: &'static str,
            body: AnyValue,
            #[serde(skip_serializing_if = "<[KeyValue]>::is_empty")]
            attributes: &'a [KeyValue],
            #[serde(skip_serializing_if = "Option::is_none")]
            trace_id: Option<TraceId>,
            #[serde(skip_serializing_if = "Option::is_none")]
            span_id: Option<SpanId>,
        }
        Wire {
            time_unix_nano: self.time,
            severity_number: self.severity.number(),
            severity_text: self.severity.text(),
            body: AnyValue::from(self.body),
            attributes: &self.attributes,
            trace_id: self.context.map(|context| context.trace_id),
            span_id: self.context.map(|context| context.span_id),
        }
        .serialize(serializer)
    }
}

/// What the SPA says about itself on every batch.
///
/// It states its own `service.name` and `service.version` — the ingest bounds
/// the name and keeps the version (`client-ingest.md`). It deliberately sets
/// **no** `deployment.environment.name`, `telemetry_source`, `user.*` or
/// `session.id`: the ingest stamps the first three from its own configuration
/// and the token, overwriting whatever a client claims, and nothing stamps a
/// session id on this estate.
pub fn resource(version: &str) -> serde_json::Value {
    serde_json::json!({
        "attributes": [
            KeyValue::new(keys::SERVICE_NAME, SERVICE_NAME),
            KeyValue::new(keys::SERVICE_VERSION, version),
            KeyValue::new(keys::TELEMETRY_SDK_NAME, "bored-spa-otlp"),
            KeyValue::new(keys::TELEMETRY_SDK_LANGUAGE, "rust"),
            KeyValue::new(keys::TELEMETRY_SDK_VERSION, version),
        ]
    })
}

/// Which of the two OTLP signals a batch belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Logs,
}

impl Signal {
    /// OTLP/HTTP's path for the signal, appended to the configured endpoint.
    pub fn path(self) -> &'static str {
        match self {
            Self::Traces => "/v1/traces",
            Self::Logs => "/v1/logs",
        }
    }
}

/// Wrap already-encoded items (each one span or log record as JSON text) in the
/// signal's `Export…ServiceRequest` envelope.
///
/// Items are encoded once, when they enter the outbox, so the outbox can
/// account for their exact size; the envelope is then assembled by string
/// concatenation rather than by re-serializing everything.
pub fn envelope(signal: Signal, items: &[String], version: &str) -> String {
    let (outer, inner, list) = match signal {
        Signal::Traces => ("resourceSpans", "scopeSpans", "spans"),
        Signal::Logs => ("resourceLogs", "scopeLogs", "logRecords"),
    };
    let scope = serde_json::json!({ "name": SCOPE_NAME, "version": version });
    format!(
        r#"{{"{outer}":[{{"resource":{resource},"{inner}":[{{"scope":{scope},"{list}":[{items}]}}]}}]}}"#,
        resource = resource(version),
        items = items.join(","),
    )
}

/// The bytes the envelope adds around its items, for batch sizing: the
/// envelope of an empty list plus one comma per item after the first.
pub fn envelope_overhead(signal: Signal, version: &str, item_count: usize) -> usize {
    envelope(signal, &[], version).len() + item_count.saturating_sub(1)
}

#[cfg(test)]
mod tests;
