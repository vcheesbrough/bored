//! The backend's one outbound HTTP client (card #120).
//!
//! Every call the backend makes to another service — OIDC discovery, the JWKS
//! fetch, the token exchange, token refresh and revocation — goes through the
//! single `reqwest::Client` built here and stored as `AppState::http`.
//!
//! Why one client rather than one per feature:
//!
//! * **Connection reuse.** A `reqwest::Client` owns a connection pool. Before
//!   this module, discovery built a fresh client (and pool) per attempt and
//!   the JWKS cache and the session manager each kept their own, so the same
//!   identity-provider host was dialled — TLS handshake and all — by three
//!   pools. One client means one pool per host.
//! * **One place for transport policy.** Timeouts live here, so no outbound
//!   call can hang a request forever because its author forgot one (the JWKS
//!   fetch and discovery had none).
//! * **One place for instrumentation.** The `observability` skill puts the
//!   outbound `http.client` span and `traceparent` injection in the transport
//!   layer; #415 adds them by wrapping this client, not by visiting every
//!   call site.
//!
//! Cloning a `reqwest::Client` is cheap and shares the pool: internally it is
//! an `Arc` around the real client, so `clone()` bumps a reference count.
//! Components that need the client keep their own clone of `AppState::http`.

use std::time::Duration;

/// How long to wait for a TCP + TLS connection to be established.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The ceiling on a whole request, from sending to the last body byte. Every
/// call the backend makes is a small JSON exchange with the identity
/// provider, so 10 s is generous; it is the value the session manager's own
/// client already used before the clients were merged.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the shared client. Called once per `AppState`.
///
/// `builder().build()` only fails if the TLS backend cannot initialise —
/// a broken build, not a runtime condition — so it panics with a message
/// rather than pushing a `Result` through every constructor.
pub fn build() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build the shared outbound HTTP client")
}

// ─────────────────────────────────────────────────────────────────────────────
// Instrumentation (card #415)
// ─────────────────────────────────────────────────────────────────────────────

/// Which outbound call this is. A fixed set, so the span's name and the
/// `bored.http.operation` attribute can never carry a URL or anything else
/// computed at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outbound {
    /// `/.well-known/openid-configuration` at startup.
    OidcDiscovery,
    /// The identity provider's signing keys.
    Jwks,
    /// Authorization-code exchange or refresh at the token endpoint.
    TokenExchange,
    /// Refresh-token revocation at logout.
    Revocation,
}

impl Outbound {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Outbound::OidcDiscovery => "oidc.discovery",
            Outbound::Jwks => "oidc.jwks",
            Outbound::TokenExchange => "oidc.token",
            Outbound::Revocation => "oidc.revocation",
        }
    }
}

/// Send `request` with a `client` span around it and the current trace
/// context injected as `traceparent`.
///
/// Call it in place of `.send()`:
///
/// ```ignore
/// http_client::send(Outbound::Jwks, self.http.get(&self.jwks_url)).await
/// ```
///
/// The span records what semconv asks of an HTTP client span — method,
/// `server.address`/`server.port`, `url.full` (scheme, host and path only: the
/// query can carry a token, so it is dropped, as `redact::url_parts` does for
/// logs), the response status, and `error.type` on failure — plus
/// `bored.http.operation`. Nothing from the request or response body.
///
/// It returns exactly what `.send()` would, so each caller keeps its own
/// error handling (and its `redact::http_error` descriptions) unchanged.
pub(crate) async fn send(
    operation: Outbound,
    request: reqwest::RequestBuilder,
) -> reqwest::Result<reqwest::Response> {
    use tracing::Instrument as _;
    use tracing::field::Empty;

    // `build_split` hands back the client and the finished request separately,
    // so the request's headers can be edited (the `traceparent` below) before
    // it is sent on that same client.
    let (client, request) = request.build_split();
    let mut request = request?;

    let method = request.method().as_str().to_owned();
    let url = request.url();
    let span = tracing::info_span!(
        "HTTP client",
        otel.name = %format!("{method} {}", operation.label()),
        otel.kind = "client",
        otel.status_code = Empty,
        http.request.method = %method,
        server.address = url.host_str().unwrap_or(""),
        server.port = url.port_or_known_default().map(i64::from),
        url.full = %crate::redact::url_parts(url),
        bored.http.operation = operation.label(),
        http.response.status_code = Empty,
        error.type = Empty,
    );

    async move {
        // Inside the span, so the injected context names *this* span as the
        // parent of whatever the identity provider records.
        crate::observability::inject_current(request.headers_mut());
        let span = tracing::Span::current();
        let result = client.execute(request).await;
        match &result {
            Ok(response) => {
                let status = response.status().as_u16();
                span.record("http.response.status_code", i64::from(status));
                // semconv: a 4xx/5xx *is* an error for a client span, labelled
                // by its status code.
                if status >= 400 {
                    span.record("error.type", status_class(status));
                    span.record("otel.status_code", "ERROR");
                }
            }
            Err(error) => {
                span.record("error.type", transport_error_class(error));
                span.record("otel.status_code", "ERROR");
            }
        }
        result
    }
    .instrument(span)
    .await
}

/// `error.type` for an HTTP error status: the code itself, as semconv asks.
/// A `&'static str` from a fixed table, falling back to the class, so the
/// value set stays bounded whatever a provider answers.
fn status_class(status: u16) -> &'static str {
    // The codes an identity provider plausibly answers with, each named.
    const NAMED: &[(u16, &str)] = &[
        (400, "400"),
        (401, "401"),
        (403, "403"),
        (404, "404"),
        (429, "429"),
        (500, "500"),
        (502, "502"),
        (503, "503"),
        (504, "504"),
    ];
    NAMED
        .iter()
        .find(|(code, _)| *code == status)
        .map(|(_, label)| *label)
        .unwrap_or(if status < 500 { "4xx" } else { "5xx" })
}

/// `error.type` for a request that never got a response.
fn transport_error_class(error: &reqwest::Error) -> &'static str {
    // The same order `redact::http_error` uses: a connect timeout reports both
    // `is_timeout` and `is_connect`, and "timeout" says more.
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else {
        "_OTHER"
    }
}
