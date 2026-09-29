//! Encoding invariants. The expected JSON is written out by hand from the
//! OTLP/JSON specification — never produced by the encoder under test — so a
//! mistake in the encoder cannot also be a mistake in the oracle.

use super::*;

fn ctx(trace_byte: u8, span_byte: u8) -> SpanContext {
    SpanContext {
        trace_id: TraceId::from_random([trace_byte; 16]),
        span_id: SpanId::from_random([span_byte; 8]),
    }
}

#[test]
fn ids_are_lowercase_hex_of_the_right_length() {
    let trace = TraceId::from_random([0xAB; 16]);
    let span = SpanId::from_random([0x0F; 8]);
    assert_eq!(trace.to_hex(), "abababababababababababababababab");
    assert_eq!(span.to_hex(), "0f0f0f0f0f0f0f0f");
}

#[test]
fn all_zero_ids_are_repaired_to_valid_ones() {
    assert_ne!(TraceId::from_random([0; 16]).to_hex(), "0".repeat(32));
    assert_ne!(SpanId::from_random([0; 8]).to_hex(), "0".repeat(16));
}

#[test]
fn hex_parsing_round_trips_and_rejects_junk() {
    let trace = TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap();
    assert_eq!(trace.to_hex(), "0af7651916cd43dd8448eb211c80319c");
    // Upper case accepted, emitted lower.
    assert_eq!(
        TraceId::from_hex("0AF7651916CD43DD8448EB211C80319C")
            .unwrap()
            .to_hex(),
        "0af7651916cd43dd8448eb211c80319c"
    );
    assert!(TraceId::from_hex("0af7").is_none());
    assert!(TraceId::from_hex(&"0".repeat(32)).is_none());
    assert!(TraceId::from_hex(&"zz".repeat(16)).is_none());
    assert!(SpanId::from_hex("b7ad6b7169203331").is_some());
    assert!(SpanId::from_hex("b7ad6b716920333").is_none());
    // Multi-byte characters must not panic the byte slicing.
    assert!(SpanId::from_hex("ééééééééé").is_none());
}

#[test]
fn traceparent_is_version_00_and_always_sampled() {
    // The W3C spec's own example ids.
    let context = SpanContext {
        trace_id: TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").unwrap(),
        span_id: SpanId::from_hex("00f067aa0ba902b7").unwrap(),
    };
    assert_eq!(
        context.traceparent(),
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );
}

#[test]
fn span_encoding_uses_hex_ids_string_times_and_numeric_enums() {
    let span = Span {
        trace_id: TraceId::from_random([1; 16]),
        span_id: SpanId::from_random([2; 8]),
        parent_span_id: Some(SpanId::from_random([3; 8])),
        name: "GET /api/boards",
        kind: SpanKind::Client,
        start_time_unix_nano: UnixNanos(1_700_000_000_000_000_001),
        end_time_unix_nano: UnixNanos(1_700_000_000_000_000_002),
        attributes: vec![KeyValue::new(keys::HTTP_RESPONSE_STATUS_CODE, 200u16)],
        links: vec![],
        status: Some(ErrorStatus {
            message: "network".to_string(),
        }),
    };
    let expected = serde_json::json!({
        "traceId": "01010101010101010101010101010101",
        "spanId": "0202020202020202",
        "parentSpanId": "0303030303030303",
        "name": "GET /api/boards",
        "kind": 3,
        "startTimeUnixNano": "1700000000000000001",
        "endTimeUnixNano": "1700000000000000002",
        "attributes": [
            { "key": "http.response.status_code", "value": { "intValue": "200" } }
        ],
        "status": { "code": 2, "message": "network" }
    });
    assert_eq!(serde_json::to_value(&span).unwrap(), expected);
}

#[test]
fn a_root_span_omits_parent_links_attributes_and_status() {
    let span = Span {
        trace_id: TraceId::from_random([1; 16]),
        span_id: SpanId::from_random([2; 8]),
        parent_span_id: None,
        name: "screen board",
        kind: SpanKind::Internal,
        start_time_unix_nano: UnixNanos(1),
        end_time_unix_nano: UnixNanos(2),
        attributes: vec![],
        links: vec![],
        status: None,
    };
    let value = serde_json::to_value(&span).unwrap();
    let object = value.as_object().unwrap();
    for absent in ["parentSpanId", "attributes", "links", "status"] {
        assert!(!object.contains_key(absent), "{absent} should be omitted");
    }
    assert_eq!(object["kind"], serde_json::json!(1));
}

#[test]
fn links_encode_as_hex_ids() {
    let span = Span {
        trace_id: TraceId::from_random([1; 16]),
        span_id: SpanId::from_random([2; 8]),
        parent_span_id: None,
        name: "sse connect",
        kind: SpanKind::Internal,
        start_time_unix_nano: UnixNanos(1),
        end_time_unix_nano: UnixNanos(2),
        attributes: vec![],
        links: vec![Link {
            trace_id: TraceId::from_random([9; 16]),
            span_id: SpanId::from_random([8; 8]),
        }],
        status: None,
    };
    assert_eq!(
        serde_json::to_value(&span).unwrap()["links"],
        serde_json::json!([{
            "traceId": "09090909090909090909090909090909",
            "spanId": "0808080808080808"
        }])
    );
}

