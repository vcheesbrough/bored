//! The telemetry module: the **only** place an OpenTelemetry SDK or exporter
//! type appears (card #415, the `observability` skill §5).
//!
//! Product code emits through façades and never learns where anything goes:
//!
//! - spans and logs through `tracing` (`info_span!`, `#[instrument]`,
//!   `info!`, …);
//! - metrics through [`metrics`], which uses only the `opentelemetry` API;
//! - trace-context propagation through [`adopt_parent`], [`inject_current`]
//!   and [`link_to`], which the transport layer calls.
//!
//! This module turns those into OTLP. A test (`tests::nothing_else_imports_the_sdk`)
//! fails if any other file names `opentelemetry_sdk`, `opentelemetry_otlp`,
//! `tracing_opentelemetry` or `opentelemetry_appender_tracing`, so swapping
//! the SDK stays a one-module change.
//!
//! ## What gets installed
//!
//! ```text
//!  tracing ─┬─► span layer  (spans only; never level-filtered) ─► batch ─► OTLP /v1/traces
//!           ├─► fmt layer   (JSON on stdout + trace_id/span_id; log level) ─► docker logs
//!           └─► log bridge  (events → OTel log records; log level)       ─► batch ─► OTLP /v1/logs
//!  metrics ─────► meter provider ─► periodic reader (cumulative)          ─► OTLP /v1/metrics
//! ```
//!
//! Configuration is the standard `OTEL_*` variables and nothing else; see
//! [`settings`] for what is read and how it is validated. **Off** — no
//! variable set, or `OTEL_SDK_DISABLED=true` — installs the fmt layer, a span
//! layer over the no-op tracer (so inbound trace context still passes through
//! to outbound calls and log lines) and the W3C propagator: no providers, no
//! exporters, no background threads.

pub(crate) mod metrics;
mod settings;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use opentelemetry::trace::TraceContextExt as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use opentelemetry_otlp::{Protocol, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::resource::TelemetryResourceDetector;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_semantic_conventions::resource::{SERVICE_INSTANCE_ID, SERVICE_VERSION};
use tracing::dispatcher::WeakDispatch;
use tracing::{Dispatch, Level, Metadata, Subscriber};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::filter::{EnvFilter, FilterExt as _, filter_fn};
use tracing_subscriber::fmt::format::{JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::layer::{Layer as _, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt as _;

pub(crate) use settings::SettingsError;
use settings::{Decision, Enabled, Signal};

/// The log level in force from [`init`] until `main` has loaded config and
/// calls [`Telemetry::set_log_level`]. Config supplies the real value, but the
/// module has to be up *before* config loads, so the sovereign-config startup
/// call can have a span.
const INITIAL_LOG_LEVEL: &str = "info";

/// How long [`Telemetry::shutdown`] waits for the three providers to flush.
/// Together with `main`'s connection-drain grace period this stays inside
/// Docker's stop timeout, so the flush happens before a SIGKILL could.
pub(crate) const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// The instrumentation scope every product span, log and metric is reported
/// under.
const SCOPE: &str = "bored";

/// The UUID namespace semantic conventions define for deriving a
/// `service.instance.id` as a version-5 UUID
/// (`4d63009a-8d0f-11ee-aad7-4c796ed8e320`).
const SERVICE_INSTANCE_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0x4d63009a_8d0f_11ee_aad7_4c796ed8e320);

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

/// A telemetry startup failure. Each one stops the process: a deployment that
/// *asked* for telemetry and would silently not get it is exactly the failure
/// mode the validation exists to prevent.
#[derive(Debug)]
pub enum TelemetryError {
    /// An `OTEL_*` variable, or the set as a whole, is invalid.
    Settings(SettingsError),
    /// An exporter could not be built (e.g. the endpoint the SDK re-parses).
    Exporter {
        signal: &'static str,
        reason: String,
    },
    /// `observability.environment` and `deployment.environment.name` disagree.
    EnvironmentMismatch {
        configured: String,
        resource: String,
    },
    /// A global subscriber was already installed (only reachable in tests).
    AlreadyInitialised,
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryError::Settings(error) => write!(f, "{error}"),
            TelemetryError::Exporter { signal, reason } => {
                write!(f, "could not build the OTLP {signal} exporter: {reason}")
            }
            TelemetryError::EnvironmentMismatch {
                configured,
                resource,
            } => write!(
                f,
                "observability.environment is `{configured}` but OTEL_RESOURCE_ATTRIBUTES says \
                 deployment.environment.name=`{resource}`; both come from the deploy's APP_ENV \
                 and must agree"
            ),
            TelemetryError::AlreadyInitialised => write!(f, "telemetry was already initialised"),
        }
    }
}

impl std::error::Error for TelemetryError {}

