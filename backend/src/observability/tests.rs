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
use tracing::Instrument as _;

use axum::http::{HeaderMap, HeaderValue};
use axum_test::TestServer;
use opentelemetry::trace::SpanKind;
use opentelemetry::{Key, KeyValue, Value};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_semantic_conventions as semconv;

use super::metrics::{
    self, AUTH_OUTCOMES, AuthOutcome, BUILD_INFO, BUILD_INFO_REVISION, BUILD_INFO_VERSION,
    DB_ERRORS, HttpMethod, RequestError, SSE_EVENTS, SSE_LAGGED, SSE_SUBSCRIBERS,
};
use super::test_support::{
    Pipeline, TEST_VARIABLES, collect_metrics, has_providers, prepare_for_test,
};
use super::{FLUSH_TIMEOUT, TelemetryError, adopt_parent, inject_current};
use crate::app::{DeploymentInfo, app};
use crate::db::DbOperation;
use crate::error::ErrorType;
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
        crate::listen::Draining::new(),
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

/// The startup line names the collector without any credential or token the
/// endpoint URL might carry.
#[tokio::test]
async fn the_startup_line_redacts_the_endpoint() {
    let stdout = CapturedWriter::default();
    let variables = [
        TEST_VARIABLES[0],
        TEST_VARIABLES[1],
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "http://user:hunter2@collector.invalid:4318/otlp?token=sekrit",
        ),
    ];
    let (_telemetry, subscriber, announce) =
        prepare_for_test(&variables, stdout.clone()).expect("valid");
    tracing::subscriber::with_default(subscriber, announce);
    let line = stdout
        .line("telemetry on (OTLP http/protobuf)")
        .expect("startup line");
    let endpoint = line["endpoint"].as_str().expect("endpoint field");
    assert_eq!(endpoint, "http://collector.invalid:4318/otlp");
    let text = stdout.text();
    assert!(
        !text.contains("hunter2") && !text.contains("sekrit"),
        "{text}"
    );
}

/// With auth disabled (local runs, e2e) every protected request is counted as
/// `anonymous` — the one outcome the auth-enabled test in
/// `tests/outbound_http.rs` cannot reach.
#[tokio::test]
async fn auth_disabled_requests_are_counted_as_anonymous() {
    use super::metrics::BORED_AUTH_OUTCOME;
    use super::test_support::counter_value;
    let before = counter_value(AUTH_OUTCOMES, BORED_AUTH_OUTCOME, "anonymous");
    let server = TestServer::new(router().await).unwrap();
    server.get("/api/boards").await.assert_status_ok();
    assert!(counter_value(AUTH_OUTCOMES, BORED_AUTH_OUTCOME, "anonymous") > before);
}

/// Numeric handler fields are exported as numbers. The span bridge turns a
/// `u64`/`usize` field into a *string* attribute, so they are recorded as
/// `i64`.
#[tokio::test]
async fn numeric_span_fields_are_exported_as_integers() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = TestServer::new(router().await).unwrap();
    server
        .get("/api/cards/by-number/41")
        .await
        .assert_status_not_found();

    let spans = pipeline.finished_spans();
    let handler = spans
        .iter()
        .find(|span| span.name == "get_card_by_number")
        .expect("handler span");
    let number = handler
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "bored.card.number")
        .map(|kv| kv.value.clone());
    assert_eq!(number, Some(Value::I64(41)));
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
async fn a_client_error_names_its_class_but_is_not_a_span_error() {
    // A 404 is the client's problem, not a failure of this service: semconv's
    // server rule leaves the span status unset. It still carries `error.type`
    // — the API's own class for it (`not_found`) — so *which* client error it
    // was is queryable. And its name is the route template even though the
    // path named a concrete board.
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
    let error_type = server_span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == semconv::attribute::ERROR_TYPE)
        .map(|kv| kv.value.to_string());
    assert_eq!(error_type.as_deref(), Some("not_found"));
    assert_eq!(
        server_span.status,
        opentelemetry::trace::Status::Unset,
        "a 404 is not an error of this service"
    );
    assert_eq!(
        server_span.name, "GET /api/boards/:slug",
        "template, not the raw path"
    );
}