#[test]
fn log_record_carries_its_span_and_numeric_severity() {
    let record = LogRecord {
        time: UnixNanos(42),
        severity: Severity::Error,
        body: "failed to fetch columns",
        attributes: vec![KeyValue::new(keys::ERROR_TYPE, "network")],
        context: Some(ctx(5, 6)),
    };
    assert_eq!(
        serde_json::to_value(&record).unwrap(),
        serde_json::json!({
            "timeUnixNano": "42",
            "severityNumber": 17,
            "severityText": "ERROR",
            "body": { "stringValue": "failed to fetch columns" },
            "attributes": [ { "key": "error.type", "value": { "stringValue": "network" } } ],
            "traceId": "05050505050505050505050505050505",
            "spanId": "0606060606060606"
        })
    );
}

#[test]
fn a_log_record_outside_any_span_has_no_ids() {
    let record = LogRecord {
        time: UnixNanos(1),
        severity: Severity::Error,
        body: "x",
        attributes: vec![],
        context: None,
    };
    let value = serde_json::to_value(&record).unwrap();
    assert!(value.get("traceId").is_none());
    assert!(value.get("spanId").is_none());
    assert!(value.get("attributes").is_none());
}

#[test]
fn the_servers_stream_trace_event_parses_into_a_link_target() {
    let context = parse_stream_trace(
        r#"{"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"00f067aa0ba902b7"}"#,
    )
    .unwrap();
    assert_eq!(
        context.trace_id.to_hex(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_eq!(context.span_id.to_hex(), "00f067aa0ba902b7");
    for junk in [
        "",
        "not json",
        "{}",
        r#"{"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"}"#,
        r#"{"trace_id":"short","span_id":"00f067aa0ba902b7"}"#,
        r#"{"trace_id":7,"span_id":"00f067aa0ba902b7"}"#,
    ] {
        assert_eq!(parse_stream_trace(junk), None, "{junk:?}");
    }
}

#[test]
fn resource_states_name_version_and_sdk_but_no_identity() {
    let resource = resource("1.64.0");
    let attributes = resource["attributes"].as_array().unwrap();
    let get = |key: &str| {
        attributes
            .iter()
            .find(|attribute| attribute["key"] == key)
            .map(|attribute| attribute["value"]["stringValue"].clone())
    };
    assert_eq!(get("service.name"), Some(serde_json::json!("bored-spa")));
    assert_eq!(get("service.version"), Some(serde_json::json!("1.64.0")));
    assert_eq!(
        get("telemetry.sdk.language"),
        Some(serde_json::json!("rust"))
    );
    assert!(get("telemetry.sdk.name").is_some());
    assert!(get("telemetry.sdk.version").is_some());
    // What the ingest stamps must never come from the client.
    for stamped in [
        "deployment.environment.name",
        "telemetry_source",
        "user.id",
        "user.name",
        "user.email",
        "user.full_name",
        "session.id",
    ] {
        assert_eq!(get(stamped), None, "{stamped} must be left to the ingest");
    }
}

#[test]
fn envelope_is_a_valid_export_request_holding_the_items() {
    let span = serde_json::to_string(&Span {
        trace_id: TraceId::from_random([1; 16]),
        span_id: SpanId::from_random([2; 8]),
        parent_span_id: None,
        name: "a",
        kind: SpanKind::Internal,
        start_time_unix_nano: UnixNanos(1),
        end_time_unix_nano: UnixNanos(2),
        attributes: vec![],
        links: vec![],
        status: None,
    })
    .unwrap();
    let body = envelope(Signal::Traces, &[span.clone(), span], "9.9.9");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    let spans = &parsed["resourceSpans"][0]["scopeSpans"][0]["spans"];
    assert_eq!(spans.as_array().unwrap().len(), 2);
    assert_eq!(
        parsed["resourceSpans"][0]["scopeSpans"][0]["scope"]["name"],
        "bored-spa"
    );

    let logs = envelope(Signal::Logs, &[], "9.9.9");
    let parsed: serde_json::Value = serde_json::from_str(&logs).unwrap();
    assert!(parsed["resourceLogs"][0]["scopeLogs"][0]["logRecords"].is_array());
}

#[test]
fn envelope_overhead_is_exact() {
    // The oracle is the length of the real body, measured — not the formula.
    let items = vec!["{}".to_string(), "{}".to_string(), "{}".to_string()];
    let body = envelope(Signal::Logs, &items, "1.0.0");
    let item_bytes: usize = items.iter().map(String::len).sum();
    assert_eq!(
        envelope_overhead(Signal::Logs, "1.0.0", items.len()) + item_bytes,
        body.len()
    );
}

#[test]
fn signal_paths_are_otlps_own() {
    assert_eq!(Signal::Traces.path(), "/v1/traces");
    assert_eq!(Signal::Logs.path(), "/v1/logs");
}

#[test]
fn every_attribute_key_is_semconv_or_prefixed() {
    // The semantic-convention namespaces this SPA uses; anything of its own
    // must carry `bored.`.
    let semconv = [
        "service.",
        "telemetry.sdk.",
        "http.",
        "url.",
        "error.",
        "exception.",
        "code.",
    ];
    for key in keys::ALL {
        assert!(
            key.starts_with("bored.") || semconv.iter().any(|prefix| key.starts_with(prefix)),
            "{key} is neither a semantic-convention name nor bored.-prefixed"
        );
    }
}
