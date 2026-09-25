//! The axum router: every route, the auth layering and the public endpoints.

use axum::{
    Router,
    middleware,
    routing::{delete, get, post, put}, // HTTP method helpers for the router
};
use tower_http::trace::{DefaultMakeSpan, TraceLayer}; // Middleware: request tracing

use crate::auth::auth_middleware;
use crate::routes::boards::AppState;
use crate::spa::SpaSvc;
use crate::{config, events, routes};

/// What `/api/info` reports about the running deployment.
///
/// A struct rather than two `&str` parameters on [`app`]: `environment` and
/// `branch` are both short lowercase strings, so a swapped pair would compile
/// and only show up as a wrong Grafana label.
#[derive(Debug, Clone)]
pub struct DeploymentInfo {
    /// `dev` | `prod` (`test` under e2e) — see `config::ObservabilityConfig`.
    pub environment: String,
    /// Branch a dev deployment was built from; `None` in prod and locally.
    pub branch: Option<String>,
}

impl DeploymentInfo {
    pub fn new(environment: impl Into<String>, branch: Option<String>) -> Self {
        Self {
            environment: environment.into(),
            branch,
        }
    }
}

// `app` is extracted from `main` so integration tests can call it directly
// without spinning up a real TCP listener. Tests construct `AppState` with an
// in-memory DB, call `app(state, static_dir, deployment).await`, and pass the
// router to `TestServer`.
//
// Routing layout:
//   Public (no auth required):
//     /health, /api/info, /auth/login, /auth/callback, /auth/logout
//   Protected (auth middleware enforced — `auth` cookie or `Bearer` header):
//     /api/me, /api/boards/*, /api/columns/*, /api/cards/*, /api/events
//
// When the server is started without `oidc.issuer-url` configured (i.e. local
// dev or tests), the middleware short-circuits and injects a synthetic
// `anonymous` claim so existing flows keep working unchanged.
pub async fn app(state: AppState, static_dir: &str, deployment: DeploymentInfo) -> Router {
    // Build the protected `/api/*` sub-router. Every route here gets the auth
    // middleware applied below; handlers can extract `Extension<Claims>` to
    // get the validated identity. The middleware needs access to AppState
    // (for the JWKS cache + auth config), so we pass state via `from_fn_with_state`.
    let protected_api = Router::new()
        // SSE stream — clients subscribe here to receive real-time board events.
        .route("/events", get(events::sse_handler))
        // Identity endpoint for the SPA navbar.
        .route("/me", get(routes::auth::me))
        .route("/boards", get(routes::boards::list_boards))
        .route("/boards", post(routes::boards::create_board))
        .route("/boards/:slug", get(routes::boards::get_board))
        .route("/boards/:slug", put(routes::boards::update_board))
        .route("/boards/:slug", delete(routes::boards::delete_board))
        .route("/boards/:slug/history", get(routes::audit::board_history))
        .route("/boards/:slug/links", get(routes::links::list_board_links))
        .route("/boards/:slug/columns", get(routes::columns::list_columns))
        .route(
            "/boards/:slug/columns",
            post(routes::columns::create_column),
        )
        // Bulk reorder: PUT replaces the entire column order in one round-trip.
        .route(
            "/boards/:slug/columns/reorder",
            put(routes::columns::reorder_columns),
        )
        .route("/columns/:id/history", get(routes::audit::column_history))
        .route("/columns/:id", put(routes::columns::update_column))
        .route("/columns/:id", delete(routes::columns::delete_column))
        .route("/columns/:id/cards", get(routes::cards::list_cards))
        .route("/columns/:id/cards", post(routes::cards::create_card))
        // Bulk reorder: PUT replaces the whole card order within one column.
        .route(
            "/columns/:id/cards/reorder",
            put(routes::cards::reorder_cards),
        )
        // Static segment "by-number" must come before `:id` so it takes priority.
        .route(
            "/cards/by-number/:number",
            get(routes::cards::get_card_by_number),
        )
        .route("/cards/:id/history", get(routes::audit::card_history))
        .route("/cards/:id", get(routes::cards::get_card))
        .route("/cards/:id", put(routes::cards::update_card))
        .route("/cards/:id", delete(routes::cards::delete_card))
        .route("/cards/:id/move", post(routes::cards::move_card))
        // Card links: created from either card, addressed by link id afterwards.
        .route("/cards/:id/links", post(routes::links::create_card_link))
        .route("/links/:id", put(routes::links::update_card_link))
        .route("/links/:id", delete(routes::links::delete_card_link))
        .route("/audit/:id/restore", post(routes::audit::restore_audit))
        // Apply the auth middleware to every route in this sub-router. The
        // middleware reads from request extensions injected by `with_state`,
        // so the layer must come AFTER the routes are mounted but BEFORE
        // `.with_state()` is called (axum resolves layers in reverse order
        // and `with_state` finalises the router).
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());

    // Public auth flow routes — no middleware, no auth required to use them.
    let auth_routes = Router::new()
        .route("/login", get(routes::auth::login))
        .route("/callback", get(routes::auth::callback))
        .route("/logout", get(routes::auth::logout))
        .with_state(state);

    // `deployment` is captured into the `/api/info` closure below rather than
    // read per-request, since it comes from `ObservabilityConfig` (set once at
    // startup) rather than a live `std::env::var` lookup.

    Router::new()
        .route("/health", get(health))
        // `/api/info` is intentionally public — the frontend fetches it
        // unauthenticated on every page load to populate the version watermark.
        // It must stay outside any auth-gated sub-router.
        .route("/api/info", get(move || info(deployment.clone())))
        // Browser-facing OAuth2 flow endpoints.
        .nest("/auth", auth_routes)
        // Protected API — every route under here requires a valid token (or
        // runs in synthetic-anonymous mode if OIDC env vars are unset).
        .nest("/api", protected_api)
        // `SpaSvc` serves static files from the dist directory and falls back to
        // index.html for any path that isn't a real file on disk, enabling
        // Leptos client-side routing to handle deep-links (e.g. /boards/123).
        .fallback_service(SpaSvc::new(static_dir))
        // `TraceLayer` logs every request (method, path, status, latency) using
        // the `tracing` crate — visible as structured JSON in production.
        //
        // The span is made at INFO rather than tower-http's default DEBUG.
        // Deployments filter at `info`, so at DEBUG the span is never created
        // and events raised inside the request — notably the `ERROR request
        // failed` line in `error.rs` — carry no method or path. Opening a span
        // emits no log line of its own; this only makes the request's fields
        // available to the events that do.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO)),
        )
}

pub(crate) async fn health() -> &'static str {
    "ok"
}

// Returns runtime version, environment and (on dev) the branch.
// Version: the release tag burned into the image at build time (see
// `shared::app_version`). `APP_VERSION` remains an optional runtime override
// (used by tests and ad-hoc runs); when unset the burned-in tag is reported.
// `deployment` is captured at startup from `ObservabilityConfig` (see `app`).
async fn info(deployment: DeploymentInfo) -> axum::Json<shared::AppInfo> {
    axum::Json(shared::AppInfo {
        version: config::app_version_override()
            .unwrap_or_else(|| shared::app_version().to_string()),
        env: deployment.environment,
        branch: deployment.branch,
    })
}
