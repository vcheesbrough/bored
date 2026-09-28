//! The `OTEL_*` variables: read, validated as a set, and turned into a
//! decision — telemetry off, or on with exactly these signals and this
//! identity.
//!
//! This is pure: it takes the variables as a list of `(name, value)` pairs and
//! never touches the process environment, so every rule below is unit-tested
//! without `std::env::set_var` (which is `unsafe` in edition 2024 and races
//! other tests). `observability::init` feeds it `std::env::vars_os()`.
//!
//! **Why validate at all?** The Rust SDK reads some of these variables itself
//! and silently ignores, or falls back from, a value it does not understand —
//! an unknown sampler becomes "always on", a missing endpoint becomes
//! `http://localhost:4318`. Either would leave a deployment that *looks*
//! configured and exports nowhere. The observability contract (skill §2, §5)
//! turns every such case into a startup failure the deploy sees:
//!
//! - no `OTEL_*` variable at all → telemetry **off**, nothing validated;
//! - `OTEL_SDK_DISABLED=true` → **off**, the explicit silence;
//! - anything else → the whole set is checked, and one bad value fails startup.
//!
//! An *unreachable* collector is deliberately **not** checked here: that is a
//! runtime condition, and telemetry must never be what stops the product from
//! serving (skill §6).

use std::collections::BTreeMap;

/// The prefix every variable of the family shares. Presence of *any* variable
/// with it is what switches validation on.
const PREFIX: &str = "OTEL_";

/// The one transport this build carries. `grpc` and `http/json` are valid OTLP
/// protocols, but the binary is compiled with the http/protobuf client only
/// (no tonic stack), so accepting them would mean silently speaking something
/// other than what the deployment asked for.
pub(crate) const HTTP_PROTOBUF: &str = "http/protobuf";

/// The sampler names the SDK implements (`opentelemetry_sdk::trace::Config`'s
/// environment reader). Anything else it would replace with
/// `parentbased_always_on` and a warning nobody reads.
const SAMPLERS: &[&str] = &[
    "always_on",
    "always_off",
    "traceidratio",
    "parentbased_always_on",
    "parentbased_always_off",
    "parentbased_traceidratio",
];

/// The integer-valued tuning knobs the SDK reads for batch processors, the
/// metric reader and the exporter timeout. Validated only for shape (a
/// non-negative integer): their *values* are left to the deployment, which by
/// the contract leaves them at the defaults until a measurement says otherwise.
const INTEGER_VARIABLES: &[&str] = &[
    "OTEL_BSP_MAX_QUEUE_SIZE",
    "OTEL_BSP_SCHEDULE_DELAY",
    "OTEL_BSP_EXPORT_TIMEOUT",
    "OTEL_BSP_MAX_EXPORT_BATCH_SIZE",
    "OTEL_BLRP_MAX_QUEUE_SIZE",
    "OTEL_BLRP_SCHEDULE_DELAY",
    "OTEL_BLRP_EXPORT_TIMEOUT",
    "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE",
    "OTEL_METRIC_EXPORT_INTERVAL",
    "OTEL_METRIC_EXPORT_TIMEOUT",
    "OTEL_EXPORTER_OTLP_TIMEOUT",
    "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
    "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
    "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT",
];

/// The resource attribute that names the deployment. Required whenever
/// telemetry is on: it is half of the join key that makes one deployment's
/// three signals the same deployment (skill §1.2).
pub(crate) const DEPLOYMENT_ENVIRONMENT_NAME: &str = "deployment.environment.name";

