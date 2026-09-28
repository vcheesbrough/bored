//! The backend's outbound HTTP: one shared client (card #120), and what its
//! failures may say in a log line (card #366).
//!
//! Every test here runs the real auth components against a fake identity
//! provider — a small axum app on a real loopback TCP port, because
//! connection reuse and connect failures only exist on a real socket.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    routing::{get, post},
};
use base64::Engine;

use super::*;
use crate::auth::validate_jwt;

/// A value that must never appear in a log line. The fake provider puts it
/// where a careless error message would echo it.
const SENSITIVE: &str = "SECRET-token-9c1e";

/// What the fake identity provider serves, and what it observed.
struct FakeIdp {
    /// Its own base URL, so the discovery document can point back at it.
    base: String,
    /// Every client socket address a request arrived from. A TCP connection
    /// has one fixed client port, so the number of *distinct* addresses is
    /// the number of connections the backend opened — counted by the server,
    /// independently of anything the client reports about its pool.
    peers: Mutex<HashSet<SocketAddr>>,
    /// The JWKS response body, so a test can serve malformed JSON.
    jwks_body: String,
    /// The token endpoint's response body, likewise.
    token_body: String,
}

impl FakeIdp {
    /// Record the connection a request came in on.
    fn saw(&self, peer: SocketAddr) {
        self.peers.lock().expect("peers lock").insert(peer);
    }

    fn connections(&self) -> usize {
        self.peers.lock().expect("peers lock").len()
    }
}

/// A token response the session manager accepts.
fn valid_token_body() -> String {
    serde_json::json!({
        "access_token": "access",
        "refresh_token": "refresh",
        "id_token": "id",
        "expires_in": 900,
    })
    .to_string()
}

/// Start a fake provider on an ephemeral loopback port and return a handle
/// to what it observes. The server task runs until the test's runtime ends.
async fn start_idp(jwks_body: &str, token_body: &str) -> Arc<FakeIdp> {
    // Port 0 asks the OS for any free port; `local_addr` says which it chose.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let idp = Arc::new(FakeIdp {
        base: format!("http://{addr}"),
        peers: Mutex::new(HashSet::new()),
        jwks_body: jwks_body.to_string(),
        token_body: token_body.to_string(),
    });

    // Each handler records its caller's socket address. `ConnectInfo` is
    // filled in per connection by `into_make_service_with_connect_info`.
    async fn discovery(
        State(idp): State<Arc<FakeIdp>>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ) -> Json<serde_json::Value> {
        idp.saw(peer);
        let base = &idp.base;
        Json(serde_json::json!({
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "jwks_uri": format!("{base}/jwks"),
            "revocation_endpoint": format!("{base}/revoke"),
        }))
    }
    async fn jwks(
        State(idp): State<Arc<FakeIdp>>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ) -> ([(&'static str, &'static str); 1], String) {
        idp.saw(peer);
        (
            [("content-type", "application/json")],
            idp.jwks_body.clone(),
        )
    }
    async fn token(
        State(idp): State<Arc<FakeIdp>>,
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ) -> ([(&'static str, &'static str); 1], String) {
        idp.saw(peer);
        (
            [("content-type", "application/json")],
            idp.token_body.clone(),
        )
    }
    async fn revoke(State(idp): State<Arc<FakeIdp>>, ConnectInfo(peer): ConnectInfo<SocketAddr>) {
        idp.saw(peer);
    }

    let router = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .route("/revoke", post(revoke))
        .with_state(Arc::clone(&idp));
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("fake idp serves");
    });
    idp
}

/// Enable auth on a fresh `AppState` exactly as `main` does, pointed at the
/// fake provider.
async fn state_against(idp: &FakeIdp) -> AppState {
    let db = db::connect_mem().await.expect("mem db");
    let oidc = config::OidcConfig {
        issuer_url: idp.base.clone(),
        client_id: "bored".to_string(),
        client_secret: "client-secret".to_string(),
        redirect_uri: "http://localhost/auth/callback".to_string(),
        required_scope: "bored:test:access".to_string(),
        end_session_url: None,
        mcp: config::OidcMcpConfig {
            issuer_url: None,
            client_id: None,
        },
    };
    let session = config::SessionConfig {
        // 64 bytes of key material, base64-encoded, as the config demands.
        cookie_key: base64::engine::general_purpose::STANDARD.encode([7_u8; 64]),
    };
    with_oidc(AppState::new(db), &oidc, &session).await
}

