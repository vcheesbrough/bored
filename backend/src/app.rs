//! The axum router: every route, the auth layering and the public endpoints.

use axum::{
    Router,
    middleware,
    routing::{delete, get, post, put}, // HTTP method helpers for the router
};
// Middleware: one server span per request, and the levels of tower-http's own
// per-request events.
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::trace::{DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer};

use crate::auth::auth_middleware;
use crate::routes::boards::AppState;
use crate::spa::SpaSvc;
use crate::{config, events, routes, server_span};

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
    /// Where the SPA sends its own telemetry (`client-telemetry.endpoint`,
    /// card #416); `None` when this deployment offers no ingest.
    pub client_telemetry_endpoint: Option<String>,
}

impl DeploymentInfo {
    pub fn new(environment: impl Into<String>, branch: Option<String>) -> Self {
        Self {
            environment: environment.into(),
            branch,
            client_telemetry_endpoint: None,
        }
    }

    /// Builder-style: the same deployment, with a client-telemetry endpoint.
    /// `mut self` takes ownership and hands it back modified, so calls chain.
    pub fn with_client_telemetry(mut self, endpoint: Option<String>) -> Self {
        self.client_telemetry_endpoint = endpoint;
        self
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
    // Client telemetry is on only with both an ingest endpoint and browser
    // auth: the ingest accepts nothing but a session's bearer, and an
    // auth-disabled server has no session to hand one out from (card #416).
    let client_telemetry = deployment.client_telemetry_endpoint.is_some() && state.auth.is_some();

    // Build the protected `/api/*` sub-router. Every route here gets the auth
    // middleware applied below; handlers can extract `Extension<Claims>` to
    // get the validated identity. The middleware needs access to AppState
    // (for the JWKS cache + auth config), so we pass state via `from_fn_with_state`.
    let protected_api = Router::new()
        // SSE stream — clients subscribe here to receive real-time board events.
        .route("/events", get(events::sse_handler))
        // Identity endpoint for the SPA navbar.
        .route("/me", get(routes::auth::me))
        // The bearer the SPA presents to the telemetry ingest (card #416).
        // A closure over `client_telemetry`, fixed at startup; the extension
        // is present only for a cookie session.
        .route(
            "/telemetry/token",
            get(
                move |session: Option<axum::Extension<crate::auth::SessionAccessToken>>| {
                    routes::telemetry::token(client_telemetry, session)
                },
            ),
        )
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

    let router = Router::new()
        .route("/health", get(health))
        // `/api/info` is intentionally public — the frontend fetches it
        // unauthenticated on every page load to populate the version watermark.
        // It must stay outside any auth-gated sub-router.
        .route(
            "/api/info",
            get(move || info(deployment.clone(), client_telemetry)),
        )
        // Browser-facing OAuth2 flow endpoints.
        .nest("/auth", auth_routes)
        // Protected API — every route under here requires a valid token (or
        // runs in synthetic-anonymous mode if OIDC env vars are unset).
        .nest("/api", protected_api)
        // `SpaSvc` serves static files from the dist directory and falls back to
        // index.html for any path that isn't a real file on disk, enabling
        // Leptos client-side routing to handle deep-links (e.g. /boards/123).
        .fallback_service(SpaSvc::new(static_dir));
    with_request_telemetry(router)
}

/// Wrap `router` in the per-request telemetry layers, outermost last:
///
/// 1. `CatchPanicLayer` turns a handler panic into a 500 *inside* the layers
///    below, so a panicking request is still recorded — without it the
///    connection task unwinds straight past `record_response`, and the
///    request leaves no metric point and no error on its span.
/// 2. `record_response` records the status on the request span and the
///    `http.server.request.duration` metric. It sits inside `TraceLayer`, so
///    it runs within the span.
/// 3. `TraceLayer` opens one `server` span per request, parented on the
///    caller's `traceparent` (see `server_span.rs`). `Router::layer` applies
///    after routing, which is what makes the matched route template
///    available to the span as `http.route`.
///
/// tower-http's own per-request events — "started processing request",
/// "finished processing request", "response failed" — are pushed down to
/// TRACE, below anything a deployment runs at (both run `debug`). They would
/// be a "request completed" log line per request, and that fact already has
/// two homes: the server span records the request and the histogram counts it
/// (one fact, one signal — skill §3). An internal error is still logged once,
/// with its cause, by `ApiError::into_response`, and a panic by
/// `server_span::panic_response`.
///
/// Split out of [`app`] so a test can put the same layers around a router of
/// its own (a route that panics, say).
pub(crate) fn with_request_telemetry(router: Router) -> Router {
    router
        .layer(CatchPanicLayer::custom(server_span::panic_response))
        .layer(middleware::from_fn(server_span::record_response))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(server_span::ServerSpan)
                .on_request(DefaultOnRequest::new().level(tracing::Level::TRACE))
                .on_response(DefaultOnResponse::new().level(tracing::Level::TRACE))
                .on_failure(DefaultOnFailure::new().level(tracing::Level::TRACE)),
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
//
// It also carries the SPA's telemetry configuration (card #416): the browser
// has no compiled-in endpoint, so this answer is the only way it learns
// whether and where to export. Off is said explicitly (`enabled: false`) rather
// than by omission, so "this deployment has switched it off" is visible.
async fn info(deployment: DeploymentInfo, client_telemetry: bool) -> axum::Json<shared::AppInfo> {
    let telemetry = match (client_telemetry, deployment.client_telemetry_endpoint) {
        (true, Some(endpoint)) => shared::ClientTelemetryConfig {
            enabled: true,
            endpoint,
        },
        _ => shared::ClientTelemetryConfig {
            enabled: false,
            endpoint: String::new(),
        },
    };
    axum::Json(shared::AppInfo {
        version: config::app_version_override()
            .unwrap_or_else(|| shared::app_version().to_string()),
        env: deployment.environment,
        branch: deployment.branch,
        telemetry: Some(telemetry),
    })
}