#[tokio::test]
async fn a_server_error_marks_the_span_and_the_metric_with_error_type() {
    let stdout = CapturedWriter::default();
    let (pipeline, subscriber) = Pipeline::new(stdout.clone(), "info");
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

    // The oracle: the class `ApiError` itself logged on the `request failed`
    // line (error.rs) — a different path from the span attribute under test.
    let logged = stdout.line("request failed").expect("the 500 is logged")["error.type"]
        .as_str()
        .expect("error.type on the log line")
        .to_string();
    assert!(logged.starts_with("db_"), "a driver failure: {logged}");

    let attribute = |span: &opentelemetry_sdk::trace::SpanData, key: &str| {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    let spans = pipeline.finished_spans();
    let failed = spans
        .iter()
        .find(|span| {
            span.span_kind == SpanKind::Server && span.name == "GET /api/columns/:id/cards"
        })
        .expect("server span for the failing request");
    assert_eq!(
        attribute(failed, semconv::attribute::ERROR_TYPE).as_deref(),
        Some(logged.as_str())
    );
    assert_eq!(failed.status, opentelemetry::trace::Status::error(""));

    // And the duration metric has a series carrying that class. (The failure
    // here is in decoding the rows, after the database call returned, so it
    // belongs to the request, not to a database span — the next test covers a
    // failure *inside* a database call.)
    let series = all_series(&collect_metrics());
    assert!(
        series.iter().any(|(name, attributes)| {
            name == semconv::metric::HTTP_SERVER_REQUEST_DURATION
                && attributes.iter().any(|kv| {
                    kv.key.as_str() == semconv::attribute::ERROR_TYPE
                        && kv.value.to_string() == logged
                })
        }),
        "no duration series with error.type={logged}"
    );
}

#[tokio::test]
async fn a_failing_database_call_marks_its_own_span_and_counts_a_db_error() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = TestServer::new(router().await).unwrap();
    let request = shared::CreateBoardRequest {
        name: "dupe".to_string(),
    };
    server.post("/api/boards").json(&request).await;
    // The second create trips the unique index on the board name, inside the
    // database call itself.
    server
        .post("/api/boards")
        .json(&request)
        .await
        .assert_status(axum::http::StatusCode::CONFLICT);

    let attribute = |span: &opentelemetry_sdk::trace::SpanData, key: &str| {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    let spans = pipeline.finished_spans();
    let failed_call = spans
        .iter()
        .find(|span| {
            span.name == "CREATE boards" && span.status != opentelemetry::trace::Status::Unset
        })
        .expect("the failed CREATE boards span");
    // The class the API gives the failure (the 409's own `conflict`), not a
    // second classification — the oracle is the response status above plus
    // the enum's own label.
    assert_eq!(
        attribute(failed_call, semconv::attribute::ERROR_TYPE).as_deref(),
        Some(ErrorType::Conflict.as_str())
    );
    assert_eq!(failed_call.status, opentelemetry::trace::Status::error(""));
    assert_eq!(
        attribute(failed_call, "bored.db.query.name").as_deref(),
        Some("boards.create_board")
    );
    // It sits under the handler's span, under the request's server span —
    // which is a client error: `conflict`, status unset.
    let parent_of = |span: &opentelemetry_sdk::trace::SpanData| {
        spans
            .iter()
            .find(|candidate| candidate.span_context.span_id() == span.parent_span_id)
            .cloned()
    };
    let handler = parent_of(failed_call).expect("handler span");
    assert_eq!(handler.name, "create_board");
    let request_span = parent_of(&handler).expect("request span");
    assert_eq!(request_span.span_kind, SpanKind::Server);
    assert_eq!(request_span.status, opentelemetry::trace::Status::Unset);

    let series = all_series(&collect_metrics());
    assert!(
        series.iter().any(|(name, attributes)| {
            name == DB_ERRORS
                && attributes.iter().any(|kv| {
                    kv.key.as_str() == semconv::attribute::ERROR_TYPE
                        && kv.value.to_string() == ErrorType::Conflict.as_str()
                })
        }),
        "no bored.db.errors series with error.type=conflict"
    );
}
#[tokio::test]
async fn a_request_has_a_child_span_per_database_call_and_no_sql() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let server = TestServer::new(router().await).unwrap();
    server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "children".to_string(),
        })
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    let spans = pipeline.finished_spans();
    let request = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Server && span.name == "POST /api/boards")
        .expect("server span");
    // server span → the handler's `#[instrument]` span → one span per call.
    let handler = spans
        .iter()
        .find(|span| span.parent_span_id == request.span_context.span_id())
        .expect("handler span under the server span");
    assert_eq!(handler.name, "create_board");
    let children: Vec<_> = spans
        .iter()
        .filter(|span| span.parent_span_id == handler.span_context.span_id())
        .collect();
    let create = children
        .iter()
        .find(|span| span.name == "CREATE boards")
        .unwrap_or_else(|| {
            panic!(
                "no `CREATE boards` child among {:?}",
                children.iter().map(|span| &span.name).collect::<Vec<_>>()
            )
        });
    assert_eq!(create.span_kind, SpanKind::Client);
    assert_eq!(
        create.span_context.trace_id(),
        request.span_context.trace_id()
    );
    // Never SQL text or a bound value: the board's name appears nowhere.
    for span in &spans {
        for kv in &span.attributes {
            let value = kv.value.to_string();
            assert!(
                !value.contains("children"),
                "`{}` leaks a value: {value}",
                kv.key
            );
            assert!(
                !value.contains("SELECT *"),
                "`{}` carries SQL: {value}",
                kv.key
            );
        }
    }
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
        DB_COLLECTION_NAME,
        HTTP_REQUEST_METHOD_ORIGINAL,
        SERVER_PORT,
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

/// Raising the level after startup — `info` until config loads, then the
/// deployments' `debug` — reaches both the stdout and the OTLP layer.
#[test]
fn raising_the_log_level_after_startup_reaches_stdout_and_otlp() {
    let stdout = CapturedWriter::default();
    // `info` is the level `init` starts at.
    let (pipeline, subscriber) = Pipeline::new(stdout.clone(), "info");
    tracing::subscriber::with_default(subscriber, || {
        tracing::debug!("before the raise");
        pipeline.telemetry.set_log_level("debug");
        tracing::debug!("after the raise");
    });
    let text = stdout.text();
    assert!(!text.contains("before the raise"), "{text}");
    assert!(text.contains("after the raise"), "{text}");
    let bodies: Vec<String> = pipeline.finished_logs().iter().map(log_body).collect();
    assert!(
        !bodies.iter().any(|body| body == "before the raise"),
        "{bodies:?}"
    );
    assert!(
        bodies.iter().any(|body| body == "after the raise"),
        "{bodies:?}"
    );
}

