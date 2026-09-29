//! Router-level behaviour: health, `/api/info`, the SPA fallback and API 404s.

use super::*;
use crate::app::health;
use serial_test::serial;

#[tokio::test]
async fn health_handler_returns_ok() {
    assert_eq!(health().await, "ok");
}

#[tokio::test]
async fn health_route_returns_200() {
    let server = test_app().await;
    let response = server.get("/health").await;
    response.assert_status(StatusCode::OK);
}

#[tokio::test]
#[serial]
async fn info_route_returns_version_and_env() {
    // SAFETY: env mutation happens strictly before `test_app()` creates any
    // db/runtime background tasks that could race a `getenv`. `#[serial]`
    // additionally serialises this against the other `#[serial]` tests here
    // (it does not by itself exclude other threads).
    unsafe { std::env::remove_var("APP_VERSION") };
    let server = test_app().await;
    let resp = server.get("/api/info").await;
    resp.assert_status_ok();
    let info: shared::AppInfo = resp.json();
    // Falls back to shared::app_version (burned-in RELEASE_TAG, else
    // CARGO_PKG_VERSION) when APP_VERSION is unset.
    //
    // Asserted as an equality, not merely non-empty: the frontend reloads
    // itself whenever this value differs from the `shared::app_version()`
    // compiled into its own bundle (see `frontend/src/connection.rs`). With
    // APP_VERSION unset — which is how the image runs — the two are the same
    // string from the same build, and that is what makes the check quiet.
    assert_eq!(info.version, shared::app_version());
    // `test_app()` passes "dev" as the environment, with no branch.
    assert_eq!(info.env, "dev");
    // A deployment with no branch reports none, rather than an empty string the
    // watermark would then have to special-case (card #412).
    assert_eq!(info.branch, None);
}

// The prod shape: environment `prod`, no branch. Pins that `env` carries the
// environment proper — before card #412 prod reported the string "production"
// and dev reported its branch name here.
#[tokio::test]
#[serial]
async fn info_route_reports_prod_environment_without_a_branch() {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    let router = app(state, "./dist", DeploymentInfo::new("prod", None)).await;
    let server = TestServer::new(router).unwrap();
    let resp = server.get("/api/info").await;
    resp.assert_status_ok();
    let info: shared::AppInfo = resp.json();
    assert_eq!(info.env, "prod");
    assert_eq!(info.branch, None);
}

// The dev shape: the environment stays `dev` across every branch — that is the
// whole point of the split — while the branch rides alongside it for the board
// watermark.
#[tokio::test]
#[serial]
async fn info_route_reports_branch_alongside_dev_environment() {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    let router = app(
        state,
        "./dist",
        DeploymentInfo::new(
            "dev",
            Some("feat/iteration-61-loki-stdout-labels".to_string()),
        ),
    )
    .await;
    let server = TestServer::new(router).unwrap();
    let resp = server.get("/api/info").await;
    resp.assert_status_ok();
    let info: shared::AppInfo = resp.json();
    assert_eq!(info.env, "dev");
    // Reported verbatim; trimming the `feat/` prefix is the frontend's job
    // (`watermark_label`), not the API's.
    assert_eq!(
        info.branch.as_deref(),
        Some("feat/iteration-61-loki-stdout-labels")
    );
}

