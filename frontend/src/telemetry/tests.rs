//! Behaviour of the public API against the real runtime state, on the host.
//!
//! Each test starts from [`test_support::reset`], the state a freshly loaded
//! page has: telemetry undecided (Pending), outboxes empty. `cargo test` runs
//! tests on several threads, and the state is `thread_local!`, so tests never
//! see one another's spans.

use super::otlp::Signal;
use super::runtime::{NOT_INITIALISED, Phase, test_support};
use super::*;

fn enabled() -> shared::ClientTelemetryConfig {
    shared::ClientTelemetryConfig {
        enabled: true,
        endpoint: "https://bored.example".to_string(),
    }
}

/// A test error whose console text contains something that must never be
/// exported.
struct Secretive;

impl std::fmt::Display for Secretive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server said: the card titled 'my secret plan' conflicts")
    }
}

impl Reportable for Secretive {
    fn error_type(&self) -> String {
        "409".to_string()
    }
    fn status(&self) -> Option<u16> {
        Some(409)
    }
}

#[test]
fn span_parenting_is_explicit_across_overlapping_spans() {
    test_support::reset();
    // A screen span, and two requests under it that overlap each other and
    // outlive it — the interleaving an ambient span stack gets wrong.
    let screen = start_span("screen board", SpanKind::Internal, None);
    let screen_ctx = screen.context().expect("pending telemetry records");
    let first = start_span("GET /api/boards/{slug}", SpanKind::Client, Some(screen_ctx));
    let unrelated = start_span("PUT /api/cards/{id}", SpanKind::Client, None);
    let second = start_span(
        "GET /api/boards/{slug}/columns",
        SpanKind::Client,
        Some(screen_ctx),
    );
    let (first_ctx, second_ctx, unrelated_ctx) = (
        first.context().unwrap(),
        second.context().unwrap(),
        unrelated.context().unwrap(),
    );
    // End in an order unrelated to start order.
    screen.end();
    second.end();
    unrelated.end();
    first.end();

    let spans = test_support::drain(Signal::Traces);
    assert_eq!(spans.len(), 4);
    let find = |ctx: SpanContext| {
        spans
            .iter()
            .find(|span| span["spanId"] == ctx.span_id.to_hex())
            .expect("span exported")
            .clone()
    };
    let screen_json = find(screen_ctx);
    assert!(
        screen_json.get("parentSpanId").is_none(),
        "screen is a root"
    );
    for child in [first_ctx, second_ctx] {
        let json = find(child);
        assert_eq!(json["parentSpanId"], screen_ctx.span_id.to_hex());
        assert_eq!(json["traceId"], screen_ctx.trace_id.to_hex());
    }
    let unrelated_json = find(unrelated_ctx);
    assert!(unrelated_json.get("parentSpanId").is_none());
    assert_ne!(
        unrelated_json["traceId"],
        screen_ctx.trace_id.to_hex(),
        "a user action is its own trace"
    );
    // Every span got its own id.
    let ids: std::collections::HashSet<_> = spans
        .iter()
        .map(|span| span["spanId"].to_string())
        .collect();
    assert_eq!(ids.len(), 4);
}