/// A handler that panics is still a recorded request: a 500 response, an
/// ERROR server span with `error.type=_OTHER`, a duration-metric point, and
/// one log line that does not carry the panic's message.
#[tokio::test]
async fn a_panicking_handler_is_recorded_as_an_unclassified_server_error() {
    let stdout = CapturedWriter::default();
    let (pipeline, subscriber) = Pipeline::new(stdout.clone(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let router = crate::app::with_request_telemetry(axum::Router::new().route(
        "/explode",
        axum::routing::get(|| async {
            panic!("secret-panic-detail");
            #[allow(unreachable_code)]
            ""
        }),
    ));
    let server = TestServer::new(router).unwrap();
    server
        .get("/explode")
        .await
        .assert_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR);

    let spans = pipeline.finished_spans();
    let span = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Server)
        .expect("server span");
    assert_eq!(span.name, "GET /explode");
    assert_eq!(span.status, opentelemetry::trace::Status::error(""));
    let error_type = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == semconv::attribute::ERROR_TYPE)
        .map(|kv| kv.value.to_string());
    assert_eq!(error_type.as_deref(), Some("_OTHER"));

    let series = all_series(&collect_metrics());
    assert!(
        series.iter().any(|(name, attributes)| {
            name == semconv::metric::HTTP_SERVER_REQUEST_DURATION
                && attributes.iter().any(|kv| {
                    kv.key.as_str() == semconv::attribute::HTTP_ROUTE
                        && kv.value.to_string() == "/explode"
                })
        }),
        "the panicking request has no duration point"
    );

    let text = stdout.text();
    assert!(stdout.line("request handler panicked").is_some(), "{text}");
    assert!(
        !text.contains("secret-panic-detail"),
        "the panic message leaked: {text}"
    );
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
// Outbound calls and detached work (skill §1.3, §5)
// ─────────────────────────────────────────────────────────────────────────────

/// A one-route server on a real loopback port that answers `status` and
/// records the `traceparent` header of every request it receives.
async fn traceparent_recorder(status: u16) -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let router = axum::Router::new().route(
        "/token",
        axum::routing::post(move |headers: HeaderMap| {
            let record = Arc::clone(&record);
            async move {
                if let Some(value) = headers.get("traceparent") {
                    record
                        .lock()
                        .unwrap()
                        .push(value.to_str().unwrap().to_string());
                }
                axum::http::StatusCode::from_u16(status).unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), seen)
}

#[tokio::test]
async fn an_outbound_call_gets_a_client_span_and_carries_its_traceparent() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let (base, seen) = traceparent_recorder(200).await;
    let http = crate::http_client::build();

    let parent = tracing::info_span!("login");
    async {
        crate::http_client::send(
            crate::http_client::Outbound::TokenExchange,
            http.post(format!("{base}/token?code=secret-code"))
                .form(&[("a", "b")]),
        )
        .await
        .expect("request succeeds");
    }
    .instrument(parent)
    .await;

    let spans = pipeline.finished_spans();
    let login = spans
        .iter()
        .find(|span| span.name == "login")
        .expect("login span");
    let client = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Client)
        .expect("client span");
    assert_eq!(client.parent_span_id, login.span_context.span_id());
    assert_eq!(client.name, "POST oidc.token");

    // The oracle is the header the server actually received, parsed here by
    // hand (`00-<trace>-<parent span>-<flags>`), not by the propagator.
    let received = seen.lock().unwrap().clone();
    assert_eq!(received.len(), 1, "one traceparent: {received:?}");
    let parts: Vec<&str> = received[0].split('-').collect();
    assert_eq!(parts[1], client.span_context.trace_id().to_string());
    assert_eq!(
        parts[2],
        client.span_context.span_id().to_string(),
        "the callee's parent is the client span, not the handler's"
    );

    let attribute = |key: &str| {
        client
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    assert_eq!(
        attribute(semconv::attribute::HTTP_RESPONSE_STATUS_CODE).as_deref(),
        Some("200")
    );
    assert_eq!(
        attribute(semconv::attribute::SERVER_ADDRESS).as_deref(),
        Some("127.0.0.1")
    );
    let full = attribute(semconv::attribute::URL_FULL).expect("url.full");
    assert!(full.ends_with("/token"), "{full}");
    assert!(
        !full.contains("secret-code"),
        "the query must not be exported: {full}"
    );
    assert_eq!(attribute(semconv::attribute::ERROR_TYPE), None);
}

#[tokio::test]
async fn a_failed_outbound_call_is_an_error_span_with_its_status_as_error_type() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let (base, _seen) = traceparent_recorder(503).await;
    let http = crate::http_client::build();
    let response = crate::http_client::send(
        crate::http_client::Outbound::Revocation,
        http.post(format!("{base}/token")),
    )
    .await
    .expect("a response, even an error one");
    assert_eq!(
        response.status().as_u16(),
        503,
        "the caller still sees the response"
    );

    let spans = pipeline.finished_spans();
    let client = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Client)
        .expect("client span");
    let error_type = client
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == semconv::attribute::ERROR_TYPE)
        .map(|kv| kv.value.to_string());
    assert_eq!(error_type.as_deref(), Some("503"));
    assert_eq!(client.status, opentelemetry::trace::Status::error(""));
}

