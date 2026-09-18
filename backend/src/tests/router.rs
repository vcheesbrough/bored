//! Router-level behaviour: health, `/api/info`, the SPA fallback and API 404s.

use super::*;
use crate::app::health;

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
    assert!(!info.version.is_empty());
    // `test_app()` passes "dev" as the environment.
    assert_eq!(info.env, "dev");
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
    // `environment` is threaded through `app()` directly (from
    // `ObservabilityConfig` in production) rather than a raw env var.
    let router = app(state, "./dist", "production").await;
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
    assert_eq!(info.env, "production");
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
    let router = app(state, dir.path().to_str().unwrap(), "dev").await;
    let server = TestServer::new(router).unwrap();
    let resp = server.get("/boards/some-deep-link").await;
    resp.assert_status(StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "text/html; charset=utf-8");
    assert!(resp.text().contains("<html>"));
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