/// A token whose header names a `kid` the cache has never seen, so
/// validating it forces a JWKS fetch. Signature and claims are never reached.
fn token_with_unknown_kid() -> String {
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    format!(
        "{}.{}.signature",
        encode(serde_json::json!({ "alg": "RS256", "kid": "unknown-kid" })),
        encode(serde_json::json!({ "sub": "user" }))
    )
}

/// Card #120: discovery, the JWKS fetch, token exchange and revocation all go
/// out on the one shared client, so four sequential calls to one provider
/// ride **one** kept-alive connection. Before the change the backend held
/// three clients (plus a throwaway one per discovery attempt), and the same
/// sequence opened three connections.
#[tokio::test]
async fn every_outbound_auth_call_shares_one_connection() {
    let idp = start_idp(r#"{"keys":[]}"#, &valid_token_body()).await;
    let state = state_against(&idp).await;
    let auth = state.auth.as_ref().expect("auth enabled");
    let jwks = state.jwks_cache.as_ref().expect("jwks cache");
    let sessions = state.auth_sessions.as_ref().expect("session manager");

    // 1. Discovery already ran inside `state_against`.
    // 2. Token exchange, through the session manager.
    sessions
        .exchange_authorization_code(auth, "code")
        .await
        .expect("token exchange succeeds");
    // 3. JWKS fetch, through the cache (the kid stays unknown afterwards).
    assert_eq!(
        validate_jwt(&token_with_unknown_kid(), auth, jwks)
            .await
            .err(),
        Some("JWT kid not in JWKS"),
        "the cache must have fetched the (empty) key set"
    );
    // 4. Revocation, through the session manager again.
    sessions
        .revoke_refresh_token(auth, "refresh")
        .await
        .expect("revocation succeeds");

    assert_eq!(
        idp.connections(),
        1,
        "discovery, token, JWKS and revocation must share one pooled connection"
    );
}

/// Card #366: a JWKS response that fails to parse is logged as a `decode`
/// failure with the endpoint's host and path — not with serde's message,
/// which quotes the body it choked on.
#[tokio::test]
async fn a_malformed_jwks_body_is_not_quoted_in_the_log() {
    // `keys` must be an array; a string makes serde say
    // `invalid type: string "SECRET…", expected a sequence`.
    let idp = start_idp(&format!(r#"{{"keys":"{SENSITIVE}"}}"#), &valid_token_body()).await;
    let state = state_against(&idp).await;
    let auth = state.auth.as_ref().expect("auth enabled");
    let jwks = state.jwks_cache.as_ref().expect("jwks cache");

    let token = token_with_unknown_kid();
    let (result, logs) = capture_logs_async(validate_jwt(&token, auth, jwks)).await;

    assert!(result.is_err(), "a token cannot validate against no keys");
    assert!(
        logs.contains("JWKS refresh failed"),
        "the failure is logged: {logs}"
    );
    assert!(logs.contains("decode"), "the failure kind is named: {logs}");
    assert!(
        logs.contains(&format!("{}/jwks", idp.base)),
        "the endpoint's host and path are kept: {logs}"
    );
    assert!(
        !logs.contains(SENSITIVE),
        "the response body leaked: {logs}"
    );
}

/// Card #366: a token response that fails to parse is reported without its
/// content. This is the one that matters most — the body is a token response.
#[tokio::test]
async fn a_malformed_token_response_is_not_quoted_in_the_error() {
    // `expires_in` must be a number; a string is quoted back by serde.
    let token_body = serde_json::json!({
        "access_token": "access",
        "refresh_token": "refresh",
        "id_token": "id",
        "expires_in": SENSITIVE,
    })
    .to_string();
    let idp = start_idp(r#"{"keys":[]}"#, &token_body).await;
    let state = state_against(&idp).await;
    let auth = state.auth.as_ref().expect("auth enabled");
    let sessions = state.auth_sessions.as_ref().expect("session manager");

    let error = sessions
        .exchange_authorization_code(auth, "code")
        .await
        .expect_err("the token response does not parse");

    // This string is what `routes::auth::callback` logs verbatim.
    assert!(
        error.contains("decode"),
        "the failure kind is named: {error}"
    );
    assert!(
        !error.contains(SENSITIVE),
        "the token response leaked: {error}"
    );
}

/// Card #366: a connect failure keeps the transport's own cause and the
/// URL's host and path, and drops the query string reqwest would otherwise
/// print as part of the URL.
#[tokio::test]
async fn a_connect_failure_names_host_and_path_but_not_the_query() {
    // Bind then drop a listener: the port was free a moment ago and nothing
    // listens on it now, so the connect is refused rather than timing out.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let url = format!("http://127.0.0.1:{port}/token?code={SENSITIVE}");

    let error = crate::http_client::build()
        .get(&url)
        .send()
        .await
        .expect_err("nothing is listening");
    assert!(
        error.to_string().contains(SENSITIVE),
        "premise: reqwest's own message carries the full URL, got: {error}"
    );

    let described = crate::redact::http_error(&error);

    assert!(described.starts_with("connect"), "kind first: {described}");
    assert!(
        described.contains(&format!("url=http://127.0.0.1:{port}/token")),
        "host and path kept: {described}"
    );
    assert!(!described.contains(SENSITIVE), "query leaked: {described}");
    assert!(!described.contains('?'), "no query at all: {described}");
}

/// Card #366, through discovery itself: a failed attempt's WARN line and the
/// error that becomes the startup panic both name the discovery endpoint's
/// host and path, and neither carries the query string. An issuer URL with a
/// query is contrived, but it is the one way a query reaches this call site —
/// and the fake answers 404, so the status-failure branch runs.
#[tokio::test]
async fn a_failed_discovery_logs_host_and_path_but_not_the_query() {
    let idp = start_idp(r#"{"keys":[]}"#, &valid_token_body()).await;
    // `discover` appends `/.well-known/openid-configuration` to this, so the
    // request target's query is `token=SECRET…/.well-known/…` and its path is
    // `/o`, which the fake does not serve.
    let issuer = format!("{}/o?token={SENSITIVE}", idp.base);
    let http = crate::http_client::build();

    let (result, logs) = capture_logs_async(crate::auth::AuthConfig::discover_with_retries(
        &issuer,
        &http,
        1,
        std::time::Duration::ZERO,
    ))
    .await;
    // `.err()` turns the `Result` into an `Option` of its error; the doc
    // type is not `Debug`, so `expect_err` is not available.
    let error = result.err().expect("the fake has no document at /o");

    assert!(
        logs.contains("OIDC discovery attempt failed"),
        "the attempt is logged: {logs}"
    );
    assert!(logs.contains("status=404"), "the status is kept: {logs}");
    assert!(
        logs.contains(&format!("{}/o", idp.base)),
        "host and path kept: {logs}"
    );
    assert!(
        !logs.contains(SENSITIVE),
        "query leaked into the log: {logs}"
    );
    // The error string is what `AuthConfig::from_config` panics with.
    assert!(error.contains("status=404"), "the status is kept: {error}");
    assert!(
        !error.contains(SENSITIVE),
        "query leaked into the error: {error}"
    );
}

/// Card #366, at the call site: the callback's WARN line carries the IdP's
/// `error` parameter only when it is shaped like an OAuth error code. The
/// parameter arrives in a query string anyone can type.
#[tokio::test]
async fn the_callback_logs_an_oauth_error_code_but_not_free_text() {
    let idp = start_idp(r#"{"keys":[]}"#, &valid_token_body()).await;
    let server = TestServer::new(
        app(
            state_against(&idp).await,
            "./dist",
            DeploymentInfo::new("dev", None),
        )
        .await,
    )
    .unwrap();

    // A genuine code is kept — the diagnostic an operator needs.
    let (response, logs) = capture_logs_async(async {
        server
            .get("/auth/callback?state=s&error=access_denied")
            .await
    })
    .await;
    response.assert_status(StatusCode::FORBIDDEN);
    assert!(
        logs.contains("auth callback received error") && logs.contains("access_denied"),
        "the OAuth error code is logged: {logs}"
    );

    // Free text is not. `%20` is a space, which no error code contains.
    let (response, logs) = capture_logs_async(async {
        server
            .get(&format!(
                "/auth/callback?state=s&error=denied%20{SENSITIVE}"
            ))
            .await
    })
    .await;
    response.assert_status(StatusCode::FORBIDDEN);
    assert!(
        logs.contains("auth callback received error"),
        "the failure is still logged: {logs}"
    );
    assert!(!logs.contains(SENSITIVE), "free text leaked: {logs}");
}

/// Card #120: the shared client gives up on a provider that accepts the
/// connection but never answers, instead of hanging the request (and, on the
/// JWKS path, every request waiting on the cache) forever. Before the change,
/// discovery and the JWKS fetch used clients with no timeout at all.
///
/// Drives `AppState::http` — the client production uses — against a socket
/// that accepts and then stays silent, and measures the wait with the test's
/// own clock. The bounds bracket the 10 s request timeout: not before it
/// (the timeout is not shorter than configured), and well before a hang.
#[tokio::test]
async fn the_shared_client_times_out_on_a_silent_provider() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("addr");
    // Accept connections and hold them open without ever writing a byte.
    // The accepted sockets are kept in a Vec so they are not dropped (which
    // would close them and turn the hang into a quick connection reset).
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });

    let state = AppState::new(db::connect_mem().await.expect("mem db"));
    let started = std::time::Instant::now();
    let error = state
        .http
        .get(format!("http://{addr}/jwks"))
        .send()
        .await
        .expect_err("a silent provider cannot answer");
    let waited = started.elapsed();

    assert!(
        crate::redact::http_error(&error).starts_with("timeout"),
        "the failure is reported as a timeout: {}",
        crate::redact::http_error(&error)
    );
    assert!(
        waited >= std::time::Duration::from_secs(9),
        "gave up after {waited:?}, before the 10 s timeout"
    );
    assert!(
        waited < std::time::Duration::from_secs(20),
        "waited {waited:?}; the 10 s timeout did not apply"
    );
}

/// Card #415: every authentication decision on a protected request is counted
/// in `bored.auth.outcomes`, labelled by outcome. Driven through the real
/// router and middleware with auth enabled against the fake provider; the
/// counters are cumulative and shared by every test in the binary, so the
/// assertion is on the *increase* each request causes.
#[tokio::test]
async fn auth_decisions_are_counted_by_outcome() {
    use crate::observability::metrics::{AUTH_OUTCOMES, BORED_AUTH_OUTCOME};
    use crate::observability::test_support::counter_value;

    let idp = start_idp(r#"{"keys":[]}"#, &valid_token_body()).await;
    let state = state_against(&idp).await;
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("test", None)).await).unwrap();
    let count = |outcome: &str| counter_value(AUTH_OUTCOMES, BORED_AUTH_OUTCOME, outcome);

    let rejected_before = count("bearer_rejected");
    let missing_before = count("missing");

    // A bearer token the cache cannot validate…
    server
        .get("/api/boards")
        .add_header(
            "authorization",
            format!("Bearer {}", token_with_unknown_kid()),
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // …and a browser with no credentials at all.
    server
        .get("/api/boards")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    assert!(
        count("bearer_rejected") > rejected_before,
        "bearer rejection not counted"
    );
    assert!(
        count("missing") > missing_before,
        "missing credential not counted"
    );
}
