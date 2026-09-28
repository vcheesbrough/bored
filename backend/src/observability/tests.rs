//! The tests that protect the telemetry contract (the `observability` skill
//! §7, card #415 §6). **Do not weaken them.**
//!
//! Every assertion here is made on what an exporter *received* — the
//! recording span exporter, the SDK's in-memory log and metric exporters, the
//! captured stdout — never on configuration. A test that checked "the layer
//! was added" would pass against a pipeline that exports nothing.
//!
//! Most tests run the production subscriber under
//! `tracing::subscriber::set_default`, which is thread-local. They are
//! `#[tokio::test]`s, whose default runtime is single-threaded, so the router
//! and every task it spawns run on the thread that holds the subscriber.

use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue};
use axum_test::TestServer;
use opentelemetry::trace::SpanKind;
use opentelemetry::{Key, KeyValue, Value};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_semantic_conventions as semconv;

use super::metrics::{
    self, AUTH_OUTCOMES, AuthOutcome, BUILD_INFO, BUILD_INFO_REVISION, BUILD_INFO_VERSION,
    DB_ERRORS, DbErrorClass, ErrorType, HttpMethod, SSE_EVENTS, SSE_LAGGED, SSE_SUBSCRIBERS,
};
use super::test_support::{
    Pipeline, TEST_VARIABLES, collect_metrics, has_providers, prepare_for_test,
};
use super::{FLUSH_TIMEOUT, TelemetryError, adopt_parent, inject_current};
use crate::app::{DeploymentInfo, app};
use crate::routes::boards::AppState;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// A `MakeWriter` that appends everything written to a shared buffer, so a
/// test can read back exactly what would have gone to stdout.
#[derive(Clone, Default)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl CapturedWriter {
    fn text(&self) -> String {
        let bytes = self.0.lock().expect("writer mutex poisoned").clone();
        String::from_utf8(bytes).expect("log output should be UTF-8")
    }

    /// Every line, parsed as JSON. Panics on a line that is not JSON — which
    /// is itself one of the properties under test.
    fn json_lines(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("not JSON: {line}")))
            .collect()
    }

    /// The first line whose `message` is `message`.
    fn line(&self, message: &str) -> Option<serde_json::Value> {
        self.json_lines()
            .into_iter()
            .find(|line| line["message"] == message)
    }
}

impl io::Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("writer mutex poisoned").extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The whole router over an in-memory database — the same `app()` `main`
/// serves.
async fn router() -> axum::Router {
    let db = crate::db::connect_mem().await.expect("mem db");
    app(
        AppState::new(db),
        "./dist",
        DeploymentInfo::new("test", None),
    )
    .await
}

/// A W3C `traceparent` a caller (Traefik, in production) might send. The ids
/// are literals, so the assertions on them do not depend on any code under
/// test to compute an expected value.
const INBOUND_TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const INBOUND_SPAN_ID: &str = "00f067aa0ba902b7";

fn inbound_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "traceparent",
        HeaderValue::from_str(&format!("00-{INBOUND_TRACE_ID}-{INBOUND_SPAN_ID}-01")).unwrap(),
    );
    headers
}

/// A resource attribute as a string, if present.
fn resource_string(resource: &Resource, key: &'static str) -> Option<String> {
    resource
        .get(&Key::from_static_str(key))
        .map(|value| value.to_string())
}

/// The identity every signal must carry (skill §1.2).
fn assert_identity(resource: &Resource, signal: &str) {
    assert_eq!(
        resource_string(resource, semconv::resource::SERVICE_NAME).as_deref(),
        Some("bored"),
        "service.name on {signal}"
    );
    assert_eq!(
        resource_string(resource, semconv::resource::SERVICE_VERSION).as_deref(),
        Some(shared::app_version()),
        "service.version on {signal}"
    );
    assert_eq!(
        resource_string(resource, semconv::resource::DEPLOYMENT_ENVIRONMENT_NAME).as_deref(),
        Some("test"),
        "deployment.environment.name on {signal}"
    );
}