#[tokio::test]
async fn detached_work_starts_its_own_trace_linked_to_the_request() {
    // The shape `routes::auth::logout` uses for its revocation task.
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);

    let request = tracing::info_span!("request");
    let task = request.in_scope(|| {
        let detached = tracing::info_span!(parent: None, "detached");
        super::link_to(&detached, &tracing::Span::current());
        tokio::spawn(async { tracing::info_span!("inside").in_scope(|| {}) }.instrument(detached))
    });
    task.await.unwrap();
    drop(request);

    let spans = pipeline.finished_spans();
    let find = |name: &str| spans.iter().find(|span| span.name == name).expect(name);
    let (request, detached, inside) = (find("request"), find("detached"), find("inside"));
    assert_ne!(
        detached.span_context.trace_id(),
        request.span_context.trace_id(),
        "detached work is its own trace"
    );
    assert_eq!(
        detached.parent_span_id,
        opentelemetry::trace::SpanId::INVALID
    );
    let links: Vec<_> = detached.links.iter().collect();
    assert_eq!(links.len(), 1, "one link, to the request");
    assert_eq!(
        links[0].span_context.span_id(),
        request.span_context.span_id()
    );
    // …and the spawned task ran inside it, not in an orphaned trace.
    assert_eq!(inside.parent_span_id, detached.span_context.span_id());
}

// ─────────────────────────────────────────────────────────────────────────────
// Shutdown with streams open, and the SSE metrics
// ─────────────────────────────────────────────────────────────────────────────

/// An SSE stream never ends by itself, so a graceful shutdown with a board
/// tab open used to have nothing to wait for but the timeout. The drain signal
/// ends the stream: `serve_plain` returns promptly, and the stream is closed —
/// its span ended and its "sse unsubscribed" line written — *before* it
/// returns, so `main`'s telemetry flush that follows includes it.
#[tokio::test]
#[serial_test::serial(sse_subscribers)]
async fn shutdown_ends_open_streams_and_returns_without_waiting_out_the_drain() {
    let stdout = CapturedWriter::default();
    let (_telemetry, subscriber, _announce) = prepare_for_test(&[], stdout.clone()).expect("off");
    let _guard = tracing::subscriber::set_default(subscriber);

    let db = crate::db::connect_mem().await.expect("mem db");
    let state = AppState::new(db);
    let draining = state.draining.clone();
    let router = app(state, "./dist", DeploymentInfo::new("test", None)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::listen::serve_plain(
        listener,
        router,
        draining,
        async move {
            let _ = stop_rx.await;
        },
    ));

    // Open a stream and hold it: the response head arrives, the body never ends.
    let mut stream = reqwest::get(format!("http://{address}/api/events"))
        .await
        .expect("stream opens");
    assert_eq!(stream.status().as_u16(), 200);
    assert!(stdout.line("sse subscribed").is_some(), "{}", stdout.text());

    let started = Instant::now();
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(2),
        "returned after {took:?}: the open stream held the drain"
    );
    assert!(
        stdout.line("sse unsubscribed").is_some(),
        "the stream must be closed before serve_plain returns: {}",
        stdout.text()
    );
    // The client sees the stream end, rather than a connection that hangs.
    let rest = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await;
    assert!(rest.is_ok(), "the client's stream did not end");
}

/// Open `/api/events` on a real listener and return the first bytes the stream
/// sends within `wait`, then shut the server down (which ends and so exports
/// the stream's span). An `EventSource` gets exactly these bytes.
async fn first_sse_bytes(wait: Duration) -> String {
    let db = crate::db::connect_mem().await.expect("mem db");
    let state = AppState::new(db);
    let draining = state.draining.clone();
    let router = app(state, "./dist", DeploymentInfo::new("test", None)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(
        crate::listen::serve_plain(listener, router, draining, async move {
            let _ = stop_rx.await;
        })
        .in_current_span(),
    );
    let mut stream = reqwest::get(format!("http://{address}/api/events"))
        .await
        .expect("stream opens");
    let first = tokio::time::timeout(wait, stream.chunk())
        .await
        .ok()
        .and_then(|chunk| chunk.ok().flatten())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    first
}

/// Card #416: the browser cannot parent the stream span (an `EventSource`
/// sends no `traceparent`), so the stream's first event names that span, for
/// the browser's connect span to link to. Asserted against the span the
/// exporter actually received.
#[tokio::test]
#[serial_test::serial(sse_subscribers)]
async fn the_sse_stream_names_its_span_in_a_first_trace_event() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);

    let first = first_sse_bytes(Duration::from_secs(2)).await;
    assert!(
        first.starts_with("event: trace\n"),
        "first bytes: {first:?}"
    );
    let data = first
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("a data line");
    let named: serde_json::Value = serde_json::from_str(data).unwrap();

    let spans = pipeline.finished_spans();
    let stream_span = spans
        .iter()
        .find(|span| span.name == "sse stream")
        .expect("the stream span is exported once the stream ends");
    assert_eq!(
        named["trace_id"],
        stream_span.span_context.trace_id().to_string()
    );
    assert_eq!(
        named["span_id"],
        stream_span.span_context.span_id().to_string()
    );
}