/// One of the three signals. A plain enum rather than three booleans so a
/// message can name the signal, and so iteration covers all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl Signal {
    /// Every variant, for code that must consider each signal in turn.
    pub(crate) const ALL: [Signal; 3] = [Signal::Traces, Signal::Metrics, Signal::Logs];

    /// `OTEL_<SIGNAL>_EXPORTER` — `otlp` (the default) or `none`.
    fn exporter_variable(self) -> &'static str {
        match self {
            Signal::Traces => "OTEL_TRACES_EXPORTER",
            Signal::Metrics => "OTEL_METRICS_EXPORTER",
            Signal::Logs => "OTEL_LOGS_EXPORTER",
        }
    }

    /// `OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT` — a per-signal override of the
    /// shared endpoint, used verbatim by the SDK (no `/v1/<signal>` suffix is
    /// appended to it, unlike the shared one).
    fn endpoint_variable(self) -> &'static str {
        match self {
            Signal::Traces => "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            Signal::Metrics => "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            Signal::Logs => "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        }
    }

    /// `OTEL_EXPORTER_OTLP_<SIGNAL>_PROTOCOL` — the SDK reads this before the
    /// shared protocol variable, so it has to be validated too.
    fn protocol_variable(self) -> &'static str {
        match self {
            Signal::Traces => "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            Signal::Metrics => "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
            Signal::Logs => "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
        }
    }

    /// The path the spec appends to the *shared* endpoint for this signal (a
    /// per-signal endpoint is used exactly as given).
    fn otlp_path(self) -> &'static str {
        match self {
            Signal::Traces => "v1/traces",
            Signal::Metrics => "v1/metrics",
            Signal::Logs => "v1/logs",
        }
    }

    /// `OTEL_EXPORTER_OTLP_<SIGNAL>_TIMEOUT` — the per-signal export timeout,
    /// in milliseconds, which wins over the shared `OTEL_EXPORTER_OTLP_TIMEOUT`.
    fn timeout_variable(self) -> &'static str {
        match self {
            Signal::Traces => "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
            Signal::Metrics => "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
            Signal::Logs => "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT",
        }
    }

    /// Lower-case name for the startup line.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Signal::Traces => "traces",
            Signal::Metrics => "metrics",
            Signal::Logs => "logs",
        }
    }
}

/// Why telemetry is off. Each reason gets its own wording in the one startup
/// line, so "why are there no spans?" has an answer in `docker logs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OffReason {
    /// No `OTEL_*` variable at all — a laptop, a test, e2e, or an environment
    /// deployed without the `otel` render layer.
    NotConfigured,
    /// `OTEL_SDK_DISABLED=true` — a deployment that carries the variables and
    /// wants them silent.
    Disabled,
    /// Every signal's exporter is `none`.
    NoSignalExports,
}

impl OffReason {
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            OffReason::NotConfigured => "no OTEL_* variable is set",
            OffReason::Disabled => "OTEL_SDK_DISABLED=true",
            OffReason::NoSignalExports => "every signal's exporter is `none`",
        }
    }
}

/// What the validated variables say to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Off(OffReason),
    // Boxed: `Enabled` is a few hundred bytes and `Off` one, and clippy
    // rightly objects to every `Decision` paying for the larger.
    On(Box<Enabled>),
}

/// Telemetry is on. Everything the module needs that the SDK would not read
/// (or would read without checking) is carried here, already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Enabled {
    /// Which signals export. At least one is `true` (otherwise the decision is
    /// `Off(NoSignalExports)`).
    pub(crate) traces: bool,
    pub(crate) metrics: bool,
    pub(crate) logs: bool,
    /// `service.name`: `OTEL_SERVICE_NAME` if set, else the `service.name`
    /// entry of `OTEL_RESOURCE_ATTRIBUTES` (the spec's precedence).
    pub(crate) service_name: String,
    /// `deployment.environment.name` from `OTEL_RESOURCE_ATTRIBUTES`. Kept
    /// separately because startup cross-checks it against
    /// `observability.environment`.
    pub(crate) environment: String,
    /// Every `OTEL_RESOURCE_ATTRIBUTES` entry, decoded, in a stable order.
    /// `service.version` is removed: the build states it, not the deployment.
    pub(crate) resource_attributes: BTreeMap<String, String>,
    /// The shared endpoint, when set, as given — for the startup line only.
    /// What each exporter is actually handed is [`Enabled::endpoints`].
    pub(crate) endpoint: Option<String>,
    /// Each signal's export timeout, in `Signal::ALL` order: its own
    /// `OTEL_EXPORTER_OTLP_<SIGNAL>_TIMEOUT`, else the shared
    /// `OTEL_EXPORTER_OTLP_TIMEOUT`, else the spec's 10 s. The module applies
    /// it to the HTTP request itself, which the SDK cannot do for a client it
    /// did not build.
    pub(crate) timeouts: [std::time::Duration; 3],
    /// Each exporting signal's full endpoint URL, in `Signal::ALL` order: its
    /// own `OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT` as given, else the shared
    /// endpoint with the spec's `/v1/<signal>` appended. `None` for a signal
    /// that does not export.
    ///
    /// Resolved here and handed to each exporter explicitly, rather than left
    /// to the SDK's own reading of the environment, so the URL that is
    /// validated is the URL that is used — and the SDK's `localhost` fallback
    /// has nothing to fall back from.
    pub(crate) endpoints: [Option<String>; 3],
}