// A bundle built before `branch` existed can be talking to a server built
// after it, and vice versa — the reload-on-deploy poll is exactly the request
// that spans the skew window. Deserializing a body with no `branch` key must
// therefore succeed rather than stall that poll.
#[test]
fn app_info_without_a_branch_field_still_deserializes() {
    let info: shared::AppInfo =
        serde_json::from_str(r#"{"version":"1.60.0","env":"production"}"#).expect("valid AppInfo");
    assert_eq!(info.version, "1.60.0");
    assert_eq!(info.branch, None);
}

#[tokio::test]
#[serial]
async fn info_route_uses_app_version_env_and_configured_environment() {
    // SAFETY: env mutation happens strictly before `db::connect_mem()` spawns
    // SurrealDB's background tasks that could race a `getenv`. `#[serial]`
    // additionally serialises this against the other `#[serial]` tests here
    // (it does not by itself exclude other threads).
    unsafe { std::env::set_var("APP_VERSION", "1.2.3") };
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    // The deployment identity is threaded through `app()` directly (from
    // `ObservabilityConfig` in production) rather than a raw env var.
    let router = app(state, "./dist", DeploymentInfo::new("prod", None)).await;
    let server = TestServer::new(router).unwrap();
    let resp = server.get("/api/info").await;
    resp.assert_status_ok();
    let body = resp.text();
    drop(server);
    // SAFETY: `server` (and the db/state it owns, including SurrealDB's
    // background tasks) was dropped just above, so no task from this test
    // can race this mutation. `#[serial]` additionally serialises this
    // against the other `#[serial]` tests here.
    unsafe { std::env::remove_var("APP_VERSION") };
    let info: shared::AppInfo = serde_json::from_str(&body).expect("valid AppInfo JSON");
    assert_eq!(info.version, "1.2.3");
    assert_eq!(info.env, "prod");
}

// Verifies that a deep-link path (e.g. /boards/abc) returns 200 with index.html
// rather than 404 when the SPA fallback is active.
#[tokio::test]
async fn spa_deep_link_returns_index_html() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), b"<html></html>").unwrap();
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    // `static_dir` is threaded through `app()` directly (from
    // `ServerConfig` in production) rather than a raw env var.
    let router = app(
        state,
        dir.path().to_str().unwrap(),
        DeploymentInfo::new("dev", None),
    )
    .await;
    let server = TestServer::new(router).unwrap();
    let resp = server.get("/boards/some-deep-link").await;
    resp.assert_status(StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");
    assert!(resp.text().contains("<html>"));
}

// The SPA document must be revalidated on every load, whether it arrives via
// the deep-link fallback or straight off disk at `/`. Without this a browser
// may reuse a cached `index.html` — whose URL, unlike the fingerprinted wasm
// and JS it links to, is identical across deploys — and hand the
// version-triggered reload in `frontend/src/connection.rs` the same stale
// bundle it was trying to escape.
#[tokio::test]
async fn spa_document_is_served_with_cache_control_no_cache() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), b"<html></html>").unwrap();
    // A fingerprinted asset alongside it, to prove the header is scoped to the
    // document and does not defeat caching of everything else.
    std::fs::write(dir.path().join("app-abc123.js"), b"console.log(1)").unwrap();
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    let router = app(
        state,
        dir.path().to_str().unwrap(),
        DeploymentInfo::new("dev", None),
    )
    .await;
    let server = TestServer::new(router).unwrap();

    // Served by ServeDir itself.
    let root = server.get("/").await;
    root.assert_status(StatusCode::OK);
    assert_eq!(root.headers()["cache-control"], "no-cache");

    // Served by the 404 fallback.
    let deep = server.get("/boards/some-deep-link").await;
    deep.assert_status(StatusCode::OK);
    assert_eq!(deep.headers()["cache-control"], "no-cache");

    let asset = server.get("/app-abc123.js").await;
    asset.assert_status(StatusCode::OK);
    assert!(
        !asset.headers().contains_key("cache-control"),
        "fingerprinted assets keep ServeDir's caching behaviour"
    );
}

// Verifies that unknown /api/* paths return 404 from the nested router and are
// not swallowed by the SPA fallback, which only applies outside /api/*.
#[tokio::test]
#[serial]
async fn api_unknown_route_returns_404_not_spa_fallback() {
    let server = test_app().await;
    let resp = server.get("/api/nonexistent").await;
    resp.assert_status(StatusCode::NOT_FOUND);
}

// ── Client telemetry configuration (card #416) ─────────────────────────────