impl From<SettingsError> for TelemetryError {
    fn from(error: SettingsError) -> Self {
        TelemetryError::Settings(error)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Providers
// ─────────────────────────────────────────────────────────────────────────────

/// The SDK providers for whichever signals export. All `None` when telemetry
/// is off.
#[derive(Default)]
struct Providers {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
    meter: Option<SdkMeterProvider>,
}

impl Providers {
    /// Real OTLP exporters for every signal `enabled` exports.
    ///
    /// Must run inside the Tokio runtime: the HTTP client below spawns each
    /// export request onto it (see [`OtlpHttpClient`]).
    fn otlp(enabled: &Enabled) -> Result<Self, TelemetryError> {
        let resource = resource(enabled);
        // One client per signal, each with that signal's export timeout on the
        // request itself (see `settings::Enabled::timeouts`).
        let runtime = tokio::runtime::Handle::current();
        let client_for =
            |signal: Signal| OtlpHttpClient::new(runtime.clone(), enabled.timeout(signal));
        // Each exporter is given the URL settings.rs resolved and validated,
        // explicitly, so the SDK's own environment reading (and its
        // `localhost` default) never decides where telemetry goes. It is
        // present for every signal that exports; the empty fallback is
        // unreachable and would fail the build of that exporter.
        let endpoint_for =
            |signal: Signal| enabled.endpoint_for(signal).unwrap_or_default().to_string();

        // A small helper so each builder's error names its signal.
        let failed = |signal: Signal| {
            move |error: opentelemetry_otlp::ExporterBuildError| TelemetryError::Exporter {
                signal: signal.name(),
                reason: error.to_string(),
            }
        };

        // Each exporter: `with_http()` picks the HTTP transport,
        // `with_protocol(HttpBinary)` is http/protobuf, and `with_http_client`
        // hands it our client instead of letting it build its own, and
        // `with_endpoint` the resolved URL. Headers are *not* set here: the
        // builder reads `OTEL_EXPORTER_OTLP_HEADERS` itself. It reads the
        // timeout variables too, but only for its retry deadline — the request
        // itself is bounded by the client above.
        let tracer = enabled
            .traces
            .then(|| {
                opentelemetry_otlp::SpanExporter::builder()
                    .with_http()
                    .with_protocol(Protocol::HttpBinary)
                    .with_endpoint(endpoint_for(Signal::Traces))
                    .with_http_client(client_for(Signal::Traces))
                    .build()
                    .map(|exporter| tracer_provider(exporter, resource.clone()))
                    .map_err(failed(Signal::Traces))
            })
            // `Option<Result<T, E>>` → `Result<Option<T>, E>`, so `?` applies.
            .transpose()?;
        let logger = enabled
            .logs
            .then(|| {
                opentelemetry_otlp::LogExporter::builder()
                    .with_http()
                    .with_protocol(Protocol::HttpBinary)
                    .with_endpoint(endpoint_for(Signal::Logs))
                    .with_http_client(client_for(Signal::Logs))
                    .build()
                    .map(|exporter| logger_provider(exporter, resource.clone()))
                    .map_err(failed(Signal::Logs))
            })
            .transpose()?;
        let meter = enabled
            .metrics
            .then(|| {
                opentelemetry_otlp::MetricExporter::builder()
                    .with_http()
                    .with_protocol(Protocol::HttpBinary)
                    .with_endpoint(endpoint_for(Signal::Metrics))
                    .with_http_client(client_for(Signal::Metrics))
                    // Cumulative is the contract (skill §2): a backend that
                    // wants delta gets it from the collector.
                    .with_temporality(Temporality::Cumulative)
                    .build()
                    .map(|exporter| meter_provider(exporter, resource.clone()))
                    .map_err(failed(Signal::Metrics))
            })
            .transpose()?;

        Ok(Self {
            tracer,
            logger,
            meter,
        })
    }
}

/// Traces: batch processor → exporter. The batch processor reads the
/// `OTEL_BSP_*` variables, and the provider's default config reads
/// `OTEL_TRACES_SAMPLER`/`_ARG` (parent-based always-on when unset).
///
/// Generic over the exporter so the tests build the *same* pipeline over an
/// in-memory exporter.
fn tracer_provider<E>(exporter: E, resource: Resource) -> SdkTracerProvider
where
    E: opentelemetry_sdk::trace::SpanExporter + 'static,
{
    SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build()
}

/// Logs: batch processor (`OTEL_BLRP_*`) → exporter.
fn logger_provider<E>(exporter: E, resource: Resource) -> SdkLoggerProvider
where
    E: opentelemetry_sdk::logs::LogExporter + 'static,
{
    SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build()
}

/// Metrics: periodic reader (`OTEL_METRIC_EXPORT_INTERVAL`/`_TIMEOUT`) →
/// exporter.
fn meter_provider<E>(exporter: E, resource: Resource) -> SdkMeterProvider
where
    E: opentelemetry_sdk::metrics::exporter::PushMetricExporter,
{
    SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter).build())
        .with_resource(resource)
        .build()
}