/// Every data point's attribute set, for every metric, whatever its value
/// type and kind. The SDK's data model is an enum per value type wrapping an
/// enum per kind, hence the macro to avoid writing the same match three times.
fn all_series(metrics: &ResourceMetrics) -> Vec<(String, Vec<KeyValue>)> {
    macro_rules! points {
        ($data:expr, $name:expr, $out:expr) => {
            match $data {
                MetricData::Gauge(gauge) => {
                    for point in gauge.data_points() {
                        $out.push(($name.clone(), point.attributes().cloned().collect()));
                    }
                }
                MetricData::Sum(sum) => {
                    for point in sum.data_points() {
                        $out.push(($name.clone(), point.attributes().cloned().collect()));
                    }
                }
                MetricData::Histogram(histogram) => {
                    for point in histogram.data_points() {
                        $out.push(($name.clone(), point.attributes().cloned().collect()));
                    }
                }
                MetricData::ExponentialHistogram(histogram) => {
                    for point in histogram.data_points() {
                        $out.push(($name.clone(), point.attributes().cloned().collect()));
                    }
                }
            }
        };
    }
    let mut series = Vec::new();
    for scope in metrics.scope_metrics() {
        for metric in scope.metrics() {
            let name = metric.name().to_string();
            match metric.data() {
                AggregatedMetrics::F64(data) => points!(data, name, series),
                AggregatedMetrics::U64(data) => points!(data, name, series),
                AggregatedMetrics::I64(data) => points!(data, name, series),
            }
        }
    }
    series
}

/// Run the startup path over `variables`, serve the real router on an
/// ephemeral port, make one request, send the shutdown signal and flush.
/// Returns the served response body, how long the stop took, and stdout.
async fn lifecycle(variables: &[(&str, &str)]) -> (String, Duration, CapturedWriter) {
    let writer = CapturedWriter::default();
    let (telemetry, subscriber, announce) =
        prepare_for_test(variables, writer.clone()).expect("startup should succeed");
    let _guard = tracing::subscriber::set_default(subscriber);
    announce();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::listen::serve_plain(
        listener,
        router().await,
        async move {
            let _ = stop_rx.await;
        },
    ));

    let body = reqwest::get(format!("http://{address}/health"))
        .await
        .expect("the server answers")
        .text()
        .await
        .expect("a body");

    let stopping = Instant::now();
    stop_tx.send(()).expect("server still running");
    server.await.expect("server task");
    telemetry.shutdown().await;
    (body, stopping.elapsed(), writer)
}

// ─────────────────────────────────────────────────────────────────────────────
// Off, disabled, and no collector (§7 bullet 1)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn with_no_otel_variables_it_serves_stops_and_says_telemetry_is_off_once() {
    let (body, stop_time, stdout) = lifecycle(&[]).await;
    assert_eq!(body, "ok");
    assert!(
        stop_time < Duration::from_secs(2),
        "stop took {stop_time:?}"
    );
    let off_lines: Vec<_> = stdout
        .json_lines()
        .into_iter()
        .filter(|line| line["message"] == "telemetry off")
        .collect();
    assert_eq!(
        off_lines.len(),
        1,
        "exactly one startup line: {}",
        stdout.text()
    );
    assert_eq!(off_lines[0]["reason"], "no OTEL_* variable is set");
}

#[tokio::test]
async fn with_sdk_disabled_it_serves_stops_and_builds_no_provider() {
    // A deployment that carries the variables and wants them silent.
    let mut variables = TEST_VARIABLES.to_vec();
    variables.push(("OTEL_SDK_DISABLED", "true"));
    let (telemetry, _subscriber, _announce) =
        prepare_for_test(&variables, CapturedWriter::default()).expect("valid");
    assert!(
        !has_providers(&telemetry),
        "disabled must build no provider"
    );

    let (body, stop_time, stdout) = lifecycle(&variables).await;
    assert_eq!(body, "ok");
    assert!(
        stop_time < Duration::from_secs(2),
        "stop took {stop_time:?}"
    );
    let line = stdout.line("telemetry off").expect("startup line");
    assert_eq!(line["reason"], "OTEL_SDK_DISABLED=true");
}