impl Enabled {
    /// Whether `signal` exports.
    pub(crate) fn exports(&self, signal: Signal) -> bool {
        match signal {
            Signal::Traces => self.traces,
            Signal::Metrics => self.metrics,
            Signal::Logs => self.logs,
        }
    }

    /// `signal`'s endpoint URL (see [`Enabled::endpoints`]).
    pub(crate) fn endpoint_for(&self, signal: Signal) -> Option<&str> {
        let index = Signal::ALL
            .iter()
            .position(|candidate| *candidate == signal)
            .expect("every signal is in ALL");
        self.endpoints[index].as_deref()
    }

    /// `signal`'s export timeout (see [`Enabled::timeouts`]).
    pub(crate) fn timeout(&self, signal: Signal) -> std::time::Duration {
        match signal {
            Signal::Traces => self.timeouts[0],
            Signal::Metrics => self.timeouts[1],
            Signal::Logs => self.timeouts[2],
        }
    }
}

/// The spec's default for `OTEL_EXPORTER_OTLP_TIMEOUT`.
const DEFAULT_EXPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A variable (or a combination of them) that fails validation. Names the
/// variable and why, **never** echoes a value it cannot vouch for: a header
/// variable, for instance, can carry a credential, so no message here quotes
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingsError {
    pub(crate) variable: String,
    pub(crate) reason: String,
}

impl SettingsError {
    fn new(variable: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            variable: variable.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid telemetry configuration `{}`: {}",
            self.variable, self.reason
        )
    }
}

impl std::error::Error for SettingsError {}

