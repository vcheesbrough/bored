//! The server side of HTTP telemetry: one span per request, adopting the
//! caller's trace context, and the request-duration metric.
//!
//! This is the transport layer's half of the contract (skill §5: "context
//! propagation belongs to the transport layer"). It knows HTTP; it does not
//! know OpenTelemetry's SDK — the propagation itself is
//! `observability::adopt_parent`.
//!
//! Two pieces, wired in `app.rs`:
//!
//! - [`ServerSpan`] is tower-http `TraceLayer`'s span factory. It opens the
//!   `server` span with the semantic-convention attributes known up front, and
//!   parents it on the `traceparent` Traefik sends, so the request joins the
//!   trace that starts at the edge.
//! - [`record_response`] is a middleware *inside* that span. It sees the
//!   response, records its status and `error.type` on the span, and
//!   records `http.server.request.duration`.

use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use tower_http::trace::MakeSpan;
use tracing::field::Empty;

use crate::error::ErrorType;
use crate::observability::metrics::{HttpMethod, RequestError};
use crate::observability::{self, metrics};

/// `TraceLayer`'s span factory: one `server` span per request.
#[derive(Clone, Copy, Default)]
pub(crate) struct ServerSpan;

impl<B> MakeSpan<B> for ServerSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        // The router's *template* for the matched route (`/api/cards/:id`),
        // present because `TraceLayer` is applied with `Router::layer`, which
        // runs after routing. `None` for the SPA fallback. Never the raw path:
        // the template is what groups requests; the path is on `url.path`.
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str);
        let method = HttpMethod::from_method(request.method());

        // semconv span name: `{method} {route}`, or just the method when no
        // route matched (a raw path in the *name* would make every card its own
        // operation in Tempo).
        let name = match route {
            Some(route) => format!("{} {route}", method.label()),
            None => method.label().to_string(),
        };

        // Keys are literals because `tracing` macros accept nothing else; each
        // is a semantic-convention name (or one of the span bridge's reserved
        // `otel.*` fields), which `observability::tests` checks on every key
        // an exporter receives. `Empty` declares a field now so it can be
        // recorded later — `tracing` cannot add a field to a span after it is
        // created.
        //
        // Level INFO (tower-http's default is DEBUG): the span layer is never
        // level-filtered, but the fmt layer is, and at DEBUG an `info`-level
        // deployment would write events raised inside the request (the
        // `request failed` line in error.rs, for one) with no request context.
        let span = tracing::info_span!(
            "HTTP request",
            otel.name = %name,
            otel.kind = "server",
            otel.status_code = Empty,
            http.request.method = method.label(),
            http.route = route,
            url.path = %request.uri().path(),
            // Only when the request says so. A server sees an origin-form URI
            // (`/api/…`) with no scheme, and this span factory cannot tell the
            // TLS listener from the plain-HTTP one, so it records nothing
            // rather than a guess.
            url.scheme = request.uri().scheme_str(),
            http.response.status_code = Empty,
            error.type = Empty,
        );
        observability::adopt_parent(&span, request.headers());
        span
    }
}

/// Record the response on the request span and in the duration histogram.
///
/// Measures to the moment the handler produced its response head. For every
/// JSON route that is the whole request (the body is already in memory); for
/// `/api/events` it is the time to open the stream, which is the useful
/// number — the stream itself lasts as long as the tab is open, and its
/// lifetime is the SSE span's (events.rs), not a latency.
pub(crate) async fn record_response(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = HttpMethod::from_method(request.method());
    // Owned copy: the request is moved into `next.run` below.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());

    let response = next.run(request).await;

    let status = response.status().as_u16();
    // The class `ApiError::into_response` attached, if a handler failed with
    // one (card #366's `ErrorType`); `_OTHER` for an unclassified 5xx.
    let classified = response.extensions().get::<ErrorType>().copied();
    let error = RequestError::for_response(status, classified);
    // `Span::current()` is the request span: `TraceLayer` runs this whole
    // middleware inside it.
    let span = tracing::Span::current();
    // As an `i64`: the span bridge records a `u64` field as a string, and
    // semconv types the status code as an int.
    span.record("http.response.status_code", i64::from(status));
    if let Some(error) = error {
        span.record("error.type", error.label());
    }
    // Span status follows semconv's server rule: only a 5xx is an error of
    // this service. A 404 or 409 keeps its `error.type` (it says *which*
    // client error) but leaves the span's status unset.
    if status >= 500 {
        span.record("otel.status_code", "ERROR");
    }
    metrics::http_request(
        method,
        route.as_deref(),
        status,
        error,
        started.elapsed().as_secs_f64(),
    );
    response
}