#[tokio::test]
async fn with_no_collector_reachable_it_serves_and_stops_in_bounded_time() {
    // A port nothing listens on: bind one, note it, let it go.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free port")
        .port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let variables = [
        TEST_VARIABLES[0],
        TEST_VARIABLES[1],
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
    ];

    // The *real* module: OTLP exporters over the real HTTP client.
    let (telemetry, _subscriber, _announce) =
        prepare_for_test(&variables, CapturedWriter::default()).expect("valid");
    assert!(
        has_providers(&telemetry),
        "telemetry must be on for this test"
    );
    drop(telemetry);

    let (body, stop_time, stdout) = lifecycle(&variables).await;
    // The product still serves…
    assert_eq!(body, "ok");
    // …and still stops, flush failures and all, inside its budget.
    let budget = crate::listen::DRAIN_TIMEOUT + FLUSH_TIMEOUT * 2;
    assert!(
        stop_time < budget,
        "stop took {stop_time:?}, budget {budget:?}"
    );
    assert!(
        stdout.line("telemetry on (OTLP http/protobuf)").is_some(),
        "{}",
        stdout.text()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Validation through the startup path (§7: half-present fails)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_half_present_set_fails_startup_and_never_reaches_localhost() {
    let result = prepare_for_test(&[("OTEL_SERVICE_NAME", "bored")], CapturedWriter::default());
    match result {
        Err(TelemetryError::Settings(error)) => {
            assert_eq!(error.variable, "OTEL_EXPORTER_OTLP_ENDPOINT")
        }
        Err(other) => panic!("wrong failure: {other}"),
        Ok(_) => panic!("a half-present set must fail startup"),
    }
}

#[tokio::test]
async fn a_malformed_protocol_fails_startup() {
    let mut variables = TEST_VARIABLES.to_vec();
    variables.push(("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"));
    assert!(matches!(
        prepare_for_test(&variables, CapturedWriter::default()),
        Err(TelemetryError::Settings(_))
    ));
}

#[test]
fn the_environment_must_agree_with_the_resource() {
    let (pipeline, _subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    assert!(pipeline.telemetry.check_environment("test").is_ok());
    assert!(matches!(
        pipeline.telemetry.check_environment("prod"),
        Err(TelemetryError::EnvironmentMismatch { .. })
    ));
}

#[tokio::test]
async fn with_telemetry_off_any_environment_is_accepted() {
    let (telemetry, _subscriber, _announce) =
        prepare_for_test(&[], CapturedWriter::default()).expect("valid");
    assert!(telemetry.check_environment("anything").is_ok());
}

// ─────────────────────────────────────────────────────────────────────────────
// Traces (§7 bullets 2, 3, 7, 8)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_request_with_traceparent_yields_a_server_span_parented_on_it() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = TestServer::new(router().await).unwrap();

    server
        .get("/api/boards")
        .add_header(
            "traceparent",
            format!("00-{INBOUND_TRACE_ID}-{INBOUND_SPAN_ID}-01"),
        )
        .await
        .assert_status_ok();

    let spans = pipeline.finished_spans();
    let server_span = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Server)
        .unwrap_or_else(|| {
            panic!(
                "no server span among {:?}",
                spans.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        server_span.span_context.trace_id().to_string(),
        INBOUND_TRACE_ID
    );
    assert_eq!(server_span.parent_span_id.to_string(), INBOUND_SPAN_ID);
    assert!(
        server_span.parent_span_is_remote,
        "the parent came over the wire"
    );
    assert_eq!(server_span.name, "GET /api/boards");

    let attribute = |key: &str| {
        server_span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.clone())
    };
    // The route *template*, and the status recorded after the handler ran.
    assert_eq!(
        attribute(semconv::attribute::HTTP_ROUTE),
        Some(Value::from("/api/boards"))
    );
    assert_eq!(
        attribute(semconv::attribute::HTTP_RESPONSE_STATUS_CODE),
        Some(Value::I64(200))
    );
    assert_eq!(
        attribute(semconv::attribute::HTTP_REQUEST_METHOD),
        Some(Value::from("GET"))
    );
}

#[tokio::test]
async fn a_request_without_traceparent_starts_its_own_trace() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = TestServer::new(router().await).unwrap();
    server.get("/health").await.assert_status_ok();

    let spans = pipeline.finished_spans();
    let server_span = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Server)
        .expect("server span");
    assert!(server_span.span_context.is_valid());
    assert_ne!(
        server_span.span_context.trace_id().to_string(),
        INBOUND_TRACE_ID
    );
    assert!(!server_span.parent_span_is_remote);
    // `/health` is a route like any other: templated name, no raw path in it.
    assert_eq!(server_span.name, "GET /health");
}

#[tokio::test]
async fn a_client_error_carries_no_error_type_and_a_templated_name() {
    // A 404 is the client's problem, not a failure of this service, so the
    // server span must carry no `error.type` (semconv) — and its name must be
    // the route template even though the path named a concrete board.
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = TestServer::new(router().await).unwrap();
    server
        .get("/api/boards/no-such-board")
        .await
        .assert_status_not_found();

    let spans = pipeline.finished_spans();
    let server_span = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Server)
        .expect("server span");
    assert!(
        !server_span
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == semconv::attribute::ERROR_TYPE),
        "a 404 is not an error of this service"
    );
    assert_eq!(
        server_span.name, "GET /api/boards/:slug",
        "template, not the raw path"
    );
}