/// Decide from a list of environment variables.
///
/// `vars` may contain anything — only `OTEL_*` names are looked at. A value
/// that is empty after trimming counts as **absent**, which is the spec's rule
/// and also what a compose `${VAR:-}` pass-through produces for an unset
/// variable.
pub(crate) fn decide<I, K, V>(vars: I) -> Result<Decision, SettingsError>
where
    // Generic over the pair types so a test can pass `&str` literals and
    // production can pass the owned `String`s from `std::env::vars_os`.
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    // Keep only the family, trimmed, with blanks dropped. A `BTreeMap` gives a
    // deterministic order, so the first error reported is always the same one.
    let otel: BTreeMap<String, String> = vars
        .into_iter()
        .filter(|(name, _)| name.as_ref().starts_with(PREFIX))
        .map(|(name, value)| (name.as_ref().to_string(), value.as_ref().trim().to_string()))
        .filter(|(_, value)| !value.is_empty())
        .collect();

    // A small closure to look a variable up. `.map(String::as_str)` turns the
    // map's `Option<&String>` into the more useful `Option<&str>`.
    let get = |name: &str| otel.get(name).map(String::as_str);

    // ── Absent is off ────────────────────────────────────────────────────────
    if otel.is_empty() {
        return Ok(Decision::Off(OffReason::NotConfigured));
    }

    // ── The explicit off ─────────────────────────────────────────────────────
    // Checked first among the present variables: a deployment that says "be
    // silent" is not also asked to be well-formed in the rest of the set.
    // Anything other than a boolean is itself a typo worth failing on, because
    // `OTEL_SDK_DISABLED=ture` meaning "on" would be a surprise.
    if let Some(disabled) = get("OTEL_SDK_DISABLED") {
        match disabled.to_ascii_lowercase().as_str() {
            "true" => return Ok(Decision::Off(OffReason::Disabled)),
            "false" => {}
            _ => {
                return Err(SettingsError::new(
                    "OTEL_SDK_DISABLED",
                    "must be `true` or `false`",
                ));
            }
        }
    }

    // ── Which signals export ─────────────────────────────────────────────────
    // `[bool; 3]` indexed in `Signal::ALL` order. The spec allows a comma list
    // (e.g. `otlp,console`); this product only ever exports OTLP, so a list is
    // rejected rather than half-honoured.
    let mut exports = [true; 3];
    for (index, signal) in Signal::ALL.iter().enumerate() {
        if let Some(value) = get(signal.exporter_variable()) {
            exports[index] = match value {
                "otlp" => true,
                "none" => false,
                _ => {
                    return Err(SettingsError::new(
                        signal.exporter_variable(),
                        "must be `otlp` or `none`",
                    ));
                }
            };
        }
    }

    // ── Protocol ─────────────────────────────────────────────────────────────
    for variable in std::iter::once("OTEL_EXPORTER_OTLP_PROTOCOL")
        .chain(Signal::ALL.iter().map(|signal| signal.protocol_variable()))
    {
        if let Some(value) = get(variable)
            && value != HTTP_PROTOBUF
        {
            let reason = match value {
                // Name the two valid-but-unsupported protocols explicitly, so
                // the operator knows it is this build, not their spelling.
                "grpc" | "http/json" => format!(
                    "`{value}` is a valid OTLP protocol but this build carries only the \
                     `{HTTP_PROTOBUF}` client"
                ),
                _ => format!("must be `{HTTP_PROTOBUF}`"),
            };
            return Err(SettingsError::new(variable, reason));
        }
    }

    // ── Propagators ──────────────────────────────────────────────────────────
    // The module installs W3C trace-context and nothing else; a deployment
    // asking for B3 or baggage would otherwise be silently ignored.
    if let Some(value) = get("OTEL_PROPAGATORS")
        && value != "tracecontext"
    {
        return Err(SettingsError::new(
            "OTEL_PROPAGATORS",
            "must be absent or `tracecontext` (the only propagator installed)",
        ));
    }

    // ── Sampler ──────────────────────────────────────────────────────────────
    if let Some(sampler) = get("OTEL_TRACES_SAMPLER")
        && !SAMPLERS.contains(&sampler)
    {
        return Err(SettingsError::new(
            "OTEL_TRACES_SAMPLER",
            format!("must be one of {}", SAMPLERS.join(", ")),
        ));
    }
    if let Some(argument) = get("OTEL_TRACES_SAMPLER_ARG") {
        // `parse::<f64>` accepts "NaN" and "inf"; `contains` on the closed
        // range rejects both, since NaN compares false with everything.
        let in_range = argument
            .parse::<f64>()
            .is_ok_and(|ratio| (0.0..=1.0).contains(&ratio));
        if !in_range {
            return Err(SettingsError::new(
                "OTEL_TRACES_SAMPLER_ARG",
                "must be a number between 0 and 1",
            ));
        }
    }

    // ── Integer tuning knobs ─────────────────────────────────────────────────
    for variable in INTEGER_VARIABLES {
        if let Some(value) = get(variable)
            && value.parse::<u64>().is_err()
        {
            return Err(SettingsError::new(
                *variable,
                "must be a non-negative integer",
            ));
        }
    }

    // ── Endpoints ────────────────────────────────────────────────────────────
    // Every endpoint that is present must be a URL, whether or not its signal
    // exports — a typo is a typo.
    let shared_endpoint = get("OTEL_EXPORTER_OTLP_ENDPOINT");
    for variable in std::iter::once("OTEL_EXPORTER_OTLP_ENDPOINT")
        .chain(Signal::ALL.iter().map(|signal| signal.endpoint_variable()))
    {
        if let Some(value) = get(variable) {
            validate_endpoint(variable, value)?;
        }
    }

    // ── Resource: identity ───────────────────────────────────────────────────
    let mut resource_attributes = match get("OTEL_RESOURCE_ATTRIBUTES") {
        Some(raw) => parse_resource_attributes(raw)?,
        None => BTreeMap::new(),
    };
    // `service.version` is the build's to state (skill §5): an environment can
    // be stale, a binary cannot. Dropped here so it can never reach the
    // resource, whatever the deployment wrote.
    resource_attributes.remove("service.version");

    // Every signal silenced: nothing to identify and nothing to send. Checked
    // after the shape checks above so a malformed set still fails, but before
    // the "is it complete" checks below, which only matter when something
    // actually exports.
    if exports.iter().all(|exporting| !exporting) {
        return Ok(Decision::Off(OffReason::NoSignalExports));
    }

    // ── Absent endpoint is a fault, never localhost ──────────────────────────
    // A signal that exports needs *its* endpoint or the shared one. Without
    // either, the SDK would quietly use `http://localhost:4318`, which inside a
    // container is nothing at all.
    for (index, signal) in Signal::ALL.iter().enumerate() {
        if exports[index] && shared_endpoint.is_none() && get(signal.endpoint_variable()).is_none()
        {
            return Err(SettingsError::new(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                format!(
                    "is required while {} export (set it, or set that signal's exporter to `none`)",
                    signal.name()
                ),
            ));
        }
    }

    // `OTEL_SERVICE_NAME` wins over a `service.name` in the attribute list, as
    // the spec orders them. Absent both, the SDK would fill in
    // `unknown_service`, which is a series nobody queries for.
    let service_name = get("OTEL_SERVICE_NAME")
        .map(str::to_string)
        .or_else(|| resource_attributes.get("service.name").cloned())
        .ok_or_else(|| {
            SettingsError::new(
                "OTEL_SERVICE_NAME",
                "is required (or `service.name` in OTEL_RESOURCE_ATTRIBUTES)",
            )
        })?;
    // Recorded once, below, from `service_name`; never twice.
    resource_attributes.remove("service.name");

    let environment = resource_attributes
        .get(DEPLOYMENT_ENVIRONMENT_NAME)
        .cloned()
        .ok_or_else(|| {
            // Name the keys that *were* there — keys are attribute names, not
            // values, so this says what arrived without echoing any of it.
            let found: Vec<&str> = resource_attributes.keys().map(String::as_str).collect();
            SettingsError::new(
                "OTEL_RESOURCE_ATTRIBUTES",
                format!(
                    "must include `{DEPLOYMENT_ENVIRONMENT_NAME}` (keys present: [{}])",
                    found.join(", ")
                ),
            )
        })?;

    // Already checked to be non-negative integers (milliseconds) above.
    let millis = |variable: &str| get(variable).and_then(|value| value.parse::<u64>().ok());
    let shared_timeout = millis("OTEL_EXPORTER_OTLP_TIMEOUT");
    let timeouts = Signal::ALL.map(|signal| {
        millis(signal.timeout_variable())
            .or(shared_timeout)
            .map_or(DEFAULT_EXPORT_TIMEOUT, std::time::Duration::from_millis)
    });

    // Each exporting signal's URL. The checks above guarantee that one of the
    // two sources exists for every signal that exports.
    let mut endpoints: [Option<String>; 3] = [None, None, None];
    for (index, signal) in Signal::ALL.iter().enumerate() {
        if exports[index] {
            endpoints[index] = get(signal.endpoint_variable())
                .map(str::to_string)
                .or_else(|| {
                    shared_endpoint.map(|shared| {
                        format!("{}/{}", shared.trim_end_matches('/'), signal.otlp_path())
                    })
                });
        }
    }

    Ok(Decision::On(Box::new(Enabled {
        traces: exports[0],
        metrics: exports[1],
        logs: exports[2],
        service_name,
        environment,
        resource_attributes,
        endpoint: shared_endpoint.map(str::to_string),
        timeouts,
        endpoints,
    })))
}

