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
// `main.rs` itself is startup only: pick the command (serve, or the deploy's
// one-shot `migrate`), load config, connect the database, build the shared
// state and bind the listener. The router lives in `app.rs` and the
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
mod migrate;
mod models;
mod observability;
mod redact;
mod routes;
mod server_span;
mod spa;
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
    /// Postgres could not be reached, or its schema is not this release's
    /// (the server's check), or applying it failed (`migrate`). Carries the
    /// error's class only (card #366: a driver message can quote values).
    Schema(migrate::SchemaError),
    /// The command line named something other than `migrate`.
    Usage,
    /// `run` panicked; the panic hook has already printed where.
    Panicked,
}

impl std::fmt::Debug for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupError::Config(error) => write!(f, "{error}"),
            StartupError::Telemetry(error) => write!(f, "{error}"),
            StartupError::Schema(error) => write!(f, "{error}"),
            // The argument itself is not echoed: nothing about it is needed
            // to fix the call, and the rule is that input stays out of logs.
            StartupError::Usage => write!(
                f,
                "unknown command: run `bored-backend` to serve, or `bored-backend migrate`"
            ),
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

impl From<migrate::SchemaError> for StartupError {
    fn from(error: migrate::SchemaError) -> Self {
        StartupError::Schema(error)
    }
}

/// What the process was started to do: `bored-backend` serves,
/// `bored-backend migrate` runs the deploy's one-shot migrate step and exits
/// (card #640).
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve,
    Migrate,
}

impl Command {
    /// Parse the arguments after the program name. Anything but nothing or
    /// exactly `migrate` is refused rather than ignored, so a typo in a
    /// compose `command:` cannot quietly start a second server.
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, StartupError> {
        match (args.next().as_deref(), args.next()) {
            (None, _) => Ok(Command::Serve),
            (Some("migrate"), None) => Ok(Command::Migrate),
            _ => Err(StartupError::Usage),
        }
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
        // cannot pick one itself. Installed here, before anything builds
        // config: that opens the sovereign-config TLS client, which would
        // otherwise panic on its first handshake. (It lives in `main` rather
        // than `run()` since card #471, when a second entry point — the
        // since-removed SurrealDB import, card #489 — needed it too.)
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("failed to install rustls crypto provider");
        let telemetry = observability::init()?;
        // `migrate` is the deploy's one-shot step (card #640); no argument
        // serves. (Until card #489 a `migrate-from-surreal` argument ran the
        // one-off SurrealDB import; it is gone, and now refused as unknown.)
        let outcome = match Command::parse(std::env::args().skip(1)) {
            Ok(Command::Serve) => guarded(run(&telemetry)).await,
            Ok(Command::Migrate) => guarded(run_migrate(&telemetry)).await,
            Err(error) => guarded(std::future::ready(Err(error))).await,
        };
        // Always flush, whether we are here after a clean shutdown, a failed
        // startup or a panic — the telemetry of a process going down is the
        // most useful there is.
        telemetry.shutdown().await;
        outcome
    });
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    // `Err` makes `main` return it, and the `Termination` impl prints it via
    // `Debug` and exits non-zero.
    result
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

/// Build the layered config, and apply the two settings every command needs
/// before it does anything else: the log level, and the check that the
/// configured environment and the telemetry resource agree.
///
/// Returns the whole config for the caller to take its own groups from, and
/// the `database` group, which both commands use.
fn load_config(
    telemetry: &observability::Telemetry,
) -> Result<
    (
        ::config::Config,
        config::ObservabilityConfig,
        config::DatabaseConfig,
    ),
    StartupError,
> {
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
    let database = config::load_group::<config::DatabaseConfig>(&cfg, "database")?;

    // Now that config is known: narrow the log level (it filters log lines,
    // never spans), and refuse to start if the configured environment and the
    // telemetry resource name different deployments.
    telemetry.set_log_level(&observability.log_level);
    telemetry.check_environment(&observability.environment)?;
    Ok((cfg, observability, database))
}

/// `bored-backend migrate`: load config, run every migrate step, exit. The
/// deploy's `migrate` service (card #640); see `migrate.rs`.
///
/// It reads the same config the server does — the database login comes from
/// sovereign-config through the access-URL secret *file* — so the two can
/// never be pointed at different databases.
async fn run_migrate(telemetry: &observability::Telemetry) -> Result<(), StartupError> {
    let (cfg, _observability, database) = load_config(telemetry)?;
    drop(cfg);
    migrate::run(&database).await?;
    Ok(())
}

/// Load config, connect the database, build the app, serve until SIGTERM or
/// Ctrl-C, then drain.
async fn run(telemetry: &observability::Telemetry) -> Result<(), StartupError> {
    let (cfg, observability, database) = load_config(telemetry)?;
    let oidc = config::load_optional_oidc(&cfg)?; // None => auth-disabled mode
    let session = oidc
        .is_some()
        .then(|| config::load_group::<config::SessionConfig>(&cfg, "session"))
        .transpose()?;
    let server = config::load_group::<config::ServerConfig>(&cfg, "server")?;
    // Optional: absent means the SPA is told telemetry is off (card #416).
    let client_telemetry = config::load_client_telemetry(&cfg)?;
    drop(cfg);

    // Connects and checks the schema is this release's — the deploy's
    // `migrate` step applied it; the server never does (card #640). An
    // unreachable database, or one `migrate` has not run on, stops startup
    // here, before the listener binds.
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

/// The command line (card #640): nothing serves, exactly `migrate` migrates,
/// and anything else is refused rather than silently served — a typo in a
/// compose `command:` must not start a second server.
#[cfg(test)]
mod command_tests {
    use super::{Command, StartupError};

    fn parse(args: &[&str]) -> Result<Command, StartupError> {
        Command::parse(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn no_argument_serves() {
        assert_eq!(parse(&[]).ok(), Some(Command::Serve));
    }

    #[test]
    fn migrate_migrates() {
        assert_eq!(parse(&["migrate"]).ok(), Some(Command::Migrate));
    }

    #[test]
    fn anything_else_is_refused() {
        for args in [
            &["migrat"][..],
            &["Migrate"],
            &["migrate", "extra"],
            &["serve"],
            &[""],
            // The retired one-off import (card #489) is unknown, not served.
            &["migrate-from-surreal"],
        ] {
            assert!(
                matches!(parse(args), Err(StartupError::Usage)),
                "{args:?} must be refused"
            );
        }
    }

    /// The refusal names both valid forms and does not echo the argument.
    #[test]
    fn the_usage_error_names_the_commands_only() {
        let message = format!(
            "{:?}",
            parse(&["secret-looking-input"]).expect_err("refused")
        );
        assert!(message.contains("bored-backend migrate"), "{message}");
        assert!(!message.contains("secret-looking-input"), "{message}");
    }
}