#[tokio::test]
async fn a_server_error_marks_the_span_and_the_metric_with_error_type() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let db = crate::db::connect_mem().await.expect("mem db");
    let server = TestServer::new(
        app(
            AppState::new(db.clone()),
            "./dist",
            DeploymentInfo::new("test", None),
        )
        .await,
    )
    .unwrap();
    let board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "broken".to_string(),
        })
        .await
        .json();
    let column: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "c".to_string(),
            position: 0,
        })
        .await
        .json();
    server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "x".to_string(),
            ..Default::default()
        })
        .await;
    // Make the stored card unreadable (the same fault `tests/errors.rs` uses),
    // so listing the column fails inside the driver: a genuine 500.
    db.query("REMOVE FIELD body ON TABLE cards")
        .await
        .unwrap()
        .check()
        .unwrap();
    db.query("UPDATE cards SET body = NONE")
        .await
        .unwrap()
        .check()
        .unwrap();
    server
        .get(&format!("/api/columns/{}/cards", column.id))
        .await
        .assert_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR);

    let spans = pipeline.finished_spans();
    let failed = spans
        .iter()
        .find(|span| {
            span.span_kind == SpanKind::Server && span.name == "GET /api/columns/:id/cards"
        })
        .expect("server span for the failing request");
    let error_type = failed
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == semconv::attribute::ERROR_TYPE)
        .map(|kv| kv.value.to_string());
    assert_eq!(
        error_type.as_deref(),
        Some(ErrorType::InternalServerError.label())
    );
    assert_eq!(failed.status, opentelemetry::trace::Status::error(""));

    // And the duration metric has a series for it carrying the same class.
    let series = all_series(&collect_metrics());
    assert!(
        series.iter().any(|(name, attributes)| {
            name == semconv::metric::HTTP_SERVER_REQUEST_DURATION
                && attributes.iter().any(|kv| {
                    kv.key.as_str() == semconv::attribute::ERROR_TYPE
                        && kv.value.to_string() == ErrorType::InternalServerError.label()
                })
        }),
        "no duration series with error.type=500"
    );
}

/// Span attribute keys the span bridge (`tracing-opentelemetry`) adds that
/// are not in the semantic-conventions crate. `code.module.name` comes with
/// the location option (the code.file.path/line.number pair is semconv); it is
/// recorded as a deviation in AGENTS.md § Observability.
const BRIDGE_KEYS: &[&str] = &["code.module.name"];

/// Every semantic-convention key bored's spans may carry. Built from the
/// crate's constants, so a typo at a call site (which the `tracing` macros
/// force to be a literal) is caught here, on what the exporter received.
fn semconv_span_keys() -> BTreeSet<&'static str> {
    use semconv::attribute::*;
    BTreeSet::from([
        HTTP_REQUEST_METHOD,
        HTTP_ROUTE,
        HTTP_RESPONSE_STATUS_CODE,
        ERROR_TYPE,
        URL_PATH,
        URL_SCHEME,
        CODE_FILE_PATH,
        CODE_LINE_NUMBER,
        THREAD_ID,
        THREAD_NAME,
        DB_SYSTEM_NAME,
        DB_OPERATION_NAME,
        DB_QUERY_SUMMARY,
        DB_NAMESPACE,
        SERVER_ADDRESS,
        URL_FULL,
    ])
}