/// With telemetry off there is no span to name, and the stream sends nothing
/// until a real event — exactly as before card #416.
#[tokio::test]
#[serial_test::serial(sse_subscribers)]
async fn with_telemetry_off_the_sse_stream_sends_no_trace_event() {
    let (_telemetry, subscriber, _announce) =
        prepare_for_test(&[], CapturedWriter::default()).expect("off");
    let _guard = tracing::subscriber::set_default(subscriber);
    let first = first_sse_bytes(Duration::from_millis(500)).await;
    assert!(!first.contains("event: trace"), "first bytes: {first:?}");
}

/// A request that ignores the drain signal cannot hold shutdown: after
/// `DRAIN_TIMEOUT`, `serve_plain` stops waiting and returns.
#[tokio::test]
async fn shutdown_gives_up_on_a_request_that_outlasts_the_drain() {
    let router = axum::Router::new().route(
        "/hang",
        axum::routing::get(|| async {
            std::future::pending::<()>().await;
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::listen::serve_plain(
        listener,
        router,
        crate::listen::Draining::new(),
        async move {
            let _ = stop_rx.await;
        },
    ));

    // Start the hanging request and give it time to reach the handler.
    let hanging = tokio::spawn(reqwest::get(format!("http://{address}/hang")));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = Instant::now();
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    let took = started.elapsed();

    let drain = crate::listen::DRAIN_TIMEOUT;
    assert!(
        took >= drain - Duration::from_millis(200),
        "returned after {took:?}: an in-flight request must get the drain period"
    );
    assert!(
        took < drain + Duration::from_secs(2),
        "returned after {took:?}: the drain is not bounded"
    );
    hanging.abort();
}

/// Start `serve_tls` — the deployed listener — on an ephemeral loopback port
/// with a certificate made for this test. Returns the base URL, a client that
/// accepts that certificate, the stop sender and the server task.
async fn start_tls(
    router: axum::Router,
    draining: crate::listen::Draining,
) -> (
    String,
    reqwest::Client,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    // The backend installs `ring` in `main`; a test has to do it itself.
    // (`Err` means another test already did — fine.)
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("test certificate");
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        certified.cert.pem().into_bytes(),
        certified.signing_key.serialize_pem().into_bytes(),
    )
    .await
    .expect("tls config");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::listen::serve_tls(
        listener,
        tls,
        router,
        draining,
        async move {
            let _ = stop_rx.await;
        },
    ));
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    (format!("https://localhost:{port}"), client, stop_tx, server)
}

