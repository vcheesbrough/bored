// Declare submodules — Rust looks for each in a file named `src/<name>.rs`.
// These are private by default; the route handlers are reached via `routes::boards::...`.
//
// `main.rs` itself is startup only: load config, connect the database, build
// the shared state and bind the listener. The router lives in `app.rs` and the
// static-file fallback in `spa.rs`.
mod app;
mod audit;
mod auth;
mod config;
mod db;
mod events;
mod models;
mod observability;
mod routes;
mod spa;

use std::net::SocketAddr;
use std::sync::Arc;

use axum_server::tls_rustls::RustlsConfig; // TLS support using rustls (pure-Rust TLS)
use routes::boards::AppState;

use crate::app::app;
use crate::auth::{AuthConfig, AuthSessionManager, JwksCache};

// `#[tokio::main]` is a macro that sets up the Tokio async runtime and runs
// this function as the entry point. Without it, `async fn main` wouldn't work
// because Rust's standard runtime is synchronous.
#[tokio::main]
async fn main() -> Result<(), config::ConfigError> {
    // rustls needs a crypto provider installed before any TLS handshakes.
    // `ring` is the default provider — this call must happen before any
    // TLS config is created.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // Composition root: build the layered config once, before any other task
    // is scheduled (`build_config` blocks on a startup sovereign-config RPC,
    // which is safe here because nothing else has run yet), then hand each
    // validated DTO to the feature that owns it. Every downstream component
    // receives its typed config, never raw env or provider access.
    let cfg = config::build_config()?;
    let observability = config::load_group::<config::ObservabilityConfig>(&cfg, "observability")?;
    let oidc = config::load_optional_oidc(&cfg)?; // None => auth-disabled mode
    let session = oidc
        .is_some()
        .then(|| config::load_group::<config::SessionConfig>(&cfg, "session"))
        .transpose()?;
    let server = config::load_group::<config::ServerConfig>(&cfg, "server")?;
    drop(cfg);

    // Initialise structured logging / tracing (returns a guard that flushes on drop).
    let _obs = observability::init(&observability);

    let db = db::connect_persistent(&server.database_path)
        .await
        .expect("failed to connect to database");

    // `oidc` is `None` when `oidc.issuer-url` is unset — auth-disabled mode,
    // useful for local hacking without a live IdP and for unit tests.
    let state = if let Some(oidc) = oidc {
        let auth = AuthConfig::from_config(&oidc).await;
        tracing::info!(
            issuer = %auth.issuer_url,
            client_id = %auth.client_id,
            required_scope = %auth.required_scope,
            jwks_uri = %auth.jwks_uri,
            authorize_endpoint = %auth.authorize_endpoint,
            token_endpoint = %auth.token_endpoint,
            "OIDC auth enabled"
        );
        let cache = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
        let sessions = Arc::new(
            AuthSessionManager::from_config(
                &session.expect("session config is loaded whenever oidc is enabled"),
            )
            // `SessionConfig::validate` already rejected a malformed key at the
            // config-loading stage above — this can only fail if that invariant
            // is broken.
            .expect("session.cookie-key already validated by config::SessionConfig::validate"),
        );
        AppState::new(db).with_auth(Arc::new(auth), cache, sessions)
    } else {
        tracing::warn!("oidc.issuer-url not set — auth middleware will inject anonymous claim");
        AppState::new(db)
    };

    let app = app(state, &server.static_dir, &observability.environment).await;

    // TLS pair present ⇒ serve HTTPS on :443. Otherwise plain HTTP on
    // `server.http-port` (dev mode).
    match server.tls_pair() {
        Some((cert, key)) => {
            let tls_config = RustlsConfig::from_pem_file(cert, key)
                .await
                .expect("failed to load TLS config");
            // `[0, 0, 0, 0]` means bind to all network interfaces (0.0.0.0).
            let addr = SocketAddr::from(([0, 0, 0, 0], 443));
            tracing::info!(%addr, "bored backend listening (TLS)");
            axum_server::bind_rustls(addr, tls_config)
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
        None => {
            let addr = SocketAddr::from(([0, 0, 0, 0], server.http_port));
            tracing::info!(%addr, "bored backend listening (plain HTTP)");
            // `tokio::net::TcpListener` is the async equivalent of the standard
            // library's `TcpListener` — it doesn't block the thread while waiting.
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        }
    }
    Ok(())
}

// ── Integration tests ─────────────────────────────────────────────────────────
// `#[cfg(test)]` means this entire module is only compiled when running tests.
// Each test spins up a real Axum router with an in-memory SurrealDB — no mocking,
// no fixtures, every test starts clean.
#[cfg(test)]
mod tests;