/// The one `Resource`, built once and handed to all three providers, so the
/// identity on traces, logs and metrics is the same by construction.
///
/// - `service.name` and every `OTEL_RESOURCE_ATTRIBUTES` entry, as validated
///   by settings.rs (the environment detector's job, done by the validator so
///   a malformed entry fails instead of vanishing);
/// - `telemetry.sdk.*` from the SDK's own detector;
/// - `service.version` from the **build** — `shared::app_version()`, the
///   release tag compiled in — which the deployment cannot override;
/// - `service.instance.id` as a v5 UUID of the host name, unless the
///   deployment supplied one.
///
/// No host or process detectors: they add values (the pid, for one) that
/// change on every restart and would start new metric series each time.
fn resource(enabled: &Enabled) -> Resource {
    let mut attributes: Vec<KeyValue> = enabled
        .resource_attributes
        .iter()
        .map(|(key, value)| KeyValue::new(key.clone(), value.clone()))
        .collect();
    if !enabled
        .resource_attributes
        .contains_key(SERVICE_INSTANCE_ID)
        && let Some(instance) = service_instance_id()
    {
        attributes.push(KeyValue::new(SERVICE_INSTANCE_ID, instance));
    }
    attributes.push(KeyValue::new(SERVICE_VERSION, shared::app_version()));

    Resource::builder_empty()
        .with_detector(Box::new(TelemetryResourceDetector))
        .with_service_name(enabled.service_name.clone())
        .with_attributes(attributes)
        .build()
}