/// `url.scheme` on the server span and the duration metric is the scheme of
/// the listener the request arrived on — `https` on the TLS listener both
/// deployments run, `http` on the plain one.
#[tokio::test]
async fn url_scheme_comes_from_the_listener() {
    let (pipeline, subscriber) = Pipeline::new(CapturedWriter::default(), "info");
    let _guard = tracing::subscriber::set_default(subscriber);
    let scheme_of = |spans: &[opentelemetry_sdk::trace::SpanData], route: &str| {
        spans
            .iter()
            .find(|span| span.span_kind == SpanKind::Server && span.name == format!("GET {route}"))
            .and_then(|span| {
                span.attributes
                    .iter()
                    .find(|kv| kv.key.as_str() == semconv::attribute::URL_SCHEME)
                    .map(|kv| kv.value.to_string())
            })
    };

    // TLS listener.
    let tls_router = crate::app::with_request_telemetry(
        axum::Router::new().route("/over-tls", axum::routing::get(|| async { "ok" })),
    );
    let (base, client, stop_tx, server) =
        start_tls(tls_router, crate::listen::Draining::new()).await;
    client.get(format!("{base}/over-tls")).send().await.unwrap();
    stop_tx.send(()).unwrap();
    server.await.unwrap();

    // Plain listener.
    let plain_router = crate::app::with_request_telemetry(
        axum::Router::new().route("/over-plain", axum::routing::get(|| async { "ok" })),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (plain_stop, plain_rx) = tokio::sync::oneshot::channel::<()>();
    let plain = tokio::spawn(crate::listen::serve_plain(
        listener,
        plain_router,
        crate::listen::Draining::new(),
        async move {
            let _ = plain_rx.await;
        },
    ));
    reqwest::get(format!("http://{address}/over-plain"))
        .await
        .unwrap();
    plain_stop.send(()).unwrap();
    plain.await.unwrap();

    let spans = pipeline.finished_spans();
    assert_eq!(scheme_of(&spans, "/over-tls").as_deref(), Some("https"));
    assert_eq!(scheme_of(&spans, "/over-plain").as_deref(), Some("http"));
    // The metric reads the scheme separately from the span, so check it per
    // route — routes unique to this test, so no other test's series can
    // satisfy the assertion.
    let series = all_series(&collect_metrics());
    let metric_scheme = |route: &str| {
        series
            .iter()
            .filter(|(name, _)| name == semconv::metric::HTTP_SERVER_REQUEST_DURATION)
            .find(|(_, attributes)| {
                attributes.iter().any(|kv| {
                    kv.key.as_str() == semconv::attribute::HTTP_ROUTE
                        && kv.value.to_string() == route
                })
            })
            .and_then(|(_, attributes)| {
                attributes
                    .iter()
                    .find(|kv| kv.key.as_str() == semconv::attribute::URL_SCHEME)
                    .map(|kv| kv.value.to_string())
            })
    };
    assert_eq!(metric_scheme("/over-tls").as_deref(), Some("https"));
    assert_eq!(metric_scheme("/over-plain").as_deref(), Some("http"));
}

/// `TCP_NODELAY` as the **server** sees it, on the socket it accepted for the
/// client connection whose local address is `client` and which reached the
/// listener at `server` (card #452).
///
/// The client end's own option says nothing about the server's, and neither
/// listener hands its accepted streams out — so find the socket the way the
/// kernel lists it: the test runs the server in this process, so the accepted
/// socket is one of this process's file descriptors. For each socket fd in
/// `/proc/self/fd`, view it as a `std::net::TcpStream` without taking
/// ownership (`ManuallyDrop`: dropping it would close a descriptor the server
/// still owns) and match on its address pair, which is the client's mirrored.
///
/// The server accepts and sets the option on its own task, so poll: this
/// returns as soon as the socket is found with the option set, and otherwise,
/// after a bounded wait, what it last saw — `Some(false)` if the socket
/// exists without the option, `None` if it never appeared.
#[cfg(target_os = "linux")]
async fn server_side_nodelay(
    client: std::net::SocketAddr,
    server: std::net::SocketAddr,
) -> Option<bool> {
    use std::mem::ManuallyDrop;
    use std::os::fd::FromRawFd as _;

    let look = || -> Option<bool> {
        let entries = std::fs::read_dir("/proc/self/fd").ok()?;
        for entry in entries.flatten() {
            // Only sockets: the link target of a socket fd is `socket:[inode]`.
            let is_socket = std::fs::read_link(entry.path())
                .is_ok_and(|target| target.to_string_lossy().starts_with("socket:"));
            let Some(fd) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                continue;
            };
            if !is_socket {
                continue;
            }
            // SAFETY: `fd` is open in this process (just listed), and the
            // `ManuallyDrop` means it is never closed here; the calls below
            // only read socket state. A non-TCP socket (or one another test
            // closes meanwhile) just fails the address calls and is skipped.
            let stream = ManuallyDrop::new(unsafe { std::net::TcpStream::from_raw_fd(fd) });
            if stream.local_addr().ok() == Some(server) && stream.peer_addr().ok() == Some(client) {
                return stream.nodelay().ok();
            }
        }
        None
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let seen = look();
        if seen == Some(true) || Instant::now() >= deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The TLS listener — what both deployments run — sets `TCP_NODELAY` on the
/// connections it accepts, so a response split across TLS records is not
/// held back by the peer's delayed ACK (card #452). It still serves after.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn tls_listener_sets_tcp_nodelay_on_accepted_connections() {
    let router = axum::Router::new().route("/nodelay", axum::routing::get(|| async { "ok" }));
    let (base, client, stop_tx, server) = start_tls(router, crate::listen::Draining::new()).await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    let listener_addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    // A bare TCP connection: the TCP acceptor runs before the TLS handshake,
    // so the option is set on accept whether or not the client ever speaks TLS.
    let connection = tokio::net::TcpStream::connect(listener_addr).await.unwrap();
    let nodelay = server_side_nodelay(connection.local_addr().unwrap(), listener_addr).await;
    assert_eq!(
        nodelay,
        Some(true),
        "server-side TCP_NODELAY on the TLS listener"
    );

    // Liveness: the listener built with the new acceptor still serves HTTPS.
    let response = client.get(format!("{base}/nodelay")).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");

    drop(connection);
    stop_tx.send(()).unwrap();
    server.await.unwrap();
}

/// …and so does the plain-HTTP listener (dev mode, e2e), so the two shapes
/// behave alike. The request is served over the very connection checked.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn plain_listener_sets_tcp_nodelay_on_accepted_connections() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let router = axum::Router::new().route("/nodelay", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener_addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::listen::serve_plain(
        listener,
        router,
        crate::listen::Draining::new(),
        async move {
            let _ = stop_rx.await;
        },
    ));

    let mut connection = tokio::net::TcpStream::connect(listener_addr).await.unwrap();
    // Checked before the request: once served, `Connection: close` has the
    // server close its end, and the socket is gone from the fd table.
    let nodelay = server_side_nodelay(connection.local_addr().unwrap(), listener_addr).await;
    assert_eq!(
        nodelay,
        Some(true),
        "server-side TCP_NODELAY on the plain listener"
    );

    // Liveness: that same connection is served. `Connection: close` so the
    // server ends the response by closing, and `read_to_end` knows when it
    // has all of it.
    connection
        .write_all(b"GET /nodelay HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    connection.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("ok"), "{response}");

    stop_tx.send(()).unwrap();
    server.await.unwrap();
}

/// The TLS listener (what both deployments run) ends open SSE streams on
/// shutdown and returns promptly, with the stream closed first.
#[tokio::test]
#[serial_test::serial(sse_subscribers)]
async fn tls_shutdown_ends_open_streams_and_returns_without_waiting_out_the_drain() {
    let stdout = CapturedWriter::default();
    let (_telemetry, subscriber, _announce) = prepare_for_test(&[], stdout.clone()).expect("off");
    let _guard = tracing::subscriber::set_default(subscriber);

    let db = crate::db::connect_mem().await.expect("mem db");
    let state = AppState::new(db);
    let draining = state.draining.clone();
    let router = app(state, "./dist", DeploymentInfo::new("test", None)).await;
    let (base, client, stop_tx, server) = start_tls(router, draining).await;

    let stream = client
        .get(format!("{base}/api/events"))
        .send()
        .await
        .expect("stream opens over TLS");
    assert_eq!(stream.status().as_u16(), 200);
    assert!(stdout.line("sse subscribed").is_some(), "{}", stdout.text());

    let started = Instant::now();
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    let took = started.elapsed();

    assert!(
        took < Duration::from_secs(2),
        "returned after {took:?}: the open stream held the drain"
    );
    assert!(
        stdout.line("sse unsubscribed").is_some(),
        "the stream must be closed before serve_tls returns: {}",
        stdout.text()
    );
    drop(stream);
}