#[tokio::test]
async fn every_exported_span_attribute_key_is_semconv_or_bored_prefixed() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = TestServer::new(router().await).unwrap();

    // A spread of routes: create, read, list, a 404, a fallback path.
    let board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "keys".to_string(),
        })
        .await
        .json();
    server
        .get(&format!("/api/boards/{}", board.name))
        .await
        .assert_status_ok();
    server.get("/api/boards").await.assert_status_ok();
    server
        .get("/api/boards/missing")
        .await
        .assert_status_not_found();
    server.get("/health").await.assert_status_ok();
    let _ = server.get("/some/spa/deep-link").await;

    let spans = pipeline.finished_spans();
    let allowed = semconv_span_keys();
    let mut seen = BTreeSet::new();
    for span in &spans {
        for kv in &span.attributes {
            let key = kv.key.as_str().to_string();
            assert!(
                allowed.contains(key.as_str())
                    || key.starts_with("bored.")
                    || BRIDGE_KEYS.contains(&key.as_str()),
                "span `{}` carries attribute `{key}`, which is neither a semantic-convention \
                 name nor `bored.`-prefixed",
                span.name
            );
            seen.insert(key);
        }
    }
    // Non-degenerate: the check above passes vacuously on spans with no
    // attributes, so require that the interesting ones were actually seen.
    for expected in [
        semconv::attribute::HTTP_ROUTE,
        semconv::attribute::HTTP_RESPONSE_STATUS_CODE,
    ] {
        assert!(
            seen.contains(expected),
            "expected `{expected}` among {seen:?}"
        );
    }
}

#[tokio::test]
async fn shutdown_flushes_pending_spans_within_its_timeout() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info_span!("pending work").in_scope(|| {});
    }
    // Nothing flushed yet: the batch processor holds the span for its
    // scheduled delay (5 s by default), far longer than this test waits.
    let exporter = pipeline.spans.clone();
    assert!(
        exporter.spans().is_empty(),
        "precondition: the span is still pending"
    );

    let started = Instant::now();
    pipeline.telemetry.shutdown().await;
    let took = started.elapsed();

    assert!(took < FLUSH_TIMEOUT, "shutdown took {took:?}");
    assert!(
        exporter
            .spans()
            .iter()
            .any(|span| span.name == "pending work"),
        "shutdown must flush the pending span"
    );
}

#[test]
fn the_log_level_never_filters_spans() {
    // `warn` would drop an `info` span if the level filter sat on the whole
    // subscriber — the trap the pre-#415 module's comment warned about.
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "warn");
    tracing::subscriber::with_default(subscriber, || {
        tracing::info_span!("info span").in_scope(|| {});
        tracing::debug_span!("debug span").in_scope(|| {});
    });
    let names: Vec<_> = pipeline
        .finished_spans()
        .into_iter()
        .map(|span| span.name)
        .collect();
    assert!(names.iter().any(|name| name == "info span"), "{names:?}");
    assert!(names.iter().any(|name| name == "debug span"), "{names:?}");
}

#[test]
fn events_are_not_recorded_twice_as_span_events() {
    // One fact, one signal: an event inside a span becomes a log record, not
    // also a span event.
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    tracing::subscriber::with_default(subscriber, || {
        tracing::info_span!("holder").in_scope(|| tracing::info!("once only"));
    });
    let spans = pipeline.finished_spans();
    let holder = spans
        .iter()
        .find(|span| span.name == "holder")
        .expect("span");
    assert_eq!(holder.events.len(), 0, "span events: {:?}", holder.events);
    assert!(
        pipeline
            .finished_logs()
            .iter()
            .any(|log| log_body(log) == "once only")
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Resource identity on every signal (§7 bullet 3)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn service_identity_is_on_exported_spans_logs_and_metrics() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info_span!("identity").in_scope(|| tracing::info!("identity log"));
        metrics::sse_event_delivered();
    }

    assert!(!pipeline.finished_spans().is_empty());
    let span_resource = pipeline
        .spans
        .resource()
        .expect("the SDK hands the exporter a resource");
    assert_identity(&span_resource, "spans");

    let logs = pipeline.finished_logs();
    let log = logs
        .iter()
        .find(|log| log_body(log) == "identity log")
        .expect("log exported");
    assert_identity(&log.resource, "logs");

    assert_identity(collect_metrics().resource(), "metrics");
}