/// A stable, unique id for this replica: a v5 UUID of its host name.
///
/// Stable because the compose file pins the container's hostname to its
/// container name, which survives restarts *and* redeploys (a container id
/// would change on every `up`, starting a new set of metric series each
/// time). Hashed so the host name itself is not published. `None` when no
/// host name can be found — absent is allowed, a random value is not.
fn service_instance_id() -> Option<String> {
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())?;
    let hostname = hostname.trim();
    (!hostname.is_empty())
        .then(|| uuid::Uuid::new_v5(&SERVICE_INSTANCE_NAMESPACE, hostname.as_bytes()).to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// The OTLP HTTP client
// ─────────────────────────────────────────────────────────────────────────────

/// The HTTP client the OTLP exporters send through.
///
/// **Why not the exporter's own?** `opentelemetry-otlp` would bring a second
/// major version of `reqwest` (0.13) into a binary that already carries 0.12,
/// with its own TLS stack. This adapter implements the exporter's
/// `HttpClient` trait over the workspace's `reqwest`, so there is one HTTP
/// stack.
///
/// **Why the runtime handle?** The SDK's batch processors and periodic reader
/// run on their own OS threads and drive each export with a minimal
/// `block_on` executor — not a Tokio runtime. `reqwest` needs Tokio's reactor
/// for its sockets, so each request is spawned onto the application's runtime
/// and the exporter thread merely waits for its result. (The alternative, the
/// exporter's blocking client, would start a runtime of its own.)
#[derive(Debug, Clone)]
struct OtlpHttpClient {
    http: reqwest::Client,
    runtime: tokio::runtime::Handle,
}

impl OtlpHttpClient {
    /// `timeout` bounds each export request, send to last response byte — the
    /// signal's `OTEL_EXPORTER_OTLP_*TIMEOUT` (10 s by default).
    fn new(runtime: tokio::runtime::Handle, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            // `build` fails only if the TLS backend cannot initialise, which
            // the rest of the process would hit first; a plain client keeps
            // telemetry from ever being the thing that fails startup here.
            .unwrap_or_default();
        Self { http, runtime }
    }
}

// `#[async_trait]` rewrites the `async fn` into a method returning a boxed
// future, which is the only way a trait could hold an async method when the
// SDK trait was written.
#[async_trait::async_trait]
impl opentelemetry_http::HttpClient for OtlpHttpClient {
    async fn send_bytes(
        &self,
        request: http::Request<bytes::Bytes>,
    ) -> Result<http::Response<bytes::Bytes>, opentelemetry_http::HttpError> {
        // Convert the `http::Request` the exporter built into reqwest's own
        // request type (reqwest 0.12 implements `TryFrom` for this).
        let request = reqwest::Request::try_from(request)?;
        let http = self.http.clone();
        // Run the whole round trip on the Tokio runtime; `JoinHandle` is itself
        // a future any executor can wait on.
        let exchange = self.runtime.spawn(async move {
            let response = http.execute(request).await?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.bytes().await?;
            Ok::<_, reqwest::Error>((status, headers, body))
        });
        // The first `?` is a panicked or cancelled task, the second a transport
        // error; both become the exporter's boxed error, which it logs (rate-
        // limited) on its own target and then drops the batch.
        let (status, headers, body) = exchange.await??;
        let mut response = http::Response::builder().status(status).body(body)?;
        *response.headers_mut() = headers;
        Ok(response)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Subscriber assembly
// ─────────────────────────────────────────────────────────────────────────────

/// Whether `target` is the OpenTelemetry SDK talking about itself — export
/// failures, dropped batches. Every crate of the family logs under its own
/// package name, all of which start with `opentelemetry`.
fn is_sdk_target(target: &str) -> bool {
    target.starts_with("opentelemetry")
}

/// Whether `target` is one of the HTTP client crates the exporter sends
/// through. Their debug chatter about an export's own request must not become
/// a log record that is itself exported — a loop that feeds itself.
fn is_http_client_target(target: &str) -> bool {
    ["hyper", "hyper_util", "h2", "reqwest"]
        .iter()
        .any(|crate_name| target == *crate_name || target.starts_with(&format!("{crate_name}::")))
}

/// The span filter: spans only, and only spans this crate opens.
///
/// - **Spans only**: without this the span layer would also record every event
///   as a span event while the log bridge turns the same event into a log
///   record — one fact, twice (skill §3).
/// - **This crate only**: dependencies (surrealdb in particular) open spans at
///   `trace` level on hot paths; exporting those would be noise, and enabling
///   them would cost every call.
/// - **No level**: the product's log-level knob never touches spans. A `warn`
///   filter on the whole subscriber would silently drop every span.
fn is_product_span(metadata: &Metadata<'_>) -> bool {
    metadata.is_span() && metadata.target().starts_with(env!("CARGO_CRATE_NAME"))
}

/// Swaps the log-level filter on the fmt and bridge layers at runtime.
///
/// Two reload handles, one per layer, each re-parsing the same directive
/// string (an `EnvFilter` is not `Clone`). Boxed closures because the handles'
/// full types name the layered subscriber they sit in, which is unwritable.
/// One layer's "apply this directive string" callback.
type Reloader = Box<dyn Fn(&str) + Send + Sync>;

struct LogLevel {
    reloaders: Vec<Reloader>,
}

impl LogLevel {
    fn set(&self, directives: &str) {
        for reload in &self.reloaders {
            reload(directives);
        }
    }
}

/// Parse a log-level directive (`info`, `debug,surrealdb=warn`, …). An
/// unparseable one is a config typo, not a reason to refuse to start, so it
/// falls back to `info` — as the pre-#415 module did.
fn level_filter(directives: &str) -> EnvFilter {
    EnvFilter::try_new(directives).unwrap_or_else(|_| EnvFilter::new(INITIAL_LOG_LEVEL))
}

/// Build the subscriber: span layer, fmt layer, log bridge.
///
/// Split from [`init`] so the tests can build it over in-memory exporters and
/// a captured writer and run it with `tracing::subscriber::with_default`.
fn subscriber<W>(
    providers: &Providers,
    writer: W,
    directives: &str,
) -> (
    impl Subscriber + Send + Sync + for<'a> LookupSpan<'a> + use<W>,
    LogLevel,
)
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    // ── Span layer ───────────────────────────────────────────────────────────
    // One of two, depending on whether traces export. `Option<Layer>` is
    // itself a layer (a `None` does nothing), which avoids boxing.
    //
    // Location attributes (`code.file.path`, `code.line.number`,
    // `code.module.name`) name the call site that opened the span. Thread
    // attributes are semconv too. Level, target and busy/idle timings are
    // switched off: they are not semconv names and say nothing the span's
    // own name and duration do not.
    let tracer = providers.tracer.as_ref().map(|provider| {
        use opentelemetry::trace::TracerProvider as _;
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer(SCOPE))
            .with_tracked_inactivity(false)
            .with_level(false)
            .with_target(false)
            .with_filter(filter_fn(is_product_span))
    });
    // With traces off, the same layer over the API's no-op tracer. It exports
    // nothing and starts no thread, but a no-op span *carries its parent's
    // context*, so a traceparent adopted from a request still reaches outbound
    // calls and log lines — a silent service must not break the trace passing
    // through it (skill §2).
    let passthrough = providers.tracer.is_none().then(|| {
        tracing_opentelemetry::layer()
            .with_tracked_inactivity(false)
            .with_filter(filter_fn(is_product_span))
    });

    // ── fmt layer ────────────────────────────────────────────────────────────
    // JSON, one object per line (Alloy ships one Loki entry per physical line),
    // with the event's own fields flattened to the top level so a Loki
    // `| json` query can address them. `WithTraceIds` adds `trace_id` and
    // `span_id` to every line written inside a span.
    let (fmt_level, fmt_handle) = reload::Layer::new(level_filter(directives));
    let dispatch = DispatchHandle::default();
    let fmt = tracing_subscriber::fmt::layer()
        .fmt_fields(JsonFields::new())
        .event_format(WithTraceIds {
            inner: tracing_subscriber::fmt::format().json().flatten_event(true),
            dispatch: dispatch.clone(),
        })
        .with_writer(writer)
        // The SDK's own target is held at `warn` here — export failures are
        // the §6 failure signal, and stay visible even at `info`.
        .with_filter(fmt_level.and(filter_fn(|metadata| {
            !is_sdk_target(metadata.target()) || *metadata.level() <= Level::WARN
        })));

    // ── Log bridge ───────────────────────────────────────────────────────────
    // Events → OTel log records. The SDK's own target is excluded entirely: a
    // failing export must never produce a log record that then fails to
    // export. The exporter's HTTP client crates are excluded below `warn` for
    // the same reason. Span attributes are *not* copied onto records (the
    // bridge's `experimental_span_attributes` feature is off): a record is
    // joined to its span by trace id, and copying would log what the span
    // already records.
    let mut reloaders: Vec<Reloader> = vec![Box::new(move |directives| {
        // A failed reload means the subscriber is gone; nothing to do.
        let _ = fmt_handle.reload(level_filter(directives));
    })];
    let bridge = providers.logger.as_ref().map(|provider| {
        let (bridge_level, bridge_handle) = reload::Layer::new(level_filter(directives));
        reloaders.push(Box::new(move |directives| {
            let _ = bridge_handle.reload(level_filter(directives));
        }));
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(provider)
            .with_filter(bridge_level.and(filter_fn(|metadata| {
                let target = metadata.target();
                !is_sdk_target(target)
                    && (!is_http_client_target(target) || *metadata.level() <= Level::WARN)
            })))
    });

    let subscriber = tracing_subscriber::registry()
        .with(CaptureDispatch(dispatch))
        .with(tracer)
        .with(passthrough)
        .with(fmt)
        .with(bridge);
    (subscriber, LogLevel { reloaders })
}

/// A handle to the `Dispatch` the subscriber was installed in.
///
/// [`WithTraceIds`] needs it to look up a span's OpenTelemetry context, and
/// cannot get it the usual way: `tracing::dispatcher::get_default` is
/// deliberately non-reentrant, and a formatter runs *inside* the dispatch of
/// the event it formats, so there it returns the no-op dispatcher. Instead,
/// [`CaptureDispatch`] (a layer that does nothing else) is told the dispatch
/// when the subscriber is installed, and keeps a weak reference for the
/// formatter.
#[derive(Clone, Default)]
struct DispatchHandle(Arc<RwLock<Option<WeakDispatch>>>);

impl DispatchHandle {
    fn get(&self) -> Option<Dispatch> {
        // A poisoned lock (a panic while writing) is treated as "no dispatch":
        // the line is then written without ids rather than not at all.
        self.0.read().ok()?.as_ref()?.upgrade()
    }
}

/// The layer that fills a [`DispatchHandle`] (see there).
struct CaptureDispatch(DispatchHandle);

impl<S: Subscriber> tracing_subscriber::Layer<S> for CaptureDispatch {
    fn on_register_dispatch(&self, dispatch: &Dispatch) {
        if let Ok(mut slot) = (self.0).0.write() {
            // Weak, so the subscriber does not keep itself alive.
            *slot = Some(dispatch.downgrade());
        }
    }
}

/// A JSON event formatter that also writes the enclosing span's `trace_id`
/// and `span_id`, so a stdout line (and the Loki entry Alloy makes of it) can
/// be found from its trace and vice versa.
///
/// It formats with the wrapped formatter into a buffer, then splices the two
/// fields in before the closing brace. That keeps every other part of the
/// line byte-identical to `tracing-subscriber`'s JSON output.
struct WithTraceIds<F> {
    inner: F,
    dispatch: DispatchHandle,
}

impl<S, N, F> FormatEvent<S, N> for WithTraceIds<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut line = String::new();
        self.inner
            .format_event(ctx, Writer::new(&mut line), event)?;

        // The event's scope, innermost span first (`event_scope` honours an
        // explicit `parent:` as well as the current span). The ids are
        // collected up front so no span is borrowed while the span layer's
        // extensions are read below.
        let scope: Vec<tracing::span::Id> = ctx
            .event_scope()
            .map(|scope| scope.map(|span| span.id()).collect())
            .unwrap_or_default();
        // The nearest enclosing span the span layer tracks. The innermost one
        // may be a dependency's (surrealdb opens `debug` spans) that the span
        // layer filters out and so has no OTel context; the log bridge still
        // correlates the event through the product span around it, and the
        // stdout copy must name the same span. Done *after* formatting, so no
        // extensions are borrowed while the span layer takes its lock.
        let ids = self.dispatch.get().and_then(|dispatch| {
            scope.iter().find_map(|id| {
                tracing_opentelemetry::get_otel_context(id, &dispatch)
                    .map(|context| context.span().span_context().clone())
                    .filter(|span_context| span_context.is_valid())
            })
        });

        if let Some(span_context) = ids
            && let Some(close) = line.rfind('}')
        {
            // `format!` of a `TraceId`/`SpanId` is lower-case hex, the same
            // form W3C traceparent and Tempo use.
            line.insert_str(
                close,
                &format!(
                    r#","trace_id":"{}","span_id":"{}""#,
                    span_context.trace_id(),
                    span_context.span_id()
                ),
            );
        }
        writer.write_str(&line)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Running telemetry. Returned by [`init`]; `main` keeps it for the life of
/// the process and calls [`Telemetry::shutdown`] last.
pub struct Telemetry {
    providers: Providers,
    log_level: LogLevel,
    /// `deployment.environment.name`, when telemetry is on — cross-checked
    /// against `observability.environment` by [`Telemetry::check_environment`].
    environment: Option<String>,
}

/// Initialise telemetry from the `OTEL_*` environment variables. Call once,
/// first thing inside the async runtime, before config is loaded.
///
/// Fails on an invalid variable set — never on an unreachable collector.
pub fn init() -> Result<Telemetry, TelemetryError> {
    // `vars_os` rather than `vars`: `vars` panics on a non-UTF-8 variable
    // anywhere in the environment. Non-UTF-8 entries are skipped (no `OTEL_*`
    // variable can legitimately be one).
    let variables = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)));
    let Prepared {
        telemetry,
        subscriber,
        decision,
    } = prepare(variables.collect(), std::io::stdout)?;

    subscriber
        .try_init()
        .map_err(|_| TelemetryError::AlreadyInitialised)?;

    // Metrics: the global meter provider is set *before* the instruments are
    // created, because an instrument bound to the no-op provider stays no-op.
    if let Some(meter) = &telemetry.providers.meter {
        global::set_meter_provider(meter.clone());
        metrics::install(&global::meter(SCOPE));
    }

    announce(&decision);
    Ok(telemetry)
}