/// …and a request that ignores the drain signal cannot hold it past
/// `DRAIN_TIMEOUT`.
#[tokio::test]
async fn tls_shutdown_gives_up_on_a_request_that_outlasts_the_drain() {
    let router = axum::Router::new().route(
        "/hang",
        axum::routing::get(|| async {
            std::future::pending::<()>().await;
        }),
    );
    let (base, client, stop_tx, server) = start_tls(router, crate::listen::Draining::new()).await;

    let hanging = tokio::spawn(async move { client.get(format!("{base}/hang")).send().await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    let took = started.elapsed();

    let drain = crate::listen::DRAIN_TIMEOUT;
    assert!(
        took >= drain - Duration::from_millis(200),
        "returned after {took:?}: an in-flight request must get the drain period"
    );
    assert!(
        took < drain + Duration::from_secs(2),
        "returned after {took:?}: the drain is not bounded"
    );
    hanging.abort();
}

// ─────────────────────────────────────────────────────────────────────────────
// The real OTLP client, end to end
// ─────────────────────────────────────────────────────────────────────────────

/// What a fake OTLP/HTTP collector received: one entry per POST.
#[derive(Clone, Debug)]
struct Received {
    path: String,
    content_type: String,
    body: Vec<u8>,
}

/// A fake collector on a real loopback port: accepts any POST, records it,
/// answers 200 with an empty body (a valid empty protobuf response).
async fn fake_collector() -> (String, Arc<Mutex<Vec<Received>>>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&received);
    let router = axum::Router::new().fallback(
        move |uri: axum::http::Uri, headers: HeaderMap, body: axum::body::Bytes| {
            let record = Arc::clone(&record);
            async move {
                record.lock().unwrap().push(Received {
                    path: uri.path().to_string(),
                    content_type: headers
                        .get("content-type")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("")
                        .to_string(),
                    body: body.to_vec(),
                });
                axum::http::StatusCode::OK
            }
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), received)
}

/// The production exporters — through `OtlpHttpClient`, the only way the
/// signals leave the process — deliver a span and a log record to a collector
/// as http/protobuf, on the OTLP paths, carrying the span's trace id and the
/// service identity. Every other test reads the in-memory exporters; this one
/// checks the bytes that actually go out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_exporters_deliver_spans_and_logs_to_a_collector() {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let (endpoint, received) = fake_collector().await;
    let stdout = CapturedWriter::default();
    let variables = [
        TEST_VARIABLES[0],
        TEST_VARIABLES[1],
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
        ("OTEL_METRICS_EXPORTER", "none"),
    ];
    let (telemetry, subscriber, _announce) =
        prepare_for_test(&variables, stdout.clone()).expect("on");

    // The global default for the duration, so the batch threads' spans and
    // the exporters' own tasks all see the same subscriber.
    let trace_id = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let span = tracing::info_span!("delivered span");
        let trace_id = span.context().span().span_context().trace_id();
        span.in_scope(|| tracing::info!("delivered log line"));
        trace_id
    };
    telemetry.shutdown().await;

    let received = received.lock().unwrap().clone();
    let paths: BTreeSet<&str> = received.iter().map(|post| post.path.as_str()).collect();
    assert!(
        paths.contains("/v1/traces"),
        "paths: {paths:?}; stdout: {}",
        stdout.text()
    );
    assert!(paths.contains("/v1/logs"), "paths: {paths:?}");
    for post in &received {
        assert_eq!(post.content_type, "application/x-protobuf", "{}", post.path);
    }
    // The trace id travels as its 16 raw bytes in the protobuf body.
    let trace_bytes = trace_id.to_bytes();
    let contains = |haystack: &[u8], needle: &[u8]| {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    };
    for path in ["/v1/traces", "/v1/logs"] {
        let body: Vec<u8> = received
            .iter()
            .filter(|post| post.path == path)
            .flat_map(|post| post.body.clone())
            .collect();
        assert!(contains(&body, &trace_bytes), "{path} lacks the trace id");
        assert!(contains(&body, b"bored"), "{path} lacks service.name");
        assert!(
            contains(&body, b"deployment.environment.name"),
            "{path} lacks the environment"
        );
    }
}

/// At `log-level = warn` the product's INFO spans are below the stdout
/// layer's own filter, yet a WARN line inside one must still carry its ids —
/// the same ones the OTLP record carries.
#[test]
fn a_warning_inside_an_info_span_keeps_its_ids_at_warn_level() {
    let stdout = CapturedWriter::default();
    let (pipeline, subscriber) = Pipeline::new(stdout.clone(), "warn");
    tracing::subscriber::with_default(subscriber, || {
        tracing::info_span!("request").in_scope(|| tracing::warn!("something odd"));
    });
    let spans = pipeline.finished_spans();
    let span = spans
        .iter()
        .find(|span| span.name == "request")
        .expect("span");
    let line = stdout.line("something odd").expect("stdout line");
    assert_eq!(line["trace_id"], span.span_context.trace_id().to_string());
    assert_eq!(line["span_id"], span.span_context.span_id().to_string());
}