/// An app with browser auth switched on, built without any network: the
/// provider endpoints are fixed strings and nothing here fetches the JWKS.
async fn auth_enabled_app(deployment: DeploymentInfo) -> TestServer {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let auth = crate::auth::AuthConfig {
        issuer_url: "https://idp.invalid/application/o/bored-test/".to_string(),
        client_id: "bored-test".to_string(),
        client_secret: "secret".to_string(),
        redirect_uri: "https://bored.invalid/auth/callback".to_string(),
        required_scope: "bored:test:access".to_string(),
        end_session_url: None,
        authorize_endpoint: "https://idp.invalid/authorize".to_string(),
        token_endpoint: "https://idp.invalid/token".to_string(),
        jwks_uri: "https://idp.invalid/jwks".to_string(),
        revocation_endpoint: None,
        mcp_issuer_url: None,
        mcp_client_id: None,
    };
    // The state owns the shared outbound client; the auth parts borrow a
    // handle to it, as `main` wires them.
    let state = AppState::new(db);
    let jwks = Arc::new(crate::auth::JwksCache::new(
        auth.jwks_uri.clone(),
        state.http.clone(),
    ));
    // Any 64 bytes make a valid cookie key; `base64::Engine` is the trait
    // whose `encode` method the STANDARD engine provides.
    let cookie_key = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [7u8; 64]);
    let sessions = Arc::new(
        crate::auth::AuthSessionManager::from_config(
            &config::SessionConfig { cookie_key },
            state.http.clone(),
        )
        .expect("a 64-byte key is valid"),
    );
    let state = state.with_auth(Arc::new(auth), jwks, sessions);
    TestServer::new(app(state, "./dist", deployment).await).unwrap()
}

#[tokio::test]
#[serial]
async fn info_reports_client_telemetry_off_without_an_endpoint() {
    let server = auth_enabled_app(DeploymentInfo::new("dev", None)).await;
    let info: shared::AppInfo = server.get("/api/info").await.json();
    assert_eq!(
        info.telemetry,
        Some(shared::ClientTelemetryConfig {
            enabled: false,
            endpoint: String::new()
        })
    );
}

#[tokio::test]
#[serial]
async fn info_reports_client_telemetry_on_with_an_endpoint_and_auth() {
    let deployment = DeploymentInfo::new("dev", None)
        .with_client_telemetry(Some("https://bored-dev.example".to_string()));
    let server = auth_enabled_app(deployment).await;
    let info: shared::AppInfo = server.get("/api/info").await.json();
    assert_eq!(
        info.telemetry,
        Some(shared::ClientTelemetryConfig {
            enabled: true,
            endpoint: "https://bored-dev.example".to_string()
        })
    );
}

#[tokio::test]
#[serial]
async fn client_telemetry_needs_auth_even_with_an_endpoint() {
    // Auth disabled: there is no session to hand a bearer out from, so the
    // SPA is told "off" and the token route has nothing to give.
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let deployment = DeploymentInfo::new("dev", None)
        .with_client_telemetry(Some("https://bored-dev.example".to_string()));
    let server = TestServer::new(app(AppState::new(db), "./dist", deployment).await).unwrap();
    let info: shared::AppInfo = server.get("/api/info").await.json();
    assert!(!info.telemetry.expect("always reported").enabled);
    server
        .get("/api/telemetry/token")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
#[serial]
async fn telemetry_token_route_is_behind_the_auth_middleware() {
    let deployment = DeploymentInfo::new("dev", None)
        .with_client_telemetry(Some("https://bored-dev.example".to_string()));
    let server = auth_enabled_app(deployment).await;
    // No cookie, no bearer: refused by the middleware before the handler.
    server
        .get("/api/telemetry/token")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

/// A panic during startup or serving is still a logged failure (card #415):
/// `guarded` turns it into `StartupError::Panicked` and writes the one
/// `startup failed` line, so `main` goes on to flush telemetry instead of
/// unwinding past it. The panic's own message stays out of that line.
#[tokio::test]
async fn a_panic_in_run_becomes_a_logged_startup_error() {
    let (outcome, logs) = capture_logs_async(async {
        crate::guarded(async {
            panic!("secret-startup-detail");
        })
        .await
    })
    .await;
    assert!(matches!(outcome, Err(crate::StartupError::Panicked)));
    assert!(logs.contains("startup failed"), "{logs}");
    assert!(!logs.contains("secret-startup-detail"), "{logs}");
}
