// A crate-level lint override. It lives here rather than in
// `backend/Cargo.toml` because this crate inherits `[lints] workspace = true`,
// and Cargo forbids a member that both inherits the workspace lints and adds
// its own entries. An inner attribute (`#![...]`) applies to the whole crate,
// exactly as the manifest entry did, and — unlike the manifest — a source
// attribute also outranks the `-D warnings` CI passes on the clippy command
// line.
//
// surrealdb::Error is inherently >128 bytes; propagating it via `?` throughout
// this crate is the intended usage of the driver, not something to box away.
#![allow(clippy::result_large_err)]

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
mod error;
mod events;
mod listen;
mod models;
mod observability;
mod routes;
mod server_span;
mod spa;

use std::sync::Arc;
use std::time::Duration;

use routes::boards::AppState;

use crate::app::{DeploymentInfo, app};
use crate::auth::{AuthConfig, AuthSessionManager, JwksCache};

/// How long the Tokio runtime may take to wind down after `run` returns,
/// before its remaining tasks are abandoned. The connection drain
/// (`listen::DRAIN_TIMEOUT`) and the telemetry flush
/// (`observability::FLUSH_TIMEOUT`) have already had their time by then; this
/// only stops a stuck blocking task from holding the process past Docker's
/// stop timeout.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Why the process failed to start. Printed by `main`'s `Termination` impl,
/// which uses `Debug` — so `Debug` delegates to `Display` for a one-line,
/// redacted reason rather than a struct dump.
enum StartupError {
    Config(config::ConfigError),
    Telemetry(observability::TelemetryError),
}

impl std::fmt::Debug for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupError::Config(error) => write!(f, "{error}"),
            StartupError::Telemetry(error) => write!(f, "{error}"),
        }
    }
}

// These two `From` impls are what let `?` turn either error into a
// `StartupError` inside `run`.
impl From<config::ConfigError> for StartupError {
    fn from(error: config::ConfigError) -> Self {
        StartupError::Config(error)
    }
}

impl From<observability::TelemetryError> for StartupError {
    fn from(error: observability::TelemetryError) -> Self {
        StartupError::Telemetry(error)
    }
}

/// The entry point builds the Tokio runtime by hand rather than with
/// `#[tokio::main]`, for one reason: `#[tokio::main]` drops the runtime when
/// `main` returns, and dropping a runtime waits *indefinitely* for its
/// blocking tasks. `shutdown_timeout` bounds that wait, so a telemetry export
/// stuck on an unresponsive collector can never keep the container alive until
/// Docker kills it.
fn main() -> Result<(), StartupError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the Tokio runtime");
    // `block_on` runs the async startup-serve-shutdown sequence to completion
    // on this thread.
    let result = runtime.block_on(async {
        // Telemetry comes up first, from the `OTEL_*` variables alone, so the
        // sovereign-config call below has a span and every later line is
        // correlated. An invalid variable set stops the process here.
        let telemetry = observability::init()?;
        let outcome = run(&telemetry).await;
        if let Err(error) = &outcome {
            // Said once on stdout (and exported) before the flush below, so the
            // reason reaches Loki as well as `docker logs`.
            tracing::error!(error = ?error, "startup failed");
        }
        // Always flush, whether we are here after a clean shutdown or a failed
        // startup — the telemetry of a process going down is the most useful
        // there is.
        telemetry.shutdown().await;
        outcome
    });
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

/// Load config, connect the database, build the app, serve until SIGTERM or
/// Ctrl-C, then drain.
async fn run(telemetry: &observability::Telemetry) -> Result<(), StartupError> {
    // rustls needs a crypto provider installed before any TLS handshakes.
    // `ring` is the default provider — this call must happen before any
    // TLS config is created.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // Composition root: build the layered config once, before any other task
    // is scheduled, then hand each validated DTO to the feature that owns it.
    // Every downstream component receives its typed config, never raw env or
    // provider access.
    //
    // `build_config` blocks on a startup sovereign-config RPC (on the
    // provider's own I/O thread). `block_in_place` tells Tokio this worker is
    // about to block, so other tasks — the telemetry exporters among them —
    // move to another worker meanwhile. The span is the one child span this
    // process opens for the call that leaves it; with no access URL set there
    // is no call, and no span.
    let cfg = tokio::task::block_in_place(|| {
        if config::sovereign_source_enabled() {
            tracing::info_span!("sovereign-config load", otel.kind = "client")
                .in_scope(config::build_config)
        } else {
            config::build_config()
        }
    })?;
    let observability = config::load_group::<config::ObservabilityConfig>(&cfg, "observability")?;
    let oidc = config::load_optional_oidc(&cfg)?; // None => auth-disabled mode
    let session = oidc
        .is_some()
        .then(|| config::load_group::<config::SessionConfig>(&cfg, "session"))
        .transpose()?;
    let server = config::load_group::<config::ServerConfig>(&cfg, "server")?;
    drop(cfg);

    // Now that config is known: narrow the log level (it filters log lines,
    // never spans), and refuse to start if the configured environment and the
    // telemetry resource name different deployments.
    telemetry.set_log_level(&observability.log_level);
    telemetry.check_environment(&observability.environment)?;

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

    let app = app(
        state,
        &server.static_dir,
        DeploymentInfo::new(&observability.environment, observability.branch.clone()),
    )
    .await;

    // Serve until SIGTERM (`docker stop`) or Ctrl-C, then drain open
    // connections for a bounded time. See `listen.rs`.
    listen::serve(&server, app, listen::shutdown_signal()).await;
    Ok(())
}

// ── Integration tests ─────────────────────────────────────────────────────────
// `#[cfg(test)]` means this entire module is only compiled when running tests.
// Each test spins up a real Axum router with an in-memory SurrealDB — no mocking,
// no fixtures, every test starts clean.
#[cfg(test)]
mod tests;