/// Stdout names the same span the OTLP record does when the innermost span is
/// a dependency's (surrealdb opens `debug` spans the span layer filters out).
#[test]
fn a_log_inside_a_dependency_span_carries_the_enclosing_product_span_ids() {
    let stdout = CapturedWriter::default();
    let (pipeline, subscriber) = Pipeline::new(stdout.clone(), "debug");
    tracing::subscriber::with_default(subscriber, || {
        tracing::info_span!("product").in_scope(|| {
            tracing::debug_span!(target: "surrealdb::core", "dependency").in_scope(|| {
                tracing::debug!("inside the dependency span");
            });
        });
    });

    let spans = pipeline.finished_spans();
    let product = spans
        .iter()
        .find(|span| span.name == "product")
        .expect("product span");
    assert!(
        !spans.iter().any(|span| span.name == "dependency"),
        "precondition: the dependency span is not exported"
    );
    let line = stdout
        .line("inside the dependency span")
        .expect("stdout line");
    assert_eq!(
        line["trace_id"],
        product.span_context.trace_id().to_string()
    );
    assert_eq!(line["span_id"], product.span_context.span_id().to_string());

    let logs = pipeline.finished_logs();
    let record = logs
        .iter()
        .find(|log| log_body(log) == "inside the dependency span")
        .expect("record");
    let context = record.record.trace_context().expect("trace context");
    assert_eq!(context.span_id, product.span_context.span_id());
}

/// `bored.sse.subscribers` goes up when a stream opens and back down when the
/// client goes away; a subscriber that falls more than the broadcast capacity
/// behind has the skipped events counted in `bored.sse.lagged` (they used to be
/// dropped silently); delivered events count in `bored.sse.events`. Driven
/// through the real handler on a real socket.
///
/// The gauge is process-wide, so any other test with a stream open at the same
/// moment moves it too (seen in the image build: `+2`, not `+1`). Every test
/// that opens `/api/events` therefore shares the `sse_subscribers` serial key.
#[tokio::test]
#[serial_test::serial(sse_subscribers)]
async fn sse_streams_count_subscribers_delivered_and_lagged_events() {
    use super::test_support::sum_value;
    use crate::events::{BROADCAST_CAPACITY, BoardEvent, BroadcastEvent};

    let db = crate::db::connect_mem().await.expect("mem db");
    let state = AppState::new(db);
    let events = state.events.clone();
    let router = app(state, "./dist", DeploymentInfo::new("test", None)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let subscribers_before = sum_value(SSE_SUBSCRIBERS);
    let lagged_before = sum_value(SSE_LAGGED);
    let delivered_before = sum_value(SSE_EVENTS);

    let mut stream = reqwest::get(format!("http://{address}/api/events"))
        .await
        .expect("stream opens");
    assert_eq!(sum_value(SSE_SUBSCRIBERS), subscribers_before + 1);

    // Send more than the channel holds without yielding. The test runtime is
    // single-threaded, so the server cannot drain the receiver meanwhile: it
    // falls `overflow` events behind.
    let overflow = 50;
    let event = || BroadcastEvent {
        board_id: "b".to_string(),
        event: BoardEvent::BoardDeleted {
            board_id: "b".to_string(),
        },
    };
    for _ in 0..(BROADCAST_CAPACITY + overflow) {
        let _ = events.send(event());
    }
    // Reading lets the server poll the stream, meet the lag, and deliver what
    // the channel still holds.
    let first = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
        .await
        .expect("an event arrives")
        .expect("readable");
    assert!(first.is_some());
    assert!(
        sum_value(SSE_LAGGED) >= lagged_before + overflow as i64,
        "lagged: {} -> {}",
        lagged_before,
        sum_value(SSE_LAGGED)
    );
    assert!(sum_value(SSE_EVENTS) > delivered_before);

    // The client goes away. The server notices on its next write, so keep
    // sending until the subscription is released (bounded).
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(5);
    while sum_value(SSE_SUBSCRIBERS) != subscribers_before && Instant::now() < deadline {
        let _ = events.send(event());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        sum_value(SSE_SUBSCRIBERS),
        subscribers_before,
        "the subscriber count must come back down on disconnect"
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
    {
        use strum::IntoEnumIterator as _;
        for error_type in ErrorType::iter() {
            metrics::db_error(error_type);
        }
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
    // `RequestError::all()` walks card #366's `ErrorType` with strum, plus the
    // `_OTHER` fallback — so a new `ErrorType` variant is covered here too.
    check(&RequestError::all(), RequestError::label);
    check(&DbOperation::ALL, DbOperation::label);
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

    // error.type: the handler's class when there is one; `_OTHER` for an
    // unclassified 5xx; nothing for an unclassified success or client error.
    assert_eq!(RequestError::for_response(200, None), None);
    assert_eq!(RequestError::for_response(401, None), None);
    assert_eq!(
        RequestError::for_response(404, Some(ErrorType::NotFound)),
        Some(RequestError::Api(ErrorType::NotFound))
    );
    assert_eq!(
        RequestError::for_response(503, None).map(RequestError::label),
        Some("_OTHER")
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