#[test]
fn the_deployment_cannot_override_the_version_and_the_instance_id_is_stable() {
    use super::settings::{Decision, decide};
    let variables = [
        ("OTEL_SERVICE_NAME", "bored"),
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "deployment.environment.name=dev,service.version=9.9.9",
        ),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318"),
    ];
    let Decision::On(enabled) = decide(variables).unwrap() else {
        panic!("on")
    };
    let first = super::resource(&enabled);
    let second = super::resource(&enabled);
    assert_eq!(
        resource_string(&first, semconv::resource::SERVICE_VERSION).as_deref(),
        Some(shared::app_version())
    );
    // Same host, same id — not a random value per start.
    let instance = resource_string(&first, semconv::resource::SERVICE_INSTANCE_ID);
    assert_eq!(
        instance,
        resource_string(&second, semconv::resource::SERVICE_INSTANCE_ID)
    );
    if let Some(instance) = instance {
        // A v5 UUID, which does not publish the host name.
        let parsed = uuid::Uuid::parse_str(&instance).expect("a UUID");
        assert_eq!(parsed.get_version_num(), 5);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Logs (§7 bullet 4, and the layer filters)
// ─────────────────────────────────────────────────────────────────────────────

/// A log record's body as text.
fn log_body(log: &opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource) -> String {
    match log.record.body() {
        Some(opentelemetry::logs::AnyValue::String(text)) => text.as_str().to_string(),
        other => format!("{other:?}"),
    }
}

#[test]
fn a_log_inside_a_span_carries_its_ids_on_the_otlp_record_and_in_stdout() {
    let stdout = CapturedWriter::default();
    let (pipeline, subscriber) = Pipeline::new(stdout.clone(), "info");
    tracing::subscriber::with_default(subscriber, || {
        tracing::info_span!("correlated").in_scope(|| tracing::info!("inside the span"));
        tracing::info!("outside any span");
    });

    // The span's own ids, as the span exporter received them — the oracle.
    let spans = pipeline.finished_spans();
    let span = spans
        .iter()
        .find(|span| span.name == "correlated")
        .expect("span");
    let trace_id = span.span_context.trace_id();
    let span_id = span.span_context.span_id();

    // On the OTLP log record…
    let logs = pipeline.finished_logs();
    let record = logs
        .iter()
        .find(|log| log_body(log) == "inside the span")
        .expect("record");
    let context = record
        .record
        .trace_context()
        .expect("trace context on the record");
    assert_eq!(context.trace_id, trace_id);
    assert_eq!(context.span_id, span_id);

    // …and on the stdout JSON line.
    let line = stdout.line("inside the span").expect("stdout line");
    assert_eq!(line["trace_id"], trace_id.to_string());
    assert_eq!(line["span_id"], span_id.to_string());

    // A line outside every span has neither.
    let outside = stdout.line("outside any span").expect("stdout line");
    assert!(outside.get("trace_id").is_none(), "{outside}");
}

#[test]
fn an_event_is_written_as_exactly_one_line_of_json() {
    // Alloy turns each *physical line* of stdout into one Loki entry, so an
    // event that spans several lines arrives as several unrelated entries.
    let stdout = CapturedWriter::default();
    let (_pipeline, subscriber) = Pipeline::new(stdout.clone(), "info");
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(card = 412, "logs reach loki via stdout");
    });
    let lines: Vec<String> = stdout.text().lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 1, "expected a single line, got: {lines:?}");
    let entry: serde_json::Value = serde_json::from_str(&lines[0]).expect("valid JSON");
    // `flatten_event` puts the message and fields at the top level, which is
    // what makes them addressable in a Loki `| json` query.
    assert_eq!(entry["message"], "logs reach loki via stdout");
    assert_eq!(entry["card"], 412);
    assert_eq!(entry["level"], "INFO");
}

#[test]
fn events_below_the_configured_level_are_suppressed_on_stdout_and_otlp() {
    let stdout = CapturedWriter::default();
    let (pipeline, subscriber) = Pipeline::new(stdout.clone(), "info");
    // Narrow the level *after* building, as `main` does once config loads —
    // so this also proves the reload reaches both layers.
    pipeline.telemetry.set_log_level("warn");
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("should not appear");
        tracing::warn!("should appear");
    });
    let text = stdout.text();
    assert!(
        !text.contains("should not appear"),
        "info survived a warn filter: {text}"
    );
    assert!(text.contains("should appear"), "warn was dropped: {text}");
    let bodies: Vec<String> = pipeline.finished_logs().iter().map(log_body).collect();
    assert!(
        !bodies.iter().any(|body| body == "should not appear"),
        "{bodies:?}"
    );
    assert!(
        bodies.iter().any(|body| body == "should appear"),
        "{bodies:?}"
    );
}

