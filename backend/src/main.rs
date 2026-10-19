// A crate-level lint override. It lives here rather than in
// `backend/Cargo.toml` because this crate inherits `[lints] workspace = true`,
// and Cargo forbids a member that both inherits the workspace lints and adds
// its own entries. An inner attribute (`#![...]`) applies to the whole crate,
// exactly as the manifest entry did, and — unlike the manifest — a source
// attribute also outranks the `-D warnings` CI passes on the clippy command
// line.
//
// `ApiError` (and the driver errors it is built from) can exceed clippy's
// 128-byte threshold; propagating them via `?` throughout this crate is the
// intended usage, not something to box away.
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
mod http_client;
mod listen;
mod models;
mod observability;
mod redact;
mod routes;
mod server_span;
mod spa;
mod surreal_migration;
mod timestamp;

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
    /// Postgres could not be reached, or its migrations failed. Carries the
    /// error's class only (card #366: a driver message can quote values).
    Database(&'static str),
    /// `migrate-from-surreal` failed; its `Display` is already redacted.
    Migration(surreal_migration::MigrationError),
    /// `run` panicked; the panic hook has already printed where.
    Panicked,
}

impl std::fmt::Debug for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupError::Config(error) => write!(f, "{error}"),
            StartupError::Telemetry(error) => write!(f, "{error}"),
            StartupError::Database(class) => write!(f, "database unavailable ({class})"),
            StartupError::Migration(error) => write!(f, "{error}"),
            StartupError::Panicked => write!(f, "panicked (see the message above on stderr)"),
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

impl From<sqlx::Error> for StartupError {
    fn from(error: sqlx::Error) -> Self {
        StartupError::Database(error::database_error_type(&error).as_str())
    }
}

impl From<sqlx::migrate::MigrateError> for StartupError {
    fn from(error: sqlx::migrate::MigrateError) -> Self {
        match error {
            // The usual case: the database could not be reached at all.
            sqlx::migrate::MigrateError::Execute(inner) => inner.into(),
            // Anything else is a migration bookkeeping problem (a changed or
            // missing migration file), named by its variant alone.
            other => StartupError::Database(match other {
                sqlx::migrate::MigrateError::VersionMismatch(_) => "migration_changed",
                sqlx::migrate::MigrateError::VersionMissing(_) => "migration_missing",
                sqlx::migrate::MigrateError::Dirty(_) => "migration_dirty",
                _ => "migration_failed",
            }),
        }
    }
}

impl From<surreal_migration::MigrationError> for StartupError {
    fn from(error: surreal_migration::MigrationError) -> Self {
        StartupError::Migration(error)
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
        // rustls needs a crypto provider installed before any TLS handshake,
        // and both ring and aws-lc-rs are in the dependency tree, so it
        // cannot pick one itself. Installed here, before *either* path:
        // serving and `migrate-from-surreal` both build config, which opens
        // the sovereign-config TLS client (card #471 — the import panicked
        // when this lived in `run()` only).
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("failed to install rustls crypto provider");
        let telemetry = observability::init()?;
        // `bored-backend migrate-from-surreal …` is the one-off SurrealDB
        // import (card #471); anything else, including no arguments, serves.
        let args: Vec<String> = std::env::args().skip(1).collect();
        let outcome = match args.split_first() {
            Some((command, rest)) if command == "migrate-from-surreal" => {
                guarded(migrate_from_surreal(rest)).await
            }
            _ => guarded(run(&telemetry)).await.map(|()| 0),
        };
        // Always flush, whether we are here after a clean shutdown, a failed
        // startup or a panic — the telemetry of a process going down is the
        // most useful there is.
        telemetry.shutdown().await;
        outcome
    });
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    match result {
        Ok(0) => Ok(()),
        // A deliberate non-zero status (`--check-empty`'s "not empty"):
        // nothing failed, so nothing is logged, but the caller sees the code.
        Ok(code) => std::process::exit(i32::from(code)),
        Err(error) => Err(error),
    }
}

/// `migrate-from-surreal`: connect to the configured Postgres (running its
/// migrations, so the schema exists) and run the import subcommand. Returns
/// the process exit status.
async fn migrate_from_surreal(args: &[String]) -> Result<u8, StartupError> {
    let command = surreal_migration::Command::parse(args)?;
    let cfg = tokio::task::block_in_place(config::build_config)?;
    let database = config::load_group::<config::DatabaseConfig>(&cfg, "database")?;
    drop(cfg);
    let db = db::connect(&database).await?;
    Ok(surreal_migration::run(command, &db).await?)
}