#[test]
fn a_spans_end_is_after_its_start() {
    test_support::reset();
    let span = start_span("x", SpanKind::Internal, None);
    span.end();
    let spans = test_support::drain(Signal::Traces);
    let start: u64 = spans[0]["startTimeUnixNano"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let end: u64 = spans[0]["endTimeUnixNano"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(end >= start);
    assert!(start > 1_600_000_000_000_000_000, "an epoch time in ns");
}

#[test]
fn no_configuration_means_otlp_is_never_initialised() {
    test_support::reset();
    // Launch-sequence span, buffered while undecided.
    start_span("screen home", SpanKind::Internal, None).end();
    configure(None);
    assert_eq!(test_support::phase(), Phase::Disabled);
    // The launch buffer is discarded, and counted.
    assert!(test_support::drain(Signal::Traces).is_empty());
    assert_eq!(test_support::dropped(), 1);
    // From now on spans are inert: no traceparent, nothing buffered.
    let span = start_span("GET /api/boards", SpanKind::Client, None);
    assert_eq!(span.traceparent(), None);
    assert_eq!(span.context(), None);
    span.end();
    error("failed", &Secretive);
    assert!(test_support::drain(Signal::Traces).is_empty());
    assert!(test_support::drain(Signal::Logs).is_empty());
}

#[test]
fn explicitly_disabled_is_the_same_as_absent() {
    test_support::reset();
    configure(Some(&shared::ClientTelemetryConfig {
        enabled: false,
        endpoint: "https://bored.example".to_string(),
    }));
    assert_eq!(test_support::phase(), Phase::Disabled);
}

#[test]
fn enabled_keeps_the_launch_buffer_and_a_later_off_is_honoured() {
    test_support::reset();
    start_span("screen home", SpanKind::Internal, None).end();
    configure(Some(&enabled()));
    assert_eq!(
        test_support::phase(),
        Phase::Enabled {
            endpoint: "https://bored.example".to_string()
        }
    );
    assert_eq!(
        test_support::drain(Signal::Traces).len(),
        1,
        "launch span kept"
    );
    // A later answer switching off wins; a later "on" does not come back.
    configure(None);
    assert_eq!(test_support::phase(), Phase::Disabled);
    configure(Some(&enabled()));
    assert_eq!(test_support::phase(), Phase::Disabled);
}

#[test]
fn the_launch_buffer_is_discarded_if_configuration_never_arrives() {
    test_support::reset();
    start_span("screen home", SpanKind::Internal, None).end();
    let now = platform::wall_ms();
    let line = runtime::with_state(|state| state.expire_launch_buffer(now + 1_000.0)).unwrap();
    assert_eq!(line, None, "still within the bound");
    let line = runtime::with_state(|state| {
        state.expire_launch_buffer(now + config::LAUNCH_BUFFER_MS + 1.0)
    })
    .unwrap();
    assert_eq!(line.as_deref(), Some(NOT_INITIALISED));
    assert_eq!(test_support::phase(), Phase::Disabled);
    assert!(test_support::drain(Signal::Traces).is_empty());
}

#[test]
fn traceparent_is_sent_while_undecided_and_withheld_once_off() {
    test_support::reset();
    // Undecided: the launch requests carry it, so an enabled deployment's
    // first trace is whole.
    let launch = start_span("GET /api/boards", SpanKind::Client, None);
    assert!(launch.traceparent().is_some());
    drop(launch);
    // Decided off: no header, the server starts its own trace.
    configure(None);
    let later = start_span("GET /api/boards", SpanKind::Client, None);
    assert_eq!(later.traceparent(), None);
}

#[test]
fn a_traceparent_names_the_span_itself() {
    test_support::reset();
    let span = start_span("GET /api/boards", SpanKind::Client, None);
    let context = span.context().unwrap();
    assert_eq!(
        span.traceparent().unwrap(),
        format!(
            "00-{}-{}-01",
            context.trace_id.to_hex(),
            context.span_id.to_hex()
        )
    );
}

#[test]
fn error_logs_carry_class_status_and_trace_but_never_the_message() {
    test_support::reset();
    struct Traced(SpanContext);
    impl std::fmt::Display for Traced {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "user typed: hunter2")
        }
    }
    impl Reportable for Traced {
        fn error_type(&self) -> String {
            "network".to_string()
        }
        fn trace(&self) -> Option<SpanContext> {
            Some(self.0)
        }
    }
    let request = start_span("PUT /api/cards/{id}", SpanKind::Client, None);
    let context = request.context().unwrap();
    drop(request);

    error("card save failed", &Traced(context));
    error("tag save failed", &Secretive);

    let logs = test_support::drain(Signal::Logs);
    assert_eq!(logs.len(), 2);
    let exported = serde_json::to_string(&logs).unwrap();
    assert!(!exported.contains("hunter2"));
    assert!(!exported.contains("secret plan"));
    assert_eq!(logs[0]["body"]["stringValue"], "card save failed");
    assert_eq!(logs[0]["traceId"], context.trace_id.to_hex());
    assert_eq!(logs[0]["spanId"], context.span_id.to_hex());
    assert_eq!(logs[1]["attributes"][0]["value"]["stringValue"], "409");
    assert_eq!(logs[1]["attributes"][1]["value"]["intValue"], "409");
    assert!(logs[1].get("traceId").is_none());
}

#[test]
fn a_panic_is_logged_inside_a_span_under_the_current_screen() {
    test_support::reset();
    let screen = start_span("screen board", SpanKind::Internal, None);
    set_current_screen(screen.context());
    let screen_ctx = screen.context().unwrap();
    drop(screen);

    record_panic(Some(PanicLocation {
        file: "src/components/card.rs",
        line: 42,
        column: 7,
    }));

    let logs = test_support::drain(Signal::Logs);
    assert_eq!(logs.len(), 1);
    let log = &logs[0];
    assert_eq!(log["body"]["stringValue"], "wasm panic");
    assert_eq!(log["traceId"], screen_ctx.trace_id.to_hex());
    let attrs = log["attributes"].as_array().unwrap();
    let value = |key: &str| {
        attrs
            .iter()
            .find(|a| a["key"] == key)
            .map(|a| a["value"].clone())
    };
    assert_eq!(
        value("exception.type"),
        Some(serde_json::json!({"stringValue": "panic"}))
    );
    assert_eq!(
        value("code.file.path"),
        Some(serde_json::json!({"stringValue": "src/components/card.rs"}))
    );
    assert_eq!(
        value("code.line.number"),
        Some(serde_json::json!({"intValue": "42"}))
    );
    assert_eq!(
        value("code.column.number"),
        Some(serde_json::json!({"intValue": "7"}))
    );
    // The message is never exported, whatever it says.
    assert_eq!(value("exception.message"), None);

    let spans = test_support::drain(Signal::Traces);
    let panic_span = spans
        .iter()
        .find(|s| s["name"] == "panic")
        .expect("panic span");
    assert_eq!(panic_span["parentSpanId"], screen_ctx.span_id.to_hex());
    assert_eq!(panic_span["spanId"], log["spanId"]);
    assert_eq!(panic_span["status"]["code"], 2);
}