/// Everything [`init`] builds before it installs anything process-global.
struct Prepared<S> {
    telemetry: Telemetry,
    subscriber: S,
    decision: Decision,
}

/// Validate `variables`, build the providers the decision calls for, and the
/// subscriber over them — without installing either. [`init`] installs them;
/// the tests run the same function over a variable list and a captured writer,
/// so what they exercise is the startup path itself.
///
/// Takes the variables as an owned list (rather than any iterator) so the
/// returned subscriber's type cannot borrow from the caller's list.
fn prepare<W>(
    variables: Vec<(String, String)>,
    writer: W,
) -> Result<Prepared<impl Subscriber + Send + Sync + for<'a> LookupSpan<'a> + use<W>>, TelemetryError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    // The W3C propagator is installed whatever happens next: extraction and
    // injection run even with telemetry off. (Idempotent, and the same
    // propagator every time, so the tests may call this repeatedly.)
    install_propagator();

    let decision = settings::decide(variables)?;
    let (providers, environment) = match &decision {
        Decision::On(enabled) => (Providers::otlp(enabled)?, Some(enabled.environment.clone())),
        Decision::Off(_) => (Providers::default(), None),
    };
    let (subscriber, log_level) = subscriber(&providers, writer, INITIAL_LOG_LEVEL);
    Ok(Prepared {
        telemetry: Telemetry {
            providers,
            log_level,
            environment,
        },
        subscriber,
        decision,
    })
}