/// An endpoint must be an absolute `http`/`https` URL with a host. `url`
/// parses far more than that (`mailto:`, `file:///`), so the scheme and host
/// are checked explicitly.
fn validate_endpoint(variable: &str, value: &str) -> Result<(), SettingsError> {
    let parsed = url::Url::parse(value)
        .map_err(|_| SettingsError::new(variable, "must be an absolute URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SettingsError::new(
            variable,
            "must be an http:// or https:// URL",
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(SettingsError::new(variable, "must name a host"));
    }
    Ok(())
}

/// Parse `key1=value1,key2=value2` (the W3C Baggage-like format the spec uses
/// for `OTEL_RESOURCE_ATTRIBUTES`), percent-decoding each value.
///
/// Stricter than the SDK's detector, which skips a malformed pair without a
/// word: here a pair with no `=`, an empty key, or a key given twice fails.
fn parse_resource_attributes(raw: &str) -> Result<BTreeMap<String, String>, SettingsError> {
    const VARIABLE: &str = "OTEL_RESOURCE_ATTRIBUTES";
    let mut attributes = BTreeMap::new();
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            // Tolerate a trailing comma; it carries no meaning.
            continue;
        }
        // `split_once` splits at the *first* `=`, so a value may itself contain
        // `=` (it should be percent-encoded, but need not be to parse).
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| SettingsError::new(VARIABLE, "every entry must be `key=value`"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(SettingsError::new(VARIABLE, "an entry has an empty key"));
        }
        let value = percent_decode(value.trim()).ok_or_else(|| {
            SettingsError::new(VARIABLE, format!("`{key}` is not valid percent-encoding"))
        })?;
        // `insert` returns the previous value when the key was already there.
        if attributes.insert(key.to_string(), value).is_some() {
            return Err(SettingsError::new(
                VARIABLE,
                format!("`{key}` is given twice"),
            ));
        }
    }
    Ok(attributes)
}