/// Run `work` (the process's whole life, `run`), turning a panic into
/// [`StartupError::Panicked`] and logging any failure once — so `main` always
/// reaches the telemetry flush, and the reason reaches Loki as well as
/// `docker logs`.
///
/// `catch_unwind` turns a panic inside `run` — the database connect, OIDC
/// discovery and the TLS listener still `expect` — into a value.
/// `AssertUnwindSafe` tells the compiler nothing borrowed by `work` is used
/// after a panic in a way that could observe a half-done update; the only
/// thing used afterwards is the telemetry handle, whose providers are
/// internally synchronised. For a panic the default panic hook has already
/// written its message to stderr; it is not repeated in the log line, because
/// a panic message is built from whatever the code had in hand (#366).
async fn guarded<T>(
    work: impl std::future::Future<Output = Result<T, StartupError>>,
) -> Result<T, StartupError> {
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(work))
        .await
        .unwrap_or(Err(StartupError::Panicked));
    if let Err(error) = &outcome {
        tracing::error!(error = ?error, "startup failed");
    }
    outcome
}

/// Load config, connect the database, build the app, serve until SIGTERM or
/// Ctrl-C, then drain.
async fn run(telemetry: &observability::Telemetry) -> Result<(), StartupError> {
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
    let database = config::load_group::<config::DatabaseConfig>(&cfg, "database")?;
    // Optional: absent means the SPA is told telemetry is off (card #416).
    let client_telemetry = config::load_client_telemetry(&cfg)?;
    drop(cfg);

    // Now that config is known: narrow the log level (it filters log lines,
    // never spans), and refuse to start if the configured environment and the
    // telemetry resource name different deployments.
    telemetry.set_log_level(&observability.log_level);
    telemetry.check_environment(&observability.environment)?;

    // Connects and brings the schema up to date; an unreachable database or a
    // failed migration stops startup here, before the listener binds.
    let db = db::connect(&database).await?;

    // `oidc` is `None` when `oidc.issuer-url` is unset — auth-disabled mode,
    // useful for local hacking without a live IdP and for unit tests.
    let state = if let Some(oidc) = oidc {
        with_oidc(
            AppState::new(db),
            &oidc,
            &session.expect("session config is loaded whenever oidc is enabled"),
        )
        .await
    } else {
        tracing::warn!("oidc.issuer-url not set — auth middleware will inject anonymous claim");
        AppState::new(db)
    };

    // The state's drain signal, kept before `state` moves into the router:
    // `listen::serve` raises it on shutdown, and the SSE handler (holding a
    // clone through the state) ends its streams on it.
    let draining = state.draining.clone();
    let app = app(
        state,
        &server.static_dir,
        DeploymentInfo::new(&observability.environment, observability.branch.clone())
            .with_client_telemetry(client_telemetry.endpoint),
    )
    .await;

    // Serve until SIGTERM (`docker stop`) or Ctrl-C, then drain open
    // connections for a bounded time. See `listen.rs`.
    listen::serve(&server, app, draining, listen::shutdown_signal()).await;
    Ok(())
}

/// Enable OIDC auth on `state`: resolve the provider's endpoints, then build
/// the JWKS cache and the browser-session manager.
///
/// Every component is handed `state.http` — the backend's one outbound HTTP
/// client (card #120) — rather than building its own, so discovery, the JWKS
/// fetch, token exchange, refresh and revocation all share one connection
/// pool. `state.http.clone()` is a cheap handle onto that same client (and
/// pool), not a copy of it. Split out of `main` so a test can run this exact
/// wiring against a fake identity provider.
async fn with_oidc(
    state: AppState,
    oidc: &config::OidcConfig,
    session: &config::SessionConfig,
) -> AppState {
    // `&state.http` lends the client for the duration of discovery.
    let auth = AuthConfig::from_config(oidc, &state.http).await;
    tracing::info!(
        issuer = %auth.issuer_url,
        client_id = %auth.client_id,
        required_scope = %auth.required_scope,
        jwks_uri = %auth.jwks_uri,
        authorize_endpoint = %auth.authorize_endpoint,
        token_endpoint = %auth.token_endpoint,
        "OIDC auth enabled"
    );
    let cache = Arc::new(JwksCache::new(auth.jwks_uri.clone(), state.http.clone()));
    let sessions = Arc::new(
        AuthSessionManager::from_config(session, state.http.clone())
            // `SessionConfig::validate` already rejected a malformed key at the
            // config-loading stage — this can only fail if that invariant is
            // broken.
            .expect("session.cookie-key already validated by config::SessionConfig::validate"),
    );
    state.with_auth(Arc::new(auth), cache, sessions)
}

// ── Integration tests ─────────────────────────────────────────────────────────
// `#[cfg(test)]` means this entire module is only compiled when running tests.
// Each test spins up a real Axum router over a fresh Postgres database of its
// own (`db::connect_test`) — no mocking, no fixtures, every test starts clean.
#[cfg(test)]
mod tests;