/// The one startup line saying what telemetry is doing — so "why are there no
/// spans?" has an answer in `docker logs`. The endpoint is named (it is an
/// internal address, not a secret); headers never are.
fn announce(decision: &Decision) {
    match decision {
        Decision::Off(reason) => {
            tracing::info!(reason = reason.describe(), "telemetry off");
        }
        Decision::On(enabled) => {
            let signals: Vec<&str> = Signal::ALL
                .into_iter()
                .filter(|signal| enabled.exports(*signal))
                .map(Signal::name)
                .collect();
            tracing::info!(
                signals = %signals.join(","),
                endpoint = enabled.endpoint.as_deref().unwrap_or("per-signal"),
                service_name = %enabled.service_name,
                environment = %enabled.environment,
                "telemetry on (OTLP http/protobuf)"
            );
        }
    }
}

/// Install the W3C trace-context propagator as the global one. Idempotent.
fn install_propagator() {
    global::set_text_map_propagator(TraceContextPropagator::new());
}

impl Telemetry {
    /// Apply `observability.log-level` to the fmt and log-bridge layers. Never
    /// to spans.
    pub fn set_log_level(&self, directives: &str) {
        self.log_level.set(directives);
    }

    /// Fail startup if the configured environment and the resource's
    /// `deployment.environment.name` disagree.
    ///
    /// They are the one fact stated twice: `observability.environment` feeds
    /// `/api/info` and the board watermark and must work with telemetry off;
    /// the resource attribute identifies every exported signal. The deploy
    /// derives both from its `APP_ENV`, and this check keeps a hand edit of
    /// either from splitting one deployment across two names. With telemetry
    /// off there is no resource, and nothing to compare.
    pub fn check_environment(&self, configured: &str) -> Result<(), TelemetryError> {
        match &self.environment {
            Some(resource) if resource != configured => Err(TelemetryError::EnvironmentMismatch {
                configured: configured.to_string(),
                resource: resource.clone(),
            }),
            _ => Ok(()),
        }
    }