/// Decode `%XX` escapes. Returns `None` for a truncated or non-hex escape, or
/// a result that is not UTF-8.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            // `get` returns None past the end, which is the truncated case.
            let hex = bytes.get(index + 1..index + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            decoded.push(u8::from_str_radix(text, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set a deployment's `otel` layer renders: everything needed, nothing
    /// optional.
    fn complete() -> Vec<(&'static str, &'static str)> {
        vec![
            ("OTEL_SERVICE_NAME", "bored"),
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "deployment.environment.name=dev,telemetry_source=otlp",
            ),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://monitor-alloy:4318"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
        ]
    }

    /// `complete()` with one variable replaced (or added).
    fn with(name: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
        let mut vars: Vec<_> = complete().into_iter().filter(|(n, _)| *n != name).collect();
        vars.push((name, value));
        vars
    }

    /// `complete()` with one variable removed.
    fn without(name: &'static str) -> Vec<(&'static str, &'static str)> {
        complete().into_iter().filter(|(n, _)| *n != name).collect()
    }

    fn error_variable(vars: Vec<(&'static str, &'static str)>) -> String {
        decide(vars)
            .expect_err("expected a startup failure")
            .variable
    }

    #[test]
    fn no_otel_variable_is_off_and_validates_nothing() {
        // Unrelated variables — including malformed-looking ones — do not
        // count: only the `OTEL_` family switches validation on.
        let decision = decide([("PATH", "/usr/bin"), ("BORED__SERVER__HTTP-PORT", "x")]).unwrap();
        assert_eq!(decision, Decision::Off(OffReason::NotConfigured));
    }

    #[test]
    fn blank_otel_variables_count_as_absent() {
        // What compose produces for `- OTEL_X=${OTEL_X:-}` with nothing set.
        let decision = decide([
            ("OTEL_SERVICE_NAME", "  "),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", ""),
        ])
        .unwrap();
        assert_eq!(decision, Decision::Off(OffReason::NotConfigured));
    }

    #[test]
    fn the_complete_set_turns_every_signal_on() {
        let Decision::On(enabled) = decide(complete()).unwrap() else {
            panic!("expected telemetry on");
        };
        assert!(enabled.traces && enabled.metrics && enabled.logs);
        assert_eq!(enabled.service_name, "bored");
        assert_eq!(enabled.environment, "dev");
        assert_eq!(
            enabled
                .resource_attributes
                .get("telemetry_source")
                .map(String::as_str),
            Some("otlp")
        );
        assert_eq!(
            enabled.endpoint.as_deref(),
            Some("http://monitor-alloy:4318")
        );
    }

    #[test]
    fn sdk_disabled_true_is_off_even_with_a_broken_set() {
        // The explicit silence is honoured before anything else is checked.
        let vars = [
            ("OTEL_SDK_DISABLED", "TRUE"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "carrier-pigeon"),
        ];
        assert_eq!(decide(vars).unwrap(), Decision::Off(OffReason::Disabled));
    }

    #[test]
    fn sdk_disabled_must_be_a_boolean() {
        assert_eq!(
            error_variable(with("OTEL_SDK_DISABLED", "yes")),
            "OTEL_SDK_DISABLED"
        );
        // `false` is a boolean, and leaves the rest of the set in charge.
        assert!(matches!(
            decide(with("OTEL_SDK_DISABLED", "false")).unwrap(),
            Decision::On(_)
        ));
    }

    #[test]
    fn a_half_present_set_fails_startup() {
        // Identity but nowhere to send it — the classic half-configured deploy.
        let vars = [("OTEL_SERVICE_NAME", "bored")];
        assert_eq!(error_variable(vars.to_vec()), "OTEL_EXPORTER_OTLP_ENDPOINT");
    }

    #[test]
    fn an_absent_endpoint_never_becomes_localhost() {
        // Everything else present: the SDK would happily default to
        // localhost:4318 here. The module must refuse instead.
        let error = decide(without("OTEL_EXPORTER_OTLP_ENDPOINT")).unwrap_err();
        assert_eq!(error.variable, "OTEL_EXPORTER_OTLP_ENDPOINT");
        assert!(!error.reason.contains("localhost"), "{error}");
    }

    #[test]
    fn per_signal_endpoints_can_stand_in_for_the_shared_one() {
        let mut vars = without("OTEL_EXPORTER_OTLP_ENDPOINT");
        vars.push((
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://t:4318/v1/traces",
        ));
        vars.push((
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            "http://m:4318/v1/metrics",
        ));
        // Logs has no endpoint of its own and no shared one → still a failure.
        assert_eq!(error_variable(vars.clone()), "OTEL_EXPORTER_OTLP_ENDPOINT");
        // …unless logs are silenced.
        vars.push(("OTEL_LOGS_EXPORTER", "none"));
        let Decision::On(enabled) = decide(vars).unwrap() else {
            panic!("expected on")
        };
        assert!(enabled.traces && enabled.metrics && !enabled.logs);
    }

    #[test]
    fn a_silenced_signal_needs_no_endpoint_but_the_others_do() {
        let vars = with("OTEL_TRACES_EXPORTER", "none");
        let Decision::On(enabled) = decide(vars).unwrap() else {
            panic!("expected on")
        };
        assert!(!enabled.traces && enabled.metrics && enabled.logs);
    }

    #[test]
    fn every_signal_silenced_is_off() {
        let mut vars = without("OTEL_EXPORTER_OTLP_ENDPOINT");
        for signal in Signal::ALL {
            vars.push((signal.exporter_variable(), "none"));
        }
        assert_eq!(
            decide(vars).unwrap(),
            Decision::Off(OffReason::NoSignalExports)
        );
    }

    #[test]
    fn malformed_values_fail_startup() {
        // One case per rule, each naming the variable it is about. Iterating a
        // table keeps the rule → variable pairing visible in one place.
        let cases: &[(&str, &str)] = &[
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http"),
            ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc"),
            ("OTEL_TRACES_EXPORTER", "jaeger"),
            ("OTEL_METRICS_EXPORTER", "prometheus"),
            ("OTEL_LOGS_EXPORTER", "otlp,console"),
            ("OTEL_TRACES_SAMPLER", "sometimes"),
            ("OTEL_TRACES_SAMPLER_ARG", "1.5"),
            ("OTEL_TRACES_SAMPLER_ARG", "-0.1"),
            ("OTEL_TRACES_SAMPLER_ARG", "NaN"),
            ("OTEL_TRACES_SAMPLER_ARG", "half"),
            ("OTEL_PROPAGATORS", "b3"),
            ("OTEL_PROPAGATORS", "tracecontext,baggage"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "monitor-alloy:4318"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "ftp://monitor-alloy"),
            ("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "not a url"),
            ("OTEL_BSP_MAX_QUEUE_SIZE", "lots"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "-5"),
        ];
        for (variable, value) in cases {
            let error = decide(with(variable, value))
                .err()
                .unwrap_or_else(|| panic!("{variable}={value} should fail startup"));
            assert_eq!(&error.variable, variable, "{variable}={value}: {error}");
        }
    }

    #[test]
    fn well_formed_optional_values_are_accepted() {
        let mut vars = complete();
        vars.extend([
            ("OTEL_TRACES_SAMPLER", "parentbased_traceidratio"),
            ("OTEL_TRACES_SAMPLER_ARG", "0.25"),
            ("OTEL_PROPAGATORS", "tracecontext"),
            ("OTEL_BSP_MAX_QUEUE_SIZE", "4096"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "15000"),
        ]);
        assert!(matches!(decide(vars).unwrap(), Decision::On(_)));
    }

    #[test]
    fn identity_is_required_when_telemetry_is_on() {
        assert_eq!(
            error_variable(without("OTEL_SERVICE_NAME")),
            "OTEL_SERVICE_NAME"
        );
        let vars = with("OTEL_RESOURCE_ATTRIBUTES", "telemetry_source=otlp");
        assert_eq!(error_variable(vars), "OTEL_RESOURCE_ATTRIBUTES");
    }

    #[test]
    fn service_name_may_come_from_the_attribute_list() {
        let mut vars = without("OTEL_SERVICE_NAME");
        vars.retain(|(n, _)| *n != "OTEL_RESOURCE_ATTRIBUTES");
        vars.push((
            "OTEL_RESOURCE_ATTRIBUTES",
            "service.name=bored,deployment.environment.name=prod",
        ));
        let Decision::On(enabled) = decide(vars).unwrap() else {
            panic!("expected on")
        };
        assert_eq!(enabled.service_name, "bored");
        // Held once, as `service_name`, not again among the attributes.
        assert!(!enabled.resource_attributes.contains_key("service.name"));
    }

    #[test]
    fn the_deployment_cannot_state_the_version() {
        let vars = with(
            "OTEL_RESOURCE_ATTRIBUTES",
            "deployment.environment.name=dev,service.version=0.0.1-stale",
        );
        let Decision::On(enabled) = decide(vars).unwrap() else {
            panic!("expected on")
        };
        assert!(!enabled.resource_attributes.contains_key("service.version"));
    }

    #[test]
    fn resource_attributes_are_parsed_strictly_and_percent_decoded() {
        let parsed = parse_resource_attributes("a=1, b = two%20words ,c=x=y,").unwrap();
        assert_eq!(parsed.get("a").map(String::as_str), Some("1"));
        assert_eq!(parsed.get("b").map(String::as_str), Some("two words"));
        assert_eq!(parsed.get("c").map(String::as_str), Some("x=y"));
        assert!(parse_resource_attributes("novalue").is_err());
        assert!(parse_resource_attributes("=v").is_err());
        assert!(parse_resource_attributes("a=1,a=2").is_err());
        assert!(parse_resource_attributes("a=%zz").is_err());
        assert!(parse_resource_attributes("a=%2").is_err());
    }

    #[test]
    fn endpoints_are_resolved_per_signal_as_the_spec_says() {
        let mut vars = with("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318/");
        vars.push((
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
            "http://logs:9999/custom",
        ));
        let Decision::On(enabled) = decide(vars).unwrap() else {
            panic!("on")
        };
        // The shared endpoint gains `/v1/<signal>` (one slash, whatever the
        // value ended with); a per-signal endpoint is used exactly as given.
        assert_eq!(
            enabled.endpoint_for(Signal::Traces),
            Some("http://collector:4318/v1/traces")
        );
        assert_eq!(
            enabled.endpoint_for(Signal::Metrics),
            Some("http://collector:4318/v1/metrics")
        );
        assert_eq!(
            enabled.endpoint_for(Signal::Logs),
            Some("http://logs:9999/custom")
        );
        // A silenced signal has none.
        let Decision::On(silenced) = decide(with("OTEL_TRACES_EXPORTER", "none")).unwrap() else {
            panic!("on")
        };
        assert_eq!(silenced.endpoint_for(Signal::Traces), None);
    }

    #[test]
    fn export_timeouts_follow_the_spec_precedence() {
        use std::time::Duration;
        let Decision::On(defaulted) = decide(complete()).unwrap() else {
            panic!("on")
        };
        for signal in Signal::ALL {
            assert_eq!(defaulted.timeout(signal), Duration::from_secs(10));
        }
        let mut vars = complete();
        vars.push(("OTEL_EXPORTER_OTLP_TIMEOUT", "2500"));
        vars.push(("OTEL_EXPORTER_OTLP_LOGS_TIMEOUT", "700"));
        let Decision::On(set) = decide(vars).unwrap() else {
            panic!("on")
        };
        // The per-signal value wins; the shared one covers the rest.
        assert_eq!(set.timeout(Signal::Logs), Duration::from_millis(700));
        assert_eq!(set.timeout(Signal::Traces), Duration::from_millis(2500));
        assert_eq!(set.timeout(Signal::Metrics), Duration::from_millis(2500));
    }

    #[test]
    fn errors_never_quote_the_offending_value() {
        // A header or endpoint variable can carry a credential; the message
        // names the variable and the rule only.
        let secret = "https://user:hunter2@collector";
        let mut vars = complete();
        vars.push(("OTEL_EXPORTER_OTLP_PROTOCOL", "hunter2"));
        vars.retain(|(n, v)| !(*n == "OTEL_EXPORTER_OTLP_PROTOCOL" && *v == "http/protobuf"));
        let error = decide(vars).unwrap_err();
        assert!(!error.to_string().contains("hunter2"), "{error}");
        let error = decide(with("OTEL_EXPORTER_OTLP_ENDPOINT", "mailto:x")).unwrap_err();
        assert!(!error.to_string().contains(secret));
    }
}