#[test]
fn a_401_drops_the_batch_and_the_cached_token_once_then_stops() {
    test_support::reset();
    configure(Some(&enabled()));
    let far = platform::wall_ms() + 1e9;
    test_support::set_token("t1", far);
    start_span("a", SpanKind::Internal, None).end();
    let line = test_support::respond(Signal::Traces, 401, 1);
    assert!(
        line.unwrap().contains("HTTP 401"),
        "first failure is reported"
    );
    assert_eq!(
        test_support::token(0.0),
        None,
        "token dropped for a refresh"
    );
    assert_eq!(test_support::dropped(), 1);

    test_support::set_token("t2", far);
    start_span("b", SpanKind::Internal, None).end();
    let line = test_support::respond(Signal::Traces, 401, 1).unwrap();
    assert!(line.contains("giving up"), "{line}");
    assert!(!line.contains("t2"), "the token never reaches the console");
    // Stopped: new spans are no longer buffered.
    let span = start_span("c", SpanKind::Internal, None);
    assert_eq!(span.context(), None);
}

#[test]
fn a_retryable_answer_keeps_the_batch_for_later() {
    test_support::reset();
    configure(Some(&enabled()));
    start_span("a", SpanKind::Internal, None).end();
    let _ = test_support::respond(Signal::Traces, 503, 1);
    assert!(test_support::has_retry(Signal::Traces));
    assert_eq!(test_support::dropped(), 0, "not dropped while retrying");
}

#[test]
fn a_hidden_tab_flush_honours_backoff_but_the_final_flush_does_not() {
    use super::runtime::Flush;
    test_support::reset();
    configure(Some(&enabled()));
    // A batch the ingest answered 503 to: it waits for its backoff.
    start_span("waiting", SpanKind::Internal, None).end();
    let _ = test_support::respond(Signal::Traces, 503, 1);
    assert!(test_support::has_retry(Signal::Traces));
    // Something newer arrives meanwhile.
    start_span("newer", SpanKind::Internal, None).end();

    // Hidden: neither the waiting retry nor the newer item goes.
    assert_eq!(test_support::unload(Signal::Traces, Flush::Hidden), None);
    assert!(
        test_support::has_retry(Signal::Traces),
        "the retry keeps its place"
    );

    // Final (pagehide, panic): there is no later — the retry goes first.
    assert_eq!(test_support::unload(Signal::Traces, Flush::Final), Some(1));
    assert!(!test_support::has_retry(Signal::Traces));
    // With no retry waiting, a hidden flush sends the fresh items.
    assert_eq!(test_support::unload(Signal::Traces, Flush::Hidden), Some(1));
}

#[test]
fn transition_lines_name_the_endpoint_and_the_dropped_count() {
    test_support::reset();
    configure(Some(&enabled()));
    start_span("a", SpanKind::Internal, None).end();
    let line = test_support::respond(Signal::Traces, 400, 1).unwrap();
    assert!(line.contains("https://bored.example"), "{line}");
    assert!(line.contains("1 dropped"), "{line}");
    start_span("b", SpanKind::Internal, None).end();
    let line = test_support::respond(Signal::Traces, 200, 1).unwrap();
    assert!(line.contains("working again"), "{line}");
}

// ── Timestamps: the anchored clock (card #458) ───────────────────────────
//
// These run on `platform::fake_clock`, so the wall clock and the monotonic
// clock can be made to disagree the way a browser's do. Every expected time is
// a literal worked by hand from the readings set just before it, never a
// value computed by the code under test.

/// 2023-11-14T22:13:20Z in nanoseconds: the wall clock when each test starts.
const WALL_0: u64 = 1_700_000_000_000_000_000;
/// `performance.now()` at that moment: 3 s into the document's life.
const MONO_0: u64 = 3_000_000_000;