    /// Flush and stop every provider, bounded by [`FLUSH_TIMEOUT`].
    ///
    /// Runs the SDK's blocking `shutdown` calls on blocking-pool threads: they
    /// wait for the exporter threads, whose HTTP requests run on *this*
    /// runtime (see [`OtlpHttpClient`]), so blocking a runtime thread here
    /// could starve the very requests being waited for.
    ///
    /// The three providers flush **concurrently**, each on its own thread and
    /// each under its own bound, so one slow signal cannot spend another's
    /// time. That matters because the metrics provider ignores its timeout
    /// argument in this SDK version (its final export is bounded only by the
    /// reader's own export timeout): run in sequence, a slow collector could
    /// make it use the whole budget and leave the logs — the shutdown's own
    /// records included — unflushed. An outer bound caps the total.
    pub async fn shutdown(self) {
        let Providers {
            tracer,
            logger,
            meter,
        } = self.providers;

        // One blocking task per provider present. Each returns the signal's
        // name and, on failure, the SDK's error text (which names the failure
        // kind, never an exported value).
        let mut flushes = Vec::new();
        if let Some(tracer) = tracer {
            flushes.push(tokio::task::spawn_blocking(move || {
                ("traces", tracer.shutdown_with_timeout(FLUSH_TIMEOUT).err())
            }));
        }
        if let Some(logger) = logger {
            flushes.push(tokio::task::spawn_blocking(move || {
                ("logs", logger.shutdown_with_timeout(FLUSH_TIMEOUT).err())
            }));
        }
        if let Some(meter) = meter {
            flushes.push(tokio::task::spawn_blocking(move || {
                ("metrics", meter.shutdown_with_timeout(FLUSH_TIMEOUT).err())
            }));
        }
        if flushes.is_empty() {
            return;
        }

        // `join_all` waits for every task; the timeout caps the wait for all
        // of them together at twice one provider's bound.
        match tokio::time::timeout(FLUSH_TIMEOUT * 2, futures_util::future::join_all(flushes)).await
        {
            Ok(results) => {
                for result in results {
                    match result {
                        Ok((_, None)) => {}
                        Ok((signal, Some(error))) => {
                            tracing::warn!(signal, error = %error, "telemetry flush at shutdown failed");
                        }
                        Err(join_error) => {
                            tracing::warn!(error = %join_error, "telemetry flush at shutdown panicked");
                        }
                    }
                }
            }
            Err(_) => tracing::warn!("telemetry flush at shutdown timed out"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Propagation helpers for the transport layer
// ─────────────────────────────────────────────────────────────────────────────

/// Make `span` a child of the trace context carried by `headers`
/// (`traceparent`), if there is one. Called by the server-span factory, so a
/// request's spans join the trace Traefik started at the edge.
pub(crate) fn adopt_parent(span: &tracing::Span, headers: &http::HeaderMap) {
    let parent =
        global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)));
    // Only adopt a context that actually carries a remote span: setting an
    // empty context would be harmless but pointless work.
    if parent.span().span_context().is_valid() {
        // `set_parent` fails only if the span was already started with a
        // different parent, which cannot happen for a span just created.
        let _ = span.set_parent(parent);
    }
}

/// Write the current span's context into outbound request `headers`
/// (`traceparent`), so the service called joins this trace.
///
/// Takes the context from the current *tracing* span, not from the SDK's own
/// "current context", which is empty unless the span layer has activated it —
/// and an empty context injects nothing, without an error (rust.md).
pub(crate) fn inject_current(headers: &mut http::HeaderMap) {
    let context = tracing::Span::current().context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut HeaderInjector(headers));
    });
}