#[test]
fn the_sdks_own_events_stay_at_warn_on_stdout_and_never_reach_the_bridge() {
    let stdout = CapturedWriter::default();
    let (pipeline, subscriber) = Pipeline::new(stdout.clone(), "debug");
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "opentelemetry_sdk", "sdk chatter");
        tracing::warn!(target: "opentelemetry_sdk", "sdk export failed");
        tracing::debug!(target: "hyper_util::client", "exporter connection detail");
        tracing::info!("a product line");
    });
    let text = stdout.text();
    assert!(!text.contains("sdk chatter"), "{text}");
    assert!(
        text.contains("sdk export failed"),
        "the failure signal must stay visible: {text}"
    );

    let bodies: Vec<String> = pipeline.finished_logs().iter().map(log_body).collect();
    assert!(
        bodies.iter().any(|body| body == "a product line"),
        "{bodies:?}"
    );
    assert!(
        !bodies.iter().any(|body| body == "sdk export failed"),
        "{bodies:?}"
    );
    assert!(
        !bodies
            .iter()
            .any(|body| body == "exporter connection detail"),
        "{bodies:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Propagation with telemetry off (skill §2: off never touches propagation)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn with_telemetry_off_inbound_context_still_reaches_outbound_headers_and_logs() {
    let stdout = CapturedWriter::default();
    let (_telemetry, subscriber, _announce) = prepare_for_test(&[], stdout.clone()).expect("off");
    let _guard = tracing::subscriber::set_default(subscriber);

    let span = tracing::info_span!("incoming");
    adopt_parent(&span, &inbound_headers());
    let outbound = span.in_scope(|| {
        tracing::info!("passing through");
        let mut headers = HeaderMap::new();
        inject_current(&mut headers);
        headers
    });

    let traceparent = outbound
        .get("traceparent")
        .expect("a traceparent is injected even with telemetry off")
        .to_str()
        .unwrap()
        .to_string();
    assert!(traceparent.contains(INBOUND_TRACE_ID), "{traceparent}");
    let line = stdout.line("passing through").expect("line");
    assert_eq!(line["trace_id"], INBOUND_TRACE_ID);
}

// ─────────────────────────────────────────────────────────────────────────────
// Metrics (§7 bullets 5, 6)
// ─────────────────────────────────────────────────────────────────────────────

/// Keys that must never appear on any metric series: identifiers and raw
/// paths. Listed by *key*, not by metric, so a new metric is covered without
/// editing this test.
const FORBIDDEN_METRIC_KEYS: &[&str] = &[
    "url.path",
    "url.full",
    "url.query",
    "http.target",
    "http.url",
    "enduser.id",
    "user.id",
    "session.id",
    "trace_id",
    "span_id",
    "bored.board.id",
    "bored.column.id",
    "bored.card.id",
    "bored.link.id",
    "bored.user.id",
    "board_id",
    "card_id",
    "column_id",
    "link_id",
    "service.version",
    "service.instance.id",
];

/// Whether a key looks like an identifier by shape (`*.id`, `*_id`, `id`).
fn looks_like_an_identifier(key: &str) -> bool {
    key == "id" || key.ends_with(".id") || key.ends_with("_id")
}

#[tokio::test]
async fn no_exported_metric_series_carries_a_forbidden_label() {
    // Exercise every instrument: requests through the real router (which
    // records the duration histogram), and each typed handle for the rest,
    // over every variant of every label enum.
    let server = TestServer::new(router().await).unwrap();
    let board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "labels".to_string(),
        })
        .await
        .json();
    server
        .get(&format!("/api/boards/{}", board.name))
        .await
        .assert_status_ok();
    server
        .get("/api/boards/missing")
        .await
        .assert_status_not_found();
    let _ = server.get("/deep/link/for/the/spa").await;
    drop(metrics::SseSubscription::new());
    metrics::sse_event_delivered();
    metrics::sse_lagged(3);
    for class in DbErrorClass::ALL {
        metrics::db_error(class);
    }
    for outcome in AuthOutcome::ALL {
        metrics::auth_outcome(outcome);
    }

    let collected = collect_metrics();
    let series = all_series(&collected);
    let names: BTreeSet<&str> = series.iter().map(|(name, _)| name.as_str()).collect();
    // Non-degenerate: every instrument must actually be in the export, or the
    // key check below proves nothing about it.
    for expected in [
        semconv::metric::HTTP_SERVER_REQUEST_DURATION,
        SSE_SUBSCRIBERS,
        SSE_EVENTS,
        SSE_LAGGED,
        DB_ERRORS,
        AUTH_OUTCOMES,
        BUILD_INFO,
    ] {
        assert!(
            names.contains(expected),
            "`{expected}` missing from {names:?}"
        );
    }

    for (name, attributes) in &series {
        for kv in attributes {
            let key = kv.key.as_str();
            assert!(
                !FORBIDDEN_METRIC_KEYS.contains(&key),
                "`{name}` carries forbidden label `{key}`"
            );
            assert!(
                !looks_like_an_identifier(key),
                "`{name}` carries identifier-shaped label `{key}`"
            );
            // Build identity appears on the info metric and nowhere else.
            if key == BUILD_INFO_VERSION || key == BUILD_INFO_REVISION {
                assert_eq!(name, BUILD_INFO, "`{name}` carries build identity `{key}`");
            }
            // A route label is always a template, never a concrete path.
            if key == semconv::attribute::HTTP_ROUTE {
                let route = kv.value.to_string();
                assert!(
                    !route.contains("labels"),
                    "`{name}` carries a raw path: {route}"
                );
            }
        }
    }
}