/// A span's exported start or end time (a JSON string of nanoseconds).
fn nanos(span: &serde_json::Value, key: &str) -> u64 {
    span[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} is a string"))
        .parse()
        .expect("nanoseconds")
}

/// Find one exported span by name.
fn named<'a>(spans: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    spans
        .iter()
        .find(|span| span["name"] == name)
        .unwrap_or_else(|| panic!("span {name} exported"))
}

#[test]
fn a_trace_starts_at_wall_time_and_its_children_stay_inside_their_parent() {
    platform::fake_clock::set(WALL_0, MONO_0);
    test_support::reset();
    let root = start_span("screen board", SpanKind::Internal, None);
    let root_ctx = root.context().unwrap();

    // 10 ms later by the monotonic clock — but the wall clock has been
    // corrected *backwards* by half a second meanwhile (an NTP step). A child
    // stamped from the wall clock would start 490 ms before its parent.
    platform::fake_clock::set(WALL_0 - 500_000_000, MONO_0 + 10_000_000);
    let child = start_span("GET /api/boards/{slug}", SpanKind::Client, Some(root_ctx));

    // 20 ms on, the wall clock jumps forward two seconds; the child ends.
    platform::fake_clock::set(WALL_0 + 2_000_000_000, MONO_0 + 30_000_000);
    child.end();

    // 10 ms more, and the root ends.
    platform::fake_clock::set(WALL_0 + 2_100_000_000, MONO_0 + 40_000_000);
    root.end();

    let spans = test_support::drain(Signal::Traces);
    let root = named(&spans, "screen board");
    let child = named(&spans, "GET /api/boards/{slug}");
    // Root: starts at the wall clock's reading, not the monotonic clock's
    // 3 s, and lasts the monotonic 40 ms, whatever the wall clock did.
    assert_eq!(nanos(root, "startTimeUnixNano"), 1_700_000_000_000_000_000);
    assert_eq!(nanos(root, "endTimeUnixNano"), 1_700_000_000_040_000_000);
    // Child: on the root's timeline — 10 ms to 30 ms after the root began.
    assert_eq!(nanos(child, "startTimeUnixNano"), 1_700_000_000_010_000_000);
    assert_eq!(nanos(child, "endTimeUnixNano"), 1_700_000_000_030_000_000);
}

#[test]
fn a_new_trace_re_anchors_so_a_sleep_cannot_push_it_into_the_past() {
    platform::fake_clock::set(WALL_0, MONO_0);
    test_support::reset();
    start_span("screen home", SpanKind::Internal, None).end();

    // The device sleeps for eight hours. The wall clock moves on; the
    // monotonic clock stood still and has advanced only 1 ms since. The old
    // `timeOrigin + now()` clock would stamp the next span 1 ms after the
    // last one — eight hours in the past.
    platform::fake_clock::set(WALL_0 + 28_800_000_000_000, MONO_0 + 1_000_000);
    let after_sleep = start_span("screen board", SpanKind::Internal, None);
    platform::fake_clock::set(WALL_0 + 28_800_005_000_000, MONO_0 + 6_000_000);
    after_sleep.end();

    let spans = test_support::drain(Signal::Traces);
    let span = named(&spans, "screen board");
    // 1_700_000_000 s + 28_800 s = 1_700_028_800 s; then the 5 ms it ran.
    assert_eq!(nanos(span, "startTimeUnixNano"), 1_700_028_800_000_000_000);
    assert_eq!(nanos(span, "endTimeUnixNano"), 1_700_028_800_005_000_000);
}

#[test]
fn log_records_are_stamped_with_the_wall_clock() {
    platform::fake_clock::set(WALL_0, MONO_0);
    test_support::reset();
    let request = start_span("PUT /api/cards/{id}", SpanKind::Client, None);

    // 1 ms later by the monotonic clock, 5 s later by the wall clock (it was
    // corrected forward). The record is a moment, not part of a duration: it
    // takes the wall clock, even though it names the open span's trace.
    platform::fake_clock::set(WALL_0 + 5_000_000_000, MONO_0 + 1_000_000);
    struct Failed(SpanContext);
    impl std::fmt::Display for Failed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "failed")
        }
    }
    impl Reportable for Failed {
        fn error_type(&self) -> String {
            "network".to_string()
        }
        fn trace(&self) -> Option<SpanContext> {
            Some(self.0)
        }
    }
    error("card save failed", &Failed(request.context().unwrap()));
    error_detail("render failed", &"detail", "render");

    let logs = test_support::drain(Signal::Logs);
    assert_eq!(logs.len(), 2);
    for log in &logs {
        assert_eq!(nanos(log, "timeUnixNano"), 1_700_000_005_000_000_000);
    }
}