/// Link `span` to `from` — for detached work, which starts its own trace
/// rather than being a child of the request that queued it (a child would keep
/// the request's trace open for as long as the work runs).
pub(crate) fn link_to(span: &tracing::Span, from: &tracing::Span) {
    let linked = from.context().span().span_context().clone();
    if linked.is_valid() {
        span.add_link(linked);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test support
// ─────────────────────────────────────────────────────────────────────────────

/// Test pipelines: the same provider builders and subscriber as production,
/// over exporters the tests can read back.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::logs::InMemoryLogExporter;
    use opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::ResourceMetrics;
    use opentelemetry_sdk::trace::{SpanData, SpanExporter};
    use std::sync::{Arc, Mutex, OnceLock};

    /// The variable set a dev deployment's `otel` layer renders, with an
    /// endpoint nothing resolves. Used to build the test resource through the
    /// production path (`settings::decide` → [`resource`]).
    pub(crate) const TEST_VARIABLES: [(&str, &str); 3] = [
        ("OTEL_SERVICE_NAME", "bored"),
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "deployment.environment.name=test,telemetry_source=otlp",
        ),
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://collector.invalid:4318",
        ),
    ];

    pub(crate) fn test_resource() -> Resource {
        let Decision::On(enabled) = settings::decide(TEST_VARIABLES).expect("valid test set")
        else {
            panic!("the test variable set turns telemetry on")
        };
        resource(&enabled)
    }

    /// A span exporter that keeps what it received — including after
    /// shutdown, unlike the SDK's in-memory one — and the resource the SDK
    /// handed it, which is what an OTLP exporter would put on the wire.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct RecordingSpanExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
        resource: Arc<Mutex<Option<Resource>>>,
    }

    impl RecordingSpanExporter {
        pub(crate) fn spans(&self) -> Vec<SpanData> {
            self.spans.lock().expect("span lock").clone()
        }

        pub(crate) fn resource(&self) -> Option<Resource> {
            self.resource.lock().expect("resource lock").clone()
        }
    }

    impl SpanExporter for RecordingSpanExporter {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.spans.lock().expect("span lock").extend(batch);
            Ok(())
        }

        fn set_resource(&mut self, resource: &Resource) {
            *self.resource.lock().expect("resource lock") = Some(resource.clone());
        }
    }

    /// The meter every instrument in the test binary records into. Built once:
    /// `metrics::instruments()` calls [`meter`] under `cfg(test)`, so router
    /// tests and metric tests all feed one in-memory exporter.
    struct TestMeter {
        provider: SdkMeterProvider,
        exporter: InMemoryMetricExporter,
    }

    fn test_meter() -> &'static TestMeter {
        static METER: OnceLock<TestMeter> = OnceLock::new();
        METER.get_or_init(|| {
            let exporter = InMemoryMetricExporter::default();
            let provider = meter_provider(exporter.clone(), test_resource());
            TestMeter { provider, exporter }
        })
    }

    pub(crate) fn meter() -> opentelemetry::metrics::Meter {
        use opentelemetry::metrics::MeterProvider as _;
        test_meter().provider.meter(SCOPE)
    }

    /// Force a collection and return the latest (cumulative) export.
    pub(crate) fn collect_metrics() -> ResourceMetrics {
        super::metrics::ensure_installed();
        let meter = test_meter();
        meter.provider.force_flush().expect("metric flush");
        meter
            .exporter
            .get_finished_metrics()
            .expect("in-memory metrics")
            .pop()
            .expect("at least one collection")
    }

    /// The current cumulative value of a `u64` counter's series whose
    /// attribute `key` is `value` — for tests outside this module, which must
    /// not name SDK types themselves.
    pub(crate) fn counter_value(metric: &str, key: &str, value: &str) -> u64 {
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        let collected = collect_metrics();
        let mut total = 0;
        for scope in collected.scope_metrics() {
            for found in scope.metrics().filter(|found| found.name() == metric) {
                if let AggregatedMetrics::U64(MetricData::Sum(sum)) = found.data() {
                    for point in sum.data_points() {
                        if point
                            .attributes()
                            .any(|kv| kv.key.as_str() == key && kv.value.to_string() == value)
                        {
                            total += point.value();
                        }
                    }
                }
            }
        }
        total
    }

    /// The current value of a sum instrument (counter or up-down counter,
    /// `u64` or `i64`) with no attributes, summed over its points.
    pub(crate) fn sum_value(metric: &str) -> i64 {
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        let collected = collect_metrics();
        let mut total: i64 = 0;
        for scope in collected.scope_metrics() {
            for found in scope.metrics().filter(|found| found.name() == metric) {
                match found.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                        total += sum
                            .data_points()
                            .map(|point| i64::try_from(point.value()).unwrap_or(i64::MAX))
                            .sum::<i64>();
                    }
                    AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                        total += sum.data_points().map(|point| point.value()).sum::<i64>();
                    }
                    _ => {}
                }
            }
        }
        total
    }

    /// Traces and logs into readable exporters, through the production
    /// [`subscriber`]. (Metrics go through the shared [`meter`] above.)
    pub(crate) struct Pipeline {
        pub(crate) spans: RecordingSpanExporter,
        pub(crate) logs: InMemoryLogExporter,
        pub(crate) telemetry: Telemetry,
    }

    impl Pipeline {
        /// Build it at log level `directives`, returning the subscriber to run
        /// it under (`tracing::subscriber::set_default`).
        pub(crate) fn new<W>(
            writer: W,
            directives: &str,
        ) -> (
            Self,
            impl Subscriber + Send + Sync + for<'a> LookupSpan<'a> + use<W>,
        )
        where
            W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
        {
            install_propagator();
            let spans = RecordingSpanExporter::default();
            let logs = InMemoryLogExporter::default();
            let resource = test_resource();
            let providers = Providers {
                tracer: Some(tracer_provider(spans.clone(), resource.clone())),
                logger: Some(logger_provider(logs.clone(), resource)),
                meter: None,
            };
            let (subscriber, log_level) = subscriber(&providers, writer, directives);
            let pipeline = Pipeline {
                spans,
                logs,
                telemetry: Telemetry {
                    providers,
                    log_level,
                    environment: Some("test".to_string()),
                },
            };
            (pipeline, subscriber)
        }

        /// Push everything buffered in the batch processors to the exporters.
        pub(crate) fn flush(&self) {
            if let Some(tracer) = &self.telemetry.providers.tracer {
                tracer.force_flush().expect("span flush");
            }
            if let Some(logger) = &self.telemetry.providers.logger {
                logger.force_flush().expect("log flush");
            }
        }

        pub(crate) fn finished_spans(&self) -> Vec<SpanData> {
            self.flush();
            self.spans.spans()
        }

        pub(crate) fn finished_logs(&self) -> Vec<LogDataWithResource> {
            self.flush();
            self.logs.get_emitted_logs().expect("in-memory logs")
        }
    }

    /// Run the startup path (`prepare`) over `variables`, as `init` would, and
    /// return the pieces: the telemetry handle, the subscriber it built, and
    /// a closure that writes the startup line.
    #[allow(clippy::type_complexity)]
    pub(crate) fn prepare_for_test<W>(
        variables: &[(&str, &str)],
        writer: W,
    ) -> Result<
        (
            Telemetry,
            impl Subscriber + Send + Sync + for<'a> LookupSpan<'a> + use<W>,
            impl Fn() + use<W>,
        ),
        TelemetryError,
    >
    where
        W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
    {
        let owned = variables
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let prepared = prepare(owned, writer)?;
        let decision = prepared.decision;
        Ok((prepared.telemetry, prepared.subscriber, move || {
            announce(&decision)
        }))
    }

    /// Whether this telemetry has any provider (i.e. is on).
    pub(crate) fn has_providers(telemetry: &Telemetry) -> bool {
        let providers = &telemetry.providers;
        providers.tracer.is_some() || providers.logger.is_some() || providers.meter.is_some()
    }
}

#[cfg(test)]
mod tests;
