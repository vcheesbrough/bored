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