#[test]
fn build_info_carries_version_and_revision_with_value_one() {
    let collected = collect_metrics();
    let (_, attributes) = all_series(&collected)
        .into_iter()
        .find(|(name, _)| name == BUILD_INFO)
        .expect("bored.build.info exported");
    let value = |key: &str| {
        attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    assert_eq!(
        value(BUILD_INFO_VERSION).as_deref(),
        Some(shared::app_version())
    );
    assert_eq!(
        value(BUILD_INFO_REVISION).as_deref(),
        Some(metrics::build_revision())
    );
}

#[test]
fn every_label_enum_has_a_distinct_label_for_every_variant() {
    // Iterate `ALL` rather than listing cases, so a new variant is covered
    // without editing this test (and `label`'s exhaustive match means it
    // cannot compile without a label).
    fn check<T: Copy + std::fmt::Debug>(all: &[T], label: fn(T) -> &'static str) {
        let mut seen = BTreeSet::new();
        for variant in all {
            let text = label(*variant);
            assert!(!text.is_empty(), "{variant:?} has an empty label");
            assert!(seen.insert(text), "{variant:?} repeats label `{text}`");
        }
    }
    check(&HttpMethod::ALL, HttpMethod::label);
    check(&ErrorType::ALL, ErrorType::label);
    check(&DbErrorClass::ALL, DbErrorClass::label);
    check(&AuthOutcome::ALL, AuthOutcome::label);

    // The method fold: every known method maps to itself, anything else to
    // `_OTHER` — a client cannot mint a new label value.
    for method in HttpMethod::ALL {
        if method == HttpMethod::Other {
            continue;
        }
        let parsed = axum::http::Method::from_bytes(method.label().as_bytes()).unwrap();
        assert_eq!(HttpMethod::from_method(&parsed), method);
    }
    let invented = axum::http::Method::from_bytes(b"BREW").unwrap();
    assert_eq!(HttpMethod::from_method(&invented).label(), "_OTHER");

    // error.type: only server errors carry one.
    assert_eq!(ErrorType::from_status(200), None);
    assert_eq!(ErrorType::from_status(404), None);
    assert_eq!(
        ErrorType::from_status(500),
        Some(ErrorType::InternalServerError)
    );
    assert_eq!(
        ErrorType::from_status(503),
        Some(ErrorType::OtherServerError)
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The allowlist (§7 bullet 9)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn nothing_outside_the_telemetry_module_imports_the_sdk() {
    // The telemetry module is `observability.rs` and its directory; nothing
    // else may name an SDK, exporter or bridge crate. Written as `concat!` so
    // this file's own source does not contain the forbidden paths verbatim —
    // not that it matters here (this file is inside the module), but it keeps
    // the pattern greppable without false hits elsewhere.
    let forbidden = [
        concat!("opentelemetry", "_sdk"),
        concat!("opentelemetry", "_otlp"),
        concat!("tracing", "_opentelemetry"),
        concat!("opentelemetry", "_appender_tracing"),
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let module_file = root.join("observability.rs");
    let module_dir = root.join("observability");

    let mut offenders = Vec::new();
    let mut checked = 0;
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                if path != module_dir {
                    pending.push(path);
                }
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") || path == module_file {
                continue;
            }
            checked += 1;
            let source = std::fs::read_to_string(&path).expect("read source");
            for name in forbidden {
                if source.contains(name) {
                    offenders.push(format!("{} names `{name}`", path.display()));
                }
            }
        }
    }
    // Non-degenerate: the walk must have found the product's sources.
    assert!(checked > 20, "only {checked} files scanned");
    assert!(
        offenders.is_empty(),
        "SDK types outside the telemetry module:\n{}",
        offenders.join("\n")
    );
}
