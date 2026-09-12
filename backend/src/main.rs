// Declare submodules — Rust looks for each in a file named `src/<name>.rs`.
// These are private by default; the route handlers are reached via `routes::boards::...`.
mod audit;
mod auth;
mod config;
mod db;
mod events;
mod models;
mod observability;
mod routes;

use std::sync::Arc;

use axum::{
    Router,
    middleware,
    routing::{delete, get, post, put}, // HTTP method helpers for the router
};
use axum_server::tls_rustls::RustlsConfig; // TLS support using rustls (pure-Rust TLS)
use routes::boards::AppState;
use std::net::SocketAddr;
use tower_http::{services::ServeDir, trace::TraceLayer}; // Middleware: static files + request tracing

use crate::auth::{AuthConfig, AuthSessionManager, JwksCache, auth_middleware};

// Wraps ServeDir and replaces any 404 response with index.html so that SPA
// deep-links (e.g. /boards/123) survive a browser reload.
// tower-http 0.6's ServeDir::not_found_service does not fire for paths that
// don't exist on disk, so we intercept the 404 response after the fact.
#[derive(Clone)]
struct SpaSvc {
    inner: ServeDir,
    index_path: std::path::PathBuf,
}

impl SpaSvc {
    fn new(static_dir: &str) -> Self {
        Self {
            inner: ServeDir::new(static_dir),
            index_path: std::path::Path::new(static_dir).join("index.html"),
        }
    }
}

impl tower::Service<axum::http::Request<axum::body::Body>> for SpaSvc {
    type Response = axum::http::Response<axum::body::Body>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        // ServeDir is always ready; delegating here would reserve readiness on
        // self.inner, but call() clones it — so the reservation would be discarded.
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        use axum::http::StatusCode;
        use tower::ServiceExt;
        let inner = self.inner.clone();
        let index_path = self.index_path.clone();
        Box::pin(async move {
            // ServeDir is infallible in tower-http 0.6
            let resp = inner.oneshot(req).await.unwrap();
            if resp.status() == StatusCode::NOT_FOUND {
                match tokio::fs::read(&index_path).await {
                    Ok(bytes) => Ok(axum::http::Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html; charset=utf-8")
                        .body(axum::body::Body::from(bytes))
                        .expect("static index.html response is always valid")),
                    // index.html itself is missing — pass through the 404
                    Err(_) => {
                        let (parts, body) = resp.into_parts();
                        Ok(axum::http::Response::from_parts(
                            parts,
                            axum::body::Body::new(body),
                        ))
                    }
                }
            } else {
                let (parts, body) = resp.into_parts();
                Ok(axum::http::Response::from_parts(
                    parts,
                    axum::body::Body::new(body),
                ))
            }
        })
    }
}

// `app` is extracted from `main` so integration tests can call it directly
// without spinning up a real TCP listener. Tests construct `AppState` with an
// in-memory DB, call `app(state, static_dir, environment).await`, and pass the
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
pub async fn app(state: AppState, static_dir: &str, environment: &str) -> Router {
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

    // `environment` is captured into the `/api/info` closure below rather than
    // read per-request, since it now comes from `ObservabilityConfig` (set once
    // at startup) rather than a live `std::env::var` lookup.
    let environment = environment.to_string();

    Router::new()
        .route("/health", get(health))
        // `/api/info` is intentionally public — the frontend fetches it
        // unauthenticated on every page load to populate the version watermark.
        // It must stay outside any auth-gated sub-router.
        .route("/api/info", get(move || info(environment.clone())))
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
        .layer(TraceLayer::new_for_http())
}

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

async fn health() -> &'static str {
    "ok"
}

// Returns runtime version and environment.
// Version: the release tag burned into the image at build time (see
// `shared::app_version`). `APP_VERSION` remains an optional runtime override
// (used by tests and ad-hoc runs); when unset the burned-in tag is reported.
// `environment` is captured at startup from `ObservabilityConfig` (see `app`).
async fn info(environment: String) -> axum::Json<shared::AppInfo> {
    axum::Json(shared::AppInfo {
        version: config::app_version_override()
            .unwrap_or_else(|| shared::app_version().to_string()),
        env: environment,
    })
}

// ── Integration tests ─────────────────────────────────────────────────────────
// `#[cfg(test)]` means this entire module is only compiled when running tests.
// Each test spins up a real Axum router with an in-memory SurrealDB — no mocking,
// no fixtures, every test starts clean.
#[cfg(test)]
mod tests {
    // `super::*` imports everything from the parent module (this file).
    use super::*;
    use axum::http::StatusCode;
    use serial_test::serial;
    use std::collections::HashSet;
    // `axum_test::TestServer` wraps the router and lets us make HTTP requests
    // in tests without opening a real TCP socket.
    use axum_test::TestServer;

    // Helper: create a TestServer backed by an in-memory database.
    // Called at the start of each test that needs a server.
    async fn test_app() -> TestServer {
        let db = db::connect_mem().await.expect("failed to connect mem db");
        let state = AppState::new(db);
        let router = app(state, "./dist", "dev").await;
        TestServer::new(router).unwrap()
    }

    #[tokio::test]
    async fn audit_baseline_backfill_inserts_once_per_entity() {
        let db = db::connect_mem().await.expect("mem db");
        let bid = ulid::Ulid::new().to_string().to_lowercase();
        let cid = ulid::Ulid::new().to_string().to_lowercase();
        let kid = ulid::Ulid::new().to_string().to_lowercase();

        db.query("CREATE type::thing('boards', $bid) SET name = $name, last_edited_by = $sub")
            .bind(("bid", bid.clone()))
            .bind(("name", format!("seed-{bid}")))
            .bind(("sub", "preaudit-board-editor"))
            .await
            .unwrap()
            .check()
            .unwrap();

        db.query(
            "CREATE type::thing('columns', $cid) SET board = type::thing('boards', $bid), \
             name = $cname, position = 0, last_edited_by = $sub",
        )
        .bind(("cid", cid.clone()))
        .bind(("bid", bid.clone()))
        .bind(("cname", "Col"))
        .bind(("sub", "preaudit-column-editor"))
        .await
        .unwrap()
        .check()
        .unwrap();

        db.query(
            "CREATE type::thing('cards', $kid) SET column = type::thing('columns', $cid), \
             body = $body, position = 0, number = 1, last_edited_by = $sub",
        )
        .bind(("kid", kid.clone()))
        .bind(("cid", cid.clone()))
        .bind(("body", "hello"))
        .bind(("sub", "preaudit-card-editor"))
        .await
        .unwrap()
        .check()
        .unwrap();

        audit::migrate_audit_baselines(&db).await.unwrap();

        let baselines: Vec<crate::models::DbAuditLog> = db
            .query("SELECT * FROM audit_log WHERE action = 'baseline' ORDER BY entity_type ASC")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(baselines.len(), 3);

        let board_row = baselines.iter().find(|r| r.entity_type == "board").unwrap();
        assert_eq!(board_row.entity_id, bid);
        assert_eq!(board_row.actor_sub, "preaudit-board-editor");

        audit::migrate_audit_baselines(&db).await.unwrap();
        let baselines_again: Vec<crate::models::DbAuditLog> = db
            .query("SELECT * FROM audit_log WHERE action = 'baseline'")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(baselines_again.len(), 3);
    }

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
    async fn create_board_starts_empty() {
        // New boards have no default columns — the user creates them manually.
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "empty-board".to_string(),
            })
            .await;
        create_resp.assert_status(StatusCode::CREATED);
        let board: shared::Board = create_resp.json();

        let list_resp = server
            .get(&format!("/api/boards/{}/columns", board.name))
            .await;
        list_resp.assert_status_ok();
        let columns: Vec<shared::Column> = list_resp.json();
        assert_eq!(columns.len(), 0, "new board must have no default columns");
    }

    #[tokio::test]
    async fn create_board_and_list() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "test-board".to_string(),
            })
            .await;
        create_resp.assert_status(StatusCode::CREATED);
        let board: shared::Board = create_resp.json();
        assert_eq!(board.name, "test-board");

        let list_resp = server.get("/api/boards").await;
        list_resp.assert_status_ok();
        let boards: Vec<shared::Board> = list_resp.json();
        // `.any(...)` returns true if at least one element satisfies the predicate.
        assert!(boards.iter().any(|b| b.id == board.id));
    }

    #[tokio::test]
    async fn create_board_invalid_name_returns_422() {
        let server = test_app().await;
        // Names with spaces, uppercase, or leading/trailing hyphens are rejected.
        for bad in &["My Board", "UPPER", "-leading", "trailing-", ""] {
            server
                .post("/api/boards")
                .json(&shared::CreateBoardRequest {
                    name: bad.to_string(),
                })
                .await
                .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[tokio::test]
    async fn create_board_duplicate_name_returns_409() {
        let server = test_app().await;
        server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "dupe-board".to_string(),
            })
            .await
            .assert_status(StatusCode::CREATED);
        server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "dupe-board".to_string(),
            })
            .await
            .assert_status(StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn get_board_by_slug() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "get-me".to_string(),
            })
            .await;
        let board: shared::Board = create_resp.json();

        let get_resp = server.get(&format!("/api/boards/{}", board.name)).await;
        get_resp.assert_status_ok();
        let fetched: shared::Board = get_resp.json();
        assert_eq!(fetched.id, board.id);
        assert_eq!(fetched.name, "get-me");
    }

    #[tokio::test]
    async fn update_board_name() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "old-name".to_string(),
            })
            .await;
        let board: shared::Board = create_resp.json();

        let update_resp = server
            .put(&format!("/api/boards/{}", board.name))
            .json(&shared::UpdateBoardRequest {
                name: "new-name".to_string(),
            })
            .await;
        update_resp.assert_status_ok();
        let updated: shared::Board = update_resp.json();
        assert_eq!(updated.name, "new-name");

        // After rename, fetch by the new slug.
        let get_resp = server.get(&format!("/api/boards/{}", updated.name)).await;
        let fetched: shared::Board = get_resp.json();
        assert_eq!(fetched.name, "new-name");
    }

    #[tokio::test]
    async fn delete_board_returns_404_on_get() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "delete-me".to_string(),
            })
            .await;
        let board: shared::Board = create_resp.json();

        let del_resp = server.delete(&format!("/api/boards/{}", board.name)).await;
        del_resp.assert_status(StatusCode::NO_CONTENT);

        let get_resp = server.get(&format!("/api/boards/{}", board.name)).await;
        get_resp.assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_column_and_list() {
        let server = test_app().await;

        let create_board_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "board-with-columns".to_string(),
            })
            .await;
        let board: shared::Board = create_board_resp.json();

        let create_col_resp = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "To Do".to_string(),
                position: 0,
            })
            .await;
        create_col_resp.assert_status(StatusCode::CREATED);
        let column: shared::Column = create_col_resp.json();
        assert_eq!(column.name, "To Do");
        assert_eq!(column.board_id, board.id);

        let list_resp = server
            .get(&format!("/api/boards/{}/columns", board.name))
            .await;
        list_resp.assert_status_ok();
        let columns: Vec<shared::Column> = list_resp.json();
        assert!(columns.iter().any(|c| c.id == column.id));
    }

    #[tokio::test]
    async fn delete_board_cascades_columns() {
        let server = test_app().await;

        let create_board_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "board-for-cascade".to_string(),
            })
            .await;
        let board: shared::Board = create_board_resp.json();

        let create_col_resp = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Col 1".to_string(),
                position: 0,
            })
            .await;
        let column: shared::Column = create_col_resp.json();

        server
            .delete(&format!("/api/boards/{}", board.name))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let update_resp = server
            .put(&format!("/api/columns/{}", column.id))
            .json(&shared::UpdateColumnRequest {
                name: Some("Updated".to_string()),
                position: None,
            })
            .await;
        update_resp.assert_status(StatusCode::NOT_FOUND);
    }

    // Shared helper used by several card tests. Creates a board and then adds
    // a fresh column named "Col".
    async fn setup_board_and_column(server: &TestServer) -> (shared::Board, shared::Column) {
        let board: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "test-board".to_string(),
            })
            .await
            .json();
        let column: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Col".to_string(),
                position: 0,
            })
            .await
            .json();
        (board, column)
    }

    #[tokio::test]
    async fn create_card_and_list() {
        let server = test_app().await;
        // `_` discards the board; we only need the column.
        let (_, column) = setup_board_and_column(&server).await;

        let create_resp = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Fix bug\n\nDetails here".to_string(),
                ..Default::default()
            })
            .await;
        create_resp.assert_status(StatusCode::CREATED);
        let card: shared::Card = create_resp.json();
        assert_eq!(card.body, "# Fix bug\n\nDetails here");
        assert_eq!(card.column_id, column.id);

        let list_resp = server
            .get(&format!("/api/columns/{}/cards", column.id))
            .await;
        list_resp.assert_status_ok();
        let cards: Vec<shared::Card> = list_resp.json();
        assert!(cards.iter().any(|c| c.id == card.id));
    }

    #[tokio::test]
    async fn update_card_body() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Old body".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let update_resp = server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("# New body\n\nWith details".to_string()),
                ..Default::default()
            })
            .await;
        update_resp.assert_status_ok();
        let updated: shared::Card = update_resp.json();
        assert_eq!(updated.body, "# New body\n\nWith details");
    }

    #[tokio::test]
    async fn card_updates_same_audit_session_merge_into_one_row() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "alpha".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let sess = "merge-test-session";
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("beta".to_string()),
                audit_edit_session: Some(sess.to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("gamma".to_string()),
                audit_edit_session: Some(sess.to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();

        let updates: Vec<_> = hist.iter().filter(|e| e.action == "update").collect();
        assert_eq!(updates.len(), 1);
        let row = updates[0];
        assert_eq!(
            row.snapshot_before
                .as_ref()
                .and_then(|v| v.get("body"))
                .and_then(|v| v.as_str()),
            Some("alpha")
        );
        assert_eq!(
            row.snapshot_after
                .as_ref()
                .and_then(|v| v.get("body"))
                .and_then(|v| v.as_str()),
            Some("gamma")
        );
    }

    #[tokio::test]
    async fn column_history_endpoint_returns_cards_for_that_column_only() {
        let server = test_app().await;
        let (board, col_a) = setup_board_and_column(&server).await;

        let col_b: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "B".to_string(),
                position: 1,
            })
            .await
            .json();

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", col_a.id))
            .json(&shared::CreateCardRequest {
                body: "only-a".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let hist_a: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/columns/{}/history", col_a.id))
            .await
            .json();

        assert!(hist_a.iter().any(|e| {
            e.entity_type == "card" && e.entity_id == card.id && e.action == "create"
        }));
        assert!(
            !hist_a
                .iter()
                .any(|e| e.entity_type == "column" && e.entity_id == col_b.id)
        );
    }

    #[tokio::test]
    async fn card_updates_without_audit_session_stay_separate_rows() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "a".to_string(),
                ..Default::default()
            })
            .await
            .json();

        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("b".to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("c".to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();

        let updates: Vec<_> = hist.iter().filter(|e| e.action == "update").collect();
        assert_eq!(updates.len(), 2);
    }

    #[tokio::test]
    async fn update_card_body_and_column_change_audit_action_is_update_not_move() {
        let server = test_app().await;
        let (board, col_a) = setup_board_and_column(&server).await;

        let col_b: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Col B".to_string(),
                position: 1,
            })
            .await
            .json();

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", col_a.id))
            .json(&shared::CreateCardRequest {
                body: "original".to_string(),
                ..Default::default()
            })
            .await
            .json();

        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("edited after move".to_string()),
                column_id: Some(col_b.id.clone()),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();

        let layout_mutations: Vec<_> = hist
            .iter()
            .filter(|e| e.action == "update" || e.action == "move")
            .collect();
        assert_eq!(layout_mutations.len(), 1);
        assert_eq!(layout_mutations[0].action, "update");
    }

    // ── Card tags (iteration 41 — card #292) ────────────────────────────

    #[tokio::test]
    async fn create_card_persists_tags() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let create_resp = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Tagged".to_string(),
                tags: vec!["bug".to_string(), "urgent".to_string()],
            })
            .await;
        create_resp.assert_status(StatusCode::CREATED);
        let card: shared::Card = create_resp.json();
        assert_eq!(card.tags, vec!["bug".to_string(), "urgent".to_string()]);

        // Round-trips through a fresh read, not just the create response.
        let fetched: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
        assert_eq!(fetched.tags, vec!["bug".to_string(), "urgent".to_string()]);
    }

    #[tokio::test]
    async fn create_card_without_tags_defaults_to_empty_list() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Plain".to_string(),
                ..Default::default()
            })
            .await
            .json();
        assert!(card.tags.is_empty());
    }

    #[tokio::test]
    async fn update_card_replaces_the_whole_tag_list() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Tagged".to_string(),
                tags: vec!["bug".to_string(), "stale".to_string()],
            })
            .await
            .json();

        let updated: shared::Card = server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                tags: Some(vec!["fresh".to_string()]),
                ..Default::default()
            })
            .await
            .json();
        // Full replace, not a merge: both original tags are gone.
        assert_eq!(updated.tags, vec!["fresh".to_string()]);
        // A tags-only update leaves the body alone.
        assert_eq!(updated.body, "# Tagged");
    }

    #[tokio::test]
    async fn update_card_omitting_tags_leaves_them_untouched() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Tagged".to_string(),
                tags: vec!["keep".to_string()],
            })
            .await
            .json();

        let updated: shared::Card = server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("# Retitled".to_string()),
                ..Default::default()
            })
            .await
            .json();
        assert_eq!(updated.tags, vec!["keep".to_string()]);
    }

    #[tokio::test]
    async fn tags_are_normalized_on_the_way_in() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Messy tags".to_string(),
                tags: vec![
                    "  #bug ".to_string(),
                    "BUG".to_string(),
                    "two words".to_string(),
                    "   ".to_string(),
                ],
            })
            .await
            .json();
        // `#` stripped, whitespace split, case-insensitive dedup, empties dropped.
        assert_eq!(
            card.tags,
            vec!["bug".to_string(), "two".to_string(), "words".to_string()]
        );
    }

    #[tokio::test]
    async fn over_long_tag_is_rejected_with_422() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let too_long = "x".repeat(shared::tags::MAX_TAG_CHARS + 1);
        server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Nope".to_string(),
                tags: vec![too_long.clone()],
            })
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Fine".to_string(),
                ..Default::default()
            })
            .await
            .json();
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                tags: Some(vec![too_long]),
                ..Default::default()
            })
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn tag_change_records_a_discrete_update_audit_row() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Tag me".to_string(),
                ..Default::default()
            })
            .await
            .json();

        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                tags: Some(vec!["bug".to_string()]),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        let updates: Vec<_> = hist.iter().filter(|e| e.action == "update").collect();
        assert_eq!(updates.len(), 1);
        let row = updates[0];
        assert_eq!(
            row.snapshot_before
                .as_ref()
                .and_then(|v| v.get("tags"))
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(
            row.snapshot_after
                .as_ref()
                .and_then(|v| v.get("tags"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str()),
            Some("bug")
        );
        // A tag change is content, not layout — never filed under "move".
        assert!(!hist.iter().any(|e| e.action == "move"));
    }

    #[tokio::test]
    async fn tag_change_does_not_merge_into_a_body_edit_session() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "alpha".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let sess = "tag-session-test";
        // A body edit inside an edit session…
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("beta".to_string()),
                audit_edit_session: Some(sess.to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();
        // …then a tag change carrying the *same* session token. It must still
        // land as its own row rather than being folded into the body edit.
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                tags: Some(vec!["bug".to_string()]),
                audit_edit_session: Some(sess.to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        let updates: Vec<_> = hist.iter().filter(|e| e.action == "update").collect();
        assert_eq!(
            updates.len(),
            2,
            "tag change must not merge into the session"
        );
        // And a later body save can no longer merge into the tag row either,
        // because that row carries no session token.
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("gamma".to_string()),
                audit_edit_session: Some(sess.to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();
        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        assert_eq!(hist.iter().filter(|e| e.action == "update").count(), 3);
    }

    #[tokio::test]
    async fn re_sending_identical_tags_is_not_an_edit() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Steady".to_string(),
                tags: vec!["bug".to_string()],
            })
            .await
            .json();

        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                tags: Some(vec!["bug".to_string()]),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        assert_eq!(hist.iter().filter(|e| e.action == "update").count(), 0);
    }

    #[tokio::test]
    async fn restoring_a_version_restores_body_and_tags_together() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Original".to_string(),
                tags: vec!["first".to_string()],
            })
            .await
            .json();

        // Move both halves away from the create snapshot.
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("# Rewritten".to_string()),
                tags: Some(vec!["second".to_string()]),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        let create_row = hist
            .iter()
            .find(|e| e.action == "create")
            .expect("create row");

        server
            .post(&format!("/api/audit/{}/restore", create_row.id))
            .await
            .assert_status_ok();

        let restored: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
        assert_eq!(restored.body, "# Original");
        assert_eq!(restored.tags, vec!["first".to_string()]);
    }

    #[tokio::test]
    async fn restoring_a_tags_only_version_is_not_a_conflict() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Same body".to_string(),
                tags: vec!["keep".to_string()],
            })
            .await
            .json();

        // Only the tags change, so the create row's body already matches the
        // card. Restoring it must still put the old tags back rather than
        // reporting "already current".
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                tags: Some(vec!["dropped".to_string()]),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        let create_row = hist
            .iter()
            .find(|e| e.action == "create")
            .expect("create row");

        server
            .post(&format!("/api/audit/{}/restore", create_row.id))
            .await
            .assert_status_ok();

        let restored: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
        assert_eq!(restored.tags, vec!["keep".to_string()]);
    }

    #[tokio::test]
    async fn restoring_a_deleted_card_brings_its_tags_back() {
        let server = test_app().await;
        let (board, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Doomed".to_string(),
                tags: vec!["bug".to_string()],
            })
            .await
            .json();

        server
            .delete(&format!("/api/cards/{}", card.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // The card-scoped endpoint 404s once the card is gone, so read the
        // delete row from the board's history instead.
        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json();
        let delete_row = hist
            .iter()
            .find(|e| e.action == "delete" && e.entity_id == card.id)
            .expect("delete row");

        server
            .post(&format!("/api/audit/{}/restore", delete_row.id))
            .await
            .assert_status_ok();

        let restored: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
        assert_eq!(restored.tags, vec!["bug".to_string()]);
    }

    #[tokio::test]
    async fn legacy_cards_without_a_tags_field_stay_readable_and_editable() {
        // Rows written before `tags` was defined have no value for it at all.
        // Reproduce that by applying the schema with the tags lines stripped,
        // writing a card, then applying the real schema on top — exactly what a
        // deploy of this iteration does to an existing database.
        let schema = include_str!("schema.surql");
        let legacy_schema: String = schema
            .lines()
            .filter(|line| !line.contains("tags"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !legacy_schema.contains("tags"),
            "legacy schema must not define tags"
        );

        let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .expect("mem db");
        db.use_ns("bored").use_db("bored").await.unwrap();
        db.query(legacy_schema).await.unwrap().check().unwrap();

        let bid = ulid::Ulid::new().to_string().to_lowercase();
        let cid = ulid::Ulid::new().to_string().to_lowercase();
        let kid = ulid::Ulid::new().to_string().to_lowercase();
        db.query("CREATE type::thing('boards', $bid) SET name = $name")
            .bind(("bid", bid.clone()))
            .bind(("name", format!("legacy-{bid}")))
            .await
            .unwrap()
            .check()
            .unwrap();
        db.query(
            "CREATE type::thing('columns', $cid) SET board = type::thing('boards', $bid), \
             name = 'Col', position = 0",
        )
        .bind(("cid", cid.clone()))
        .bind(("bid", bid.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();
        db.query(
            "CREATE type::thing('cards', $kid) SET column = type::thing('columns', $cid), \
             body = 'legacy', position = 0, number = 1",
        )
        .bind(("kid", kid.clone()))
        .bind(("cid", cid.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();

        // Now upgrade: the real schema defines `tags` and backfills it.
        db.query(schema).await.unwrap().check().unwrap();

        let card: Option<crate::models::DbCard> = db.select(("cards", &kid)).await.unwrap();
        let card = card.expect("legacy card still readable");
        assert!(card.tags.is_empty());

        // The backfill is what makes this work: on a SCHEMAFULL table an UPDATE
        // of a row whose `tags` is still missing fails the type check.
        db.query("UPDATE type::thing('cards', $kid) SET body = 'edited'")
            .bind(("kid", kid))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    #[tokio::test]
    async fn move_card_between_columns() {
        let server = test_app().await;
        let (board, col_a) = setup_board_and_column(&server).await;

        let col_b: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Col B".to_string(),
                position: 1,
            })
            .await
            .json();

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", col_a.id))
            .json(&shared::CreateCardRequest {
                body: "Movable card".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let move_resp = server
            .post(&format!("/api/cards/{}/move", card.id))
            .json(&shared::MoveCardRequest {
                column_id: col_b.id.clone(),
                position: 0,
            })
            .await;
        move_resp.assert_status_ok();
        let moved: shared::Card = move_resp.json();
        assert_eq!(moved.column_id, col_b.id);

        // Verify the card is no longer in col_a.
        let cards_a: Vec<shared::Card> = server
            .get(&format!("/api/columns/{}/cards", col_a.id))
            .await
            .json();
        assert!(!cards_a.iter().any(|c| c.id == card.id));

        // Verify the card is now in col_b.
        let cards_b: Vec<shared::Card> = server
            .get(&format!("/api/columns/{}/cards", col_b.id))
            .await
            .json();
        assert!(cards_b.iter().any(|c| c.id == card.id));
    }

    #[tokio::test]
    async fn delete_card() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "To Delete".to_string(),
                ..Default::default()
            })
            .await
            .json();

        server
            .delete(&format!("/api/cards/{}", card.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let cards: Vec<shared::Card> = server
            .get(&format!("/api/columns/{}/cards", column.id))
            .await
            .json();
        assert!(!cards.iter().any(|c| c.id == card.id));
    }

    #[tokio::test]
    async fn delete_column_cascades_cards() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "Orphan card".to_string(),
                ..Default::default()
            })
            .await
            .json();

        server
            .delete(&format!("/api/columns/{}", column.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // The card should be gone — trying to update it should 404.
        let resp = server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("x".to_string()),
                ..Default::default()
            })
            .await;
        resp.assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_board_cascades_columns_and_cards() {
        let server = test_app().await;
        let (board, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "Deep Orphan card".to_string(),
                ..Default::default()
            })
            .await
            .json();

        server
            .delete(&format!("/api/boards/{}", board.name))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Both the column and its card should be gone after board deletion.
        let col_resp = server
            .put(&format!("/api/columns/{}", column.id))
            .json(&shared::UpdateColumnRequest {
                name: Some("x".to_string()),
                position: None,
            })
            .await;
        col_resp.assert_status(StatusCode::NOT_FOUND);

        let card_resp = server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("x".to_string()),
                ..Default::default()
            })
            .await;
        card_resp.assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cards_returned_ordered_by_position() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let c1: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "Card 1".to_string(),
                ..Default::default()
            })
            .await
            .json();
        let c2: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "Card 2".to_string(),
                ..Default::default()
            })
            .await
            .json();
        let c3: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "Card 3".to_string(),
                ..Default::default()
            })
            .await
            .json();

        // Deliberately set positions out of insertion order to verify sorting.
        server
            .put(&format!("/api/cards/{}", c1.id))
            .json(&shared::UpdateCardRequest {
                position: Some(2),
                ..Default::default()
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", c2.id))
            .json(&shared::UpdateCardRequest {
                position: Some(0),
                ..Default::default()
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", c3.id))
            .json(&shared::UpdateCardRequest {
                position: Some(1),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let cards: Vec<shared::Card> = server
            .get(&format!("/api/columns/{}/cards", column.id))
            .await
            .json();

        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].body, "Card 2"); // position 0
        assert_eq!(cards[1].body, "Card 3"); // position 1
        assert_eq!(cards[2].body, "Card 1"); // position 2
    }

    #[tokio::test]
    async fn create_card_in_nonexistent_column_returns_404() {
        let server = test_app().await;
        server
            .post("/api/columns/doesnotexist/cards")
            .json(&shared::CreateCardRequest {
                body: "Ghost card".to_string(),
                ..Default::default()
            })
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn move_card_to_nonexistent_column_returns_404() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "Movable card".to_string(),
                ..Default::default()
            })
            .await
            .json();

        server
            .post(&format!("/api/cards/{}/move", card.id))
            .json(&shared::MoveCardRequest {
                column_id: "doesnotexist".to_string(),
                position: 0,
            })
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_nonexistent_card_returns_404() {
        let server = test_app().await;
        server
            .put("/api/cards/doesnotexist")
            .json(&shared::UpdateCardRequest {
                body: Some("x".to_string()),
                ..Default::default()
            })
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_card_by_id() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Get me".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let resp = server.get(&format!("/api/cards/{}", card.id)).await;
        resp.assert_status_ok();
        let fetched: shared::Card = resp.json();
        assert_eq!(fetched.id, card.id);
        assert_eq!(fetched.body, "# Get me");
    }

    #[tokio::test]
    async fn get_card_by_number() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        // Create a card and capture its sequential number assigned by the backend.
        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Number me".to_string(),
                ..Default::default()
            })
            .await
            .json();

        // Fetch the same card via the human-readable number endpoint.
        let resp = server
            .get(&format!("/api/cards/by-number/{}", card.number))
            .await;
        resp.assert_status_ok();
        let fetched: shared::Card = resp.json();
        assert_eq!(fetched.id, card.id);
        assert_eq!(fetched.number, card.number);
        assert_eq!(fetched.body, "# Number me");
    }

    #[tokio::test]
    async fn get_card_by_nonexistent_number_returns_404() {
        let server = test_app().await;
        // u32::MAX is extremely unlikely to be a real card number in tests.
        server
            .get(&format!("/api/cards/by-number/{}", u32::MAX))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_nonexistent_card_returns_404() {
        let server = test_app().await;
        server
            .get("/api/cards/doesnotexist")
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_nonexistent_card_returns_404() {
        let server = test_app().await;
        server
            .delete("/api/cards/doesnotexist")
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn card_response_has_body_not_title_or_description() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# My Card\n\nSome content".to_string(),
                ..Default::default()
            })
            .await
            .json();

        // Verify the full body is preserved verbatim.
        assert_eq!(card.body, "# My Card\n\nSome content");
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

    // Verifies that reorder_columns assigns positions matching the supplied order
    // and returns the columns sorted by their new positions.
    #[tokio::test]
    async fn reorder_columns_assigns_positions() {
        let server = test_app().await;

        let board: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "reorder-board".to_string(),
            })
            .await
            .json();

        // Create three columns explicitly (no default columns since iteration 13).
        let col_a: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Todo".to_string(),
                position: 0,
            })
            .await
            .json();
        let col_b: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Done".to_string(),
                position: 1,
            })
            .await
            .json();
        let col_c: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "In Progress".to_string(),
                position: 2,
            })
            .await
            .json();

        let col_todo = col_a.id.clone();
        let col_done = col_b.id.clone();
        let col_ip = col_c.id.clone();

        // Reorder to: In Progress, Todo, Done.
        let reorder_resp = server
            .put(&format!("/api/boards/{}/columns/reorder", board.name))
            .json(&shared::ColumnsReorderRequest {
                order: vec![col_ip.clone(), col_todo.clone(), col_done.clone()],
            })
            .await;
        reorder_resp.assert_status_ok();

        let reordered: Vec<shared::Column> = reorder_resp.json();
        assert_eq!(reordered.len(), 3);
        assert_eq!(reordered[0].id, col_ip);
        assert_eq!(reordered[0].position, 0);
        assert_eq!(reordered[1].id, col_todo);
        assert_eq!(reordered[1].position, 1);
        assert_eq!(reordered[2].id, col_done);
        assert_eq!(reordered[2].position, 2);
    }

    // Verifies that reorder_columns ignores column IDs that belong to a
    // different board, preventing cross-board IDOR position writes.
    #[tokio::test]
    async fn reorder_columns_rejects_foreign_column_ids() {
        let server = test_app().await;

        // Board A — we will try to tamper with its column from board B's endpoint.
        let board_a: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "board-a".to_string(),
            })
            .await
            .json();

        // Create a column on board A explicitly (no default columns since iteration 13).
        let col_a_todo: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board_a.name))
            .json(&shared::CreateColumnRequest {
                name: "Todo".to_string(),
                position: 0,
            })
            .await
            .json();
        let original_position = col_a_todo.position;

        // Board B — the attacker's board. Submit board A's column ID in the order.
        let board_b: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "board-b".to_string(),
            })
            .await
            .json();

        // Create two columns on board B explicitly.
        let col_b_todo: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board_b.name))
            .json(&shared::CreateColumnRequest {
                name: "Todo".to_string(),
                position: 0,
            })
            .await
            .json();
        let col_b_done: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board_b.name))
            .json(&shared::CreateColumnRequest {
                name: "Done".to_string(),
                position: 1,
            })
            .await
            .json();

        // Inject board A's column into board B's reorder request.
        // The WHERE board = … clause should make this a no-op for col_a_todo.
        let resp = server
            .put(&format!("/api/boards/{}/columns/reorder", board_b.name))
            .json(&shared::ColumnsReorderRequest {
                order: vec![
                    col_b_done.id.clone(),
                    col_a_todo.id.clone(), // foreign — must be ignored
                    col_b_todo.id.clone(),
                ],
            })
            .await;
        resp.assert_status_ok();

        // Board A's column must still have its original position.
        let cols_a_after: Vec<shared::Column> = server
            .get(&format!("/api/boards/{}/columns", board_a.name))
            .await
            .json();
        let col_a_todo_after = cols_a_after.iter().find(|c| c.id == col_a_todo.id).unwrap();
        assert_eq!(
            col_a_todo_after.position, original_position,
            "foreign column position must be unchanged after cross-board reorder"
        );
    }

    /// Every mutation now records an `AuditAppended` event before the domain
    /// `BoardEvent` — tests that care about the latter skip audit noise here.
    async fn recv_next_non_audit_board_event(
        rx: &mut tokio::sync::broadcast::Receiver<crate::events::BroadcastEvent>,
    ) -> crate::events::BoardEvent {
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("SSE recv timed out")
                .expect("broadcast channel closed");
            match msg.event {
                crate::events::BoardEvent::AuditAppended { .. } => continue,
                other => return other,
            }
        }
    }

    // Verifies that mutation routes emit the expected SSE events. We subscribe
    // to the broadcast channel before performing a mutation and check that the
    // correct event arrives with the right payload.
    #[tokio::test]
    async fn mutations_emit_sse_events() {
        let db = db::connect_mem().await.expect("failed to connect mem db");
        let state = AppState::new(db);
        // Subscribe *before* making requests so we don't miss any events.
        let mut rx = state.events.subscribe();

        let server = TestServer::new(app(state, "./dist", "dev").await).unwrap();

        // CREATE board → BoardCreated
        let board: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "event-board".to_string(),
            })
            .await
            .json();

        // Use a bounded async wait instead of try_recv so the test doesn't race
        // the handler. The send always happens before the HTTP response returns,
        // but relying on try_recv returning Ok rather than Empty is fragile under
        // a busy executor. 1 s is generous — in practice the channel is ready
        // in microseconds.
        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::BoardCreated { .. }));

        // CREATE column → ColumnCreated
        let col: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Col".to_string(),
                position: 0,
            })
            .await
            .json();

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::ColumnCreated { .. }));

        // Create a second column so there is somewhere to move the card to.
        let other_col: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Other Col".to_string(),
                position: 1,
            })
            .await
            .json();

        // Drain audit + ColumnCreated for other_col so it doesn't interfere.
        let _ = recv_next_non_audit_board_event(&mut rx).await;

        // CREATE card → CardCreated
        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", col.id))
            .json(&shared::CreateCardRequest {
                body: "hello".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::CardCreated { .. }));

        server
            .post(&format!("/api/cards/{}/move", card.id))
            .json(&shared::MoveCardRequest {
                column_id: other_col.id.clone(),
                position: 0,
            })
            .await
            .assert_status_ok();

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::CardMoved { .. }));

        // UPDATE card → CardUpdated
        server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("updated body".to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::CardUpdated { .. }));

        // DELETE card → CardDeleted
        server
            .delete(&format!("/api/cards/{}", card.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::CardDeleted { .. }));

        // UPDATE column → ColumnUpdated
        server
            .put(&format!("/api/columns/{}", col.id))
            .json(&shared::UpdateColumnRequest {
                name: Some("Renamed".to_string()),
                position: None,
            })
            .await
            .assert_status_ok();

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::ColumnUpdated { .. }));

        // REORDER columns → ColumnsReordered
        let cols: Vec<shared::Column> = server
            .get(&format!("/api/boards/{}/columns", board.name))
            .await
            .json();
        let order: Vec<String> = cols.iter().rev().map(|c| c.id.clone()).collect();
        server
            .put(&format!("/api/boards/{}/columns/reorder", board.name))
            .json(&shared::ColumnsReorderRequest { order })
            .await
            .assert_status_ok();

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::ColumnsReordered { .. }));

        // DELETE column → ColumnDeleted
        server
            .delete(&format!("/api/columns/{}", col.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::ColumnDeleted { .. }));

        // UPDATE board → BoardUpdated
        server
            .put(&format!("/api/boards/{}", board.name))
            .json(&shared::UpdateBoardRequest {
                name: "renamed-board".to_string(),
            })
            .await
            .assert_status_ok();

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::BoardUpdated { .. }));

        // DELETE board → BoardDeleted (use the updated name from the rename above)
        server
            .delete("/api/boards/renamed-board")
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let event = recv_next_non_audit_board_event(&mut rx).await;
        assert!(matches!(event, events::BoardEvent::BoardDeleted { .. }));
    }

    #[tokio::test]
    async fn audit_delete_card_then_restore_via_audit_endpoint() {
        let server = test_app().await;
        let (board, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Audit restore target".to_string(),
                ..Default::default()
            })
            .await
            .json();

        server
            .delete(&format!("/api/cards/{}", card.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let hist_resp = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await;
        hist_resp.assert_status_ok();
        let hist: Vec<shared::AuditLogEntry> = hist_resp.json();
        let delete_row = hist
            .iter()
            .find(|e| e.action == "delete" && e.entity_type == "card" && e.entity_id == card.id)
            .expect("delete audit row present");

        let restore_resp = server
            .post(&format!("/api/audit/{}/restore", delete_row.id))
            .await;
        restore_resp.assert_status_ok();

        server
            .get(&format!("/api/cards/{}", card.id))
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn audit_restore_prior_card_body_preserves_card_identity_and_layout() {
        let server = test_app().await;
        let (board, column) = setup_board_and_column(&server).await;

        let original: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# Version A".to_string(),
                ..Default::default()
            })
            .await
            .json();
        server
            .put(&format!("/api/cards/{}", original.id))
            .json(&shared::UpdateCardRequest {
                body: Some("# Version B".to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", original.id))
            .json(&shared::UpdateCardRequest {
                body: Some("# Version C".to_string()),
                ..Default::default()
            })
            .await
            .assert_status_ok();

        let history: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", original.id))
            .await
            .json();
        let version_b = history
            .iter()
            .find(|entry| {
                entry.action == "update"
                    && entry
                        .snapshot_after
                        .as_ref()
                        .and_then(|value| value.get("body"))
                        .and_then(|value| value.as_str())
                        == Some("# Version B")
            })
            .expect("Version B update present");

        let restore_response = server
            .post(&format!("/api/audit/{}/restore", version_b.id))
            .await;
        restore_response.assert_status_ok();
        let restored_rows: Vec<shared::AuditLogEntry> = restore_response.json();
        assert_eq!(restored_rows.len(), 1);
        let restore = &restored_rows[0];
        assert_eq!(restore.action, "restore");
        assert_eq!(
            restore.restored_from.as_deref(),
            Some(version_b.id.as_str())
        );
        assert_eq!(
            restore
                .snapshot_before
                .as_ref()
                .and_then(|value| value.get("body"))
                .and_then(|value| value.as_str()),
            Some("# Version C")
        );

        let restored: shared::Card = server
            .get(&format!("/api/cards/{}", original.id))
            .await
            .json();
        assert_eq!(restored.body, "# Version B");
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.number, original.number);
        assert_eq!(restored.column_id, original.column_id);
        assert_eq!(restored.position, original.position);

        let board_history: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json();
        assert!(board_history.iter().any(|entry| entry.id == restore.id));
    }

    #[tokio::test]
    async fn audit_body_restore_rejects_invalid_or_current_versions() {
        let db = db::connect_mem().await.expect("failed to connect mem db");
        let state = AppState::new(db.clone());
        let server = TestServer::new(app(state, "./dist", "dev").await).unwrap();
        let (board, column) = setup_board_and_column(&server).await;
        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "current body".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let card_history: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        let current = card_history
            .iter()
            .find(|entry| entry.action == "create")
            .expect("current create version present");
        server
            .post(&format!("/api/audit/{}/restore", current.id))
            .await
            .assert_status(StatusCode::CONFLICT);

        let malformed_id = ulid::Ulid::new().to_string().to_lowercase();
        db.query(
            "CREATE type::thing('audit_log', $id) SET \
             actor_sub = 'test', actor_display_name = 'Test', \
             entity_type = 'card', entity_id = $entity_id, board_id = $board_id, \
             action = 'update', snapshot_before = NONE, snapshot_after = $snapshot_after, \
             restored_from = NONE, batch_group = NONE, audit_edit_session = NONE",
        )
        .bind(("id", malformed_id.clone()))
        .bind(("entity_id", card.id.clone()))
        .bind(("board_id", board.id.clone()))
        .bind(("snapshot_after", serde_json::json!({ "id": card.id })))
        .await
        .expect("insert malformed audit row")
        .check()
        .expect("malformed audit row accepted by storage");
        server
            .post(&format!("/api/audit/{malformed_id}/restore"))
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

        let board_history: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json();
        let board_create = board_history
            .iter()
            .find(|entry| entry.entity_type == "board" && entry.action == "create")
            .expect("board create row present");
        server
            .post(&format!("/api/audit/{}/restore", board_create.id))
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

        let other_column: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Other".to_string(),
                position: 1,
            })
            .await
            .json();
        server
            .post(&format!("/api/cards/{}/move", card.id))
            .json(&shared::MoveCardRequest {
                column_id: other_column.id,
                position: 0,
            })
            .await
            .assert_status_ok();
        let history_after_move: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        let move_row = history_after_move
            .iter()
            .find(|entry| entry.action == "move")
            .expect("move row present");
        server
            .post(&format!("/api/audit/{}/restore", move_row.id))
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

        let unchanged: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
        assert_eq!(unchanged.body, "current body");
    }
    // ── Card links (iteration 42 — card #76) ─────────────────────────────

    /// Three cards in one column, ready to be linked.
    async fn setup_three_cards(
        server: &TestServer,
    ) -> (shared::Board, shared::Column, [shared::Card; 3]) {
        let (board, column) = setup_board_and_column(server).await;
        let mut cards = Vec::new();
        for title in ["# A", "# B", "# C"] {
            let card: shared::Card = server
                .post(&format!("/api/columns/{}/cards", column.id))
                .json(&shared::CreateCardRequest {
                    body: title.to_string(),
                    ..Default::default()
                })
                .await
                .json();
            cards.push(card);
        }
        let cards: [shared::Card; 3] = cards.try_into().expect("three cards");
        (board, column, cards)
    }

    /// `POST /api/cards/:id/links` with `other` as the successor of `card`.
    async fn link_after(
        server: &TestServer,
        card: &shared::Card,
        other: &shared::Card,
        reason: Option<&str>,
    ) -> axum_test::TestResponse {
        server
            .post(&format!("/api/cards/{}/links", card.id))
            .json(&shared::CreateCardLinkRequest {
                direction: shared::LinkDirection::Successor,
                other_card_id: other.id.clone(),
                reason: reason.map(str::to_string),
            })
            .await
    }

    async fn board_links(server: &TestServer, board: &shared::Board) -> Vec<shared::CardLink> {
        server
            .get(&format!("/api/boards/{}/links", board.name))
            .await
            .json()
    }

    #[tokio::test]
    async fn create_link_returns_it_with_both_card_numbers() {
        let server = test_app().await;
        let (board, _, [a, b, _]) = setup_three_cards(&server).await;

        let resp = link_after(&server, &a, &b, Some("  B needs A's API  ")).await;
        resp.assert_status(StatusCode::CREATED);
        let link: shared::CardLink = resp.json();
        assert_eq!(link.predecessor_id, a.id);
        assert_eq!(link.successor_id, b.id);
        // Numbers are projected from the cards, not stored on the link.
        assert_eq!(link.predecessor_number, a.number);
        assert_eq!(link.successor_number, b.number);
        // The reason is trimmed on the way in.
        assert_eq!(link.reason.as_deref(), Some("B needs A's API"));

        // The board listing round-trips the same row.
        let listed = board_links(&server, &board).await;
        assert_eq!(listed, vec![link]);
    }

    #[tokio::test]
    async fn link_is_one_fact_whichever_end_creates_it() {
        let server = test_app().await;
        let (board, _, [a, b, _]) = setup_three_cards(&server).await;

        // "A is a predecessor of B", asked from B's side …
        let from_b: shared::CardLink = server
            .post(&format!("/api/cards/{}/links", b.id))
            .json(&shared::CreateCardLinkRequest {
                direction: shared::LinkDirection::Predecessor,
                other_card_id: a.id.clone(),
                reason: None,
            })
            .await
            .json();
        assert_eq!(from_b.predecessor_id, a.id);
        assert_eq!(from_b.successor_id, b.id);

        // … is the same row as "B is a successor of A" asked from A's side,
        // so the second request is a conflict rather than a second link.
        link_after(&server, &a, &b, None)
            .await
            .assert_status(StatusCode::CONFLICT);
        assert_eq!(board_links(&server, &board).await.len(), 1);
    }

    #[tokio::test]
    async fn empty_reason_is_stored_as_none() {
        let server = test_app().await;
        let (_, _, [a, b, _]) = setup_three_cards(&server).await;
        let link: shared::CardLink = link_after(&server, &a, &b, Some("   ")).await.json();
        assert_eq!(link.reason, None);
    }

    #[tokio::test]
    async fn self_link_is_rejected_with_422() {
        let server = test_app().await;
        let (_, _, [a, _, _]) = setup_three_cards(&server).await;
        let resp = link_after(&server, &a, &a, None).await;
        resp.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(resp.text().contains("itself"));
    }

    #[tokio::test]
    async fn reciprocal_link_is_rejected_as_a_cycle() {
        let server = test_app().await;
        let (_, _, [a, b, _]) = setup_three_cards(&server).await;
        link_after(&server, &a, &b, None)
            .await
            .assert_status(StatusCode::CREATED);
        let resp = link_after(&server, &b, &a, None).await;
        resp.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(resp.text().contains("loop"));
    }

    #[tokio::test]
    async fn longer_cycle_is_rejected() {
        let server = test_app().await;
        let (board, _, [a, b, c]) = setup_three_cards(&server).await;
        link_after(&server, &a, &b, None)
            .await
            .assert_status(StatusCode::CREATED);
        link_after(&server, &b, &c, None)
            .await
            .assert_status(StatusCode::CREATED);
        // C → A would close A → B → C → A.
        link_after(&server, &c, &a, None)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        // A → C points the same way as the chain and is fine.
        link_after(&server, &a, &c, None)
            .await
            .assert_status(StatusCode::CREATED);
        assert_eq!(board_links(&server, &board).await.len(), 3);
    }

    #[tokio::test]
    async fn cross_board_link_is_rejected() {
        let server = test_app().await;
        let (_, _, [a, _, _]) = setup_three_cards(&server).await;

        let other_board: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "other-board".to_string(),
            })
            .await
            .json();
        let other_col: shared::Column = server
            .post(&format!("/api/boards/{}/columns", other_board.name))
            .json(&shared::CreateColumnRequest {
                name: "Col".to_string(),
                position: 0,
            })
            .await
            .json();
        let foreign: shared::Card = server
            .post(&format!("/api/columns/{}/cards", other_col.id))
            .json(&shared::CreateCardRequest {
                body: "# Elsewhere".to_string(),
                ..Default::default()
            })
            .await
            .json();

        let resp = link_after(&server, &a, &foreign, None).await;
        resp.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(resp.text().contains("same board"));
    }

    #[tokio::test]
    async fn over_long_reason_is_rejected() {
        let server = test_app().await;
        let (board, _, [a, b, _]) = setup_three_cards(&server).await;
        let long = "x".repeat(shared::links::MAX_REASON_CHARS + 1);
        link_after(&server, &a, &b, Some(&long))
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        // Nothing was written.
        assert!(board_links(&server, &board).await.is_empty());
    }

    #[tokio::test]
    async fn linking_an_unknown_card_is_404() {
        let server = test_app().await;
        let (_, _, [a, _, _]) = setup_three_cards(&server).await;
        server
            .post(&format!("/api/cards/{}/links", a.id))
            .json(&shared::CreateCardLinkRequest {
                direction: shared::LinkDirection::Successor,
                other_card_id: "01hzzzzzzzzzzzzzzzzzzzzzzz".to_string(),
                reason: None,
            })
            .await
            .assert_status(StatusCode::NOT_FOUND);
        server
            .post("/api/cards/01hzzzzzzzzzzzzzzzzzzzzzzz/links")
            .json(&shared::CreateCardLinkRequest {
                direction: shared::LinkDirection::Successor,
                other_card_id: a.id.clone(),
                reason: None,
            })
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn update_link_changes_only_the_reason() {
        let server = test_app().await;
        let (_, _, [a, b, _]) = setup_three_cards(&server).await;
        let link: shared::CardLink = link_after(&server, &a, &b, Some("first")).await.json();

        let updated: shared::CardLink = server
            .put(&format!("/api/links/{}", link.id))
            .json(&shared::UpdateCardLinkRequest {
                reason: Some("second".to_string()),
            })
            .await
            .json();
        assert_eq!(updated.id, link.id);
        assert_eq!(updated.predecessor_id, a.id);
        assert_eq!(updated.reason.as_deref(), Some("second"));

        // An empty reason clears it.
        let cleared: shared::CardLink = server
            .put(&format!("/api/links/{}", link.id))
            .json(&shared::UpdateCardLinkRequest {
                reason: Some(String::new()),
            })
            .await
            .json();
        assert_eq!(cleared.reason, None);

        // Unknown link → 404; over-long reason → 422.
        server
            .put("/api/links/01hzzzzzzzzzzzzzzzzzzzzzzz")
            .json(&shared::UpdateCardLinkRequest::default())
            .await
            .assert_status(StatusCode::NOT_FOUND);
        server
            .put(&format!("/api/links/{}", link.id))
            .json(&shared::UpdateCardLinkRequest {
                reason: Some("x".repeat(shared::links::MAX_REASON_CHARS + 1)),
            })
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn omitting_reason_on_update_clears_it() {
        // Pins the current behaviour: `UpdateCardLinkRequest.reason` is
        // `#[serde(default)]`, so a body with the key left out entirely
        // deserialises the same as `reason: None` and wipes it — unlike
        // `update_card`, which treats an absent field as untouched. If this
        // ever changes to match that convention, this test should change too.
        let server = test_app().await;
        let (_, _, [a, b, _]) = setup_three_cards(&server).await;
        let link: shared::CardLink = link_after(&server, &a, &b, Some("first")).await.json();

        let cleared: shared::CardLink = server
            .put(&format!("/api/links/{}", link.id))
            .json(&serde_json::json!({}))
            .await
            .json();
        assert_eq!(cleared.reason, None);
    }

    #[tokio::test]
    async fn re_sending_the_same_reason_writes_no_history() {
        let server = test_app().await;
        let (board, _, [a, b, _]) = setup_three_cards(&server).await;
        let link: shared::CardLink = link_after(&server, &a, &b, Some("why")).await.json();

        server
            .put(&format!("/api/links/{}", link.id))
            .json(&shared::UpdateCardLinkRequest {
                reason: Some("  why  ".to_string()),
            })
            .await
            .assert_status_ok();

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json();
        let link_rows: Vec<_> = hist
            .iter()
            .filter(|e| e.entity_type == "card_link")
            .collect();
        assert_eq!(link_rows.len(), 1, "only the create row");
        assert_eq!(link_rows[0].action, "create");
    }

    #[tokio::test]
    async fn delete_link_removes_it_from_the_board() {
        let server = test_app().await;
        let (board, _, [a, b, _]) = setup_three_cards(&server).await;
        let link: shared::CardLink = link_after(&server, &a, &b, None).await.json();

        server
            .delete(&format!("/api/links/{}", link.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert!(board_links(&server, &board).await.is_empty());
        server
            .delete(&format!("/api/links/{}", link.id))
            .await
            .assert_status(StatusCode::NOT_FOUND);

        // Once the link is gone the reverse direction is legal again.
        link_after(&server, &b, &a, None)
            .await
            .assert_status(StatusCode::CREATED);
    }

    #[tokio::test]
    async fn link_changes_appear_in_board_and_both_card_histories() {
        let server = test_app().await;
        let (board, _, [a, b, c]) = setup_three_cards(&server).await;
        let link: shared::CardLink = link_after(&server, &a, &b, Some("why")).await.json();
        server
            .put(&format!("/api/links/{}", link.id))
            .json(&shared::UpdateCardLinkRequest {
                reason: Some("because".to_string()),
            })
            .await
            .assert_status_ok();
        server
            .delete(&format!("/api/links/{}", link.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let actions = |rows: &[shared::AuditLogEntry]| -> Vec<String> {
            rows.iter()
                .filter(|e| e.entity_type == "card_link" && e.entity_id == link.id)
                .map(|e| e.action.clone())
                .collect()
        };

        // Board history: newest first.
        let board_hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json();
        assert_eq!(actions(&board_hist), vec!["delete", "update", "create"]);

        // Both cards see the same three rows; the unrelated card sees none.
        for card in [&a, &b] {
            let hist: Vec<shared::AuditLogEntry> = server
                .get(&format!("/api/cards/{}/history", card.id))
                .await
                .json();
            assert_eq!(actions(&hist), vec!["delete", "update", "create"]);
            // Interleaved correctly with the card's own rows: the card's
            // create row is the oldest thing in its history.
            assert_eq!(hist.last().map(|e| e.entity_type.as_str()), Some("card"));
        }
        let unrelated: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", c.id))
            .await
            .json();
        assert!(actions(&unrelated).is_empty());

        // The snapshots carry the card numbers so the drawer can label the row.
        let create_row = board_hist
            .iter()
            .find(|e| e.entity_type == "card_link" && e.action == "create")
            .expect("create row");
        let after = create_row.snapshot_after.as_ref().expect("snapshot");
        assert_eq!(after["predecessor_number"], a.number);
        assert_eq!(after["successor_number"], b.number);
        assert_eq!(after["reason"], "why");
    }

    #[tokio::test]
    async fn deleting_a_card_removes_its_links_and_records_them() {
        let server = test_app().await;
        let (board, _, [a, b, c]) = setup_three_cards(&server).await;
        let ab: shared::CardLink = link_after(&server, &a, &b, None).await.json();
        let bc: shared::CardLink = link_after(&server, &b, &c, None).await.json();

        server
            .delete(&format!("/api/cards/{}", b.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Both links touched B, so both are gone.
        assert!(board_links(&server, &board).await.is_empty());

        // A's history records the loss of its link even though A itself was
        // never touched.
        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", a.id))
            .await
            .json();
        assert!(
            hist.iter().any(|e| e.entity_type == "card_link"
                && e.entity_id == ab.id
                && e.action == "delete")
        );
        assert!(!hist.iter().any(|e| e.entity_id == bc.id));

        // The link delete rows land before the card delete row in time, so a
        // newest-first listing shows the card going last.
        let board_hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json();
        assert_eq!(board_hist[0].entity_type, "card");
        assert_eq!(board_hist[0].action, "delete");
        assert_eq!(board_hist[1].entity_type, "card_link");
        assert_eq!(board_hist[2].entity_type, "card_link");
    }

    #[tokio::test]
    async fn deleting_a_column_cascades_links_and_its_restore_skips_them() {
        let server = test_app().await;
        let (board, column, [a, b, _]) = setup_three_cards(&server).await;
        link_after(&server, &a, &b, None)
            .await
            .assert_status(StatusCode::CREATED);

        server
            .delete(&format!("/api/columns/{}", column.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert!(board_links(&server, &board).await.is_empty());

        // The cascade grouped the link delete with the column delete …
        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json();
        let col_delete = hist
            .iter()
            .find(|e| e.entity_type == "column" && e.action == "delete")
            .expect("column delete row");
        let link_delete = hist
            .iter()
            .find(|e| e.entity_type == "card_link" && e.action == "delete")
            .expect("link delete row");
        assert!(col_delete.batch_group.is_some());
        assert_eq!(link_delete.batch_group, col_delete.batch_group);

        // … but restoring the column brings back the cards and not the link.
        server
            .post(&format!("/api/audit/{}/restore", col_delete.id))
            .await
            .assert_status_ok();
        let cards: Vec<shared::Card> = server
            .get(&format!("/api/columns/{}/cards", column.id))
            .await
            .json();
        assert_eq!(cards.len(), 3);
        assert!(board_links(&server, &board).await.is_empty());
    }

    #[tokio::test]
    async fn deleting_a_board_cascades_its_links() {
        let server = test_app().await;
        let (board, _, [a, b, _]) = setup_three_cards(&server).await;
        let link: shared::CardLink = link_after(&server, &a, &b, None).await.json();

        server
            .delete(&format!("/api/boards/{}", board.name))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // The row itself is gone — a fresh board with the same slug has no links.
        let again: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: board.name.clone(),
            })
            .await
            .json();
        assert!(board_links(&server, &again).await.is_empty());
        server
            .delete(&format!("/api/links/{}", link.id))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn link_audit_rows_are_not_restorable() {
        let server = test_app().await;
        let (board, _, [a, b, _]) = setup_three_cards(&server).await;
        let link: shared::CardLink = link_after(&server, &a, &b, None).await.json();
        server
            .delete(&format!("/api/links/{}", link.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json();
        for row in hist.iter().filter(|e| e.entity_type == "card_link") {
            server
                .post(&format!("/api/audit/{}/restore", row.id))
                .await
                .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        }
        assert!(board_links(&server, &board).await.is_empty());
    }

    #[tokio::test]
    async fn links_for_an_unknown_board_are_404() {
        let server = test_app().await;
        server
            .get("/api/boards/no-such-board/links")
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    // ── PUT /api/columns/:id/cards/reorder ────────────────────────────────

    /// Create `bodies` as cards in `column`, top-to-bottom in the order given.
    /// `create_card` inserts at the *top*, so the list is created back to front.
    async fn seed_cards(
        server: &TestServer,
        column: &shared::Column,
        bodies: &[&str],
    ) -> Vec<shared::Card> {
        for body in bodies.iter().rev() {
            server
                .post(&format!("/api/columns/{}/cards", column.id))
                .json(&shared::CreateCardRequest {
                    body: (*body).to_string(),
                    ..Default::default()
                })
                .await
                .assert_status(StatusCode::CREATED);
        }
        server
            .get(&format!("/api/columns/{}/cards", column.id))
            .await
            .json()
    }

    /// `(id, position)` per card — `shared::Card` has no `PartialEq`, and
    /// these are the two fields a reorder is allowed to be judged on.
    fn card_positions(cards: &[shared::Card]) -> Vec<(String, i32)> {
        cards.iter().map(|c| (c.id.clone(), c.position)).collect()
    }

    async fn column_cards(server: &TestServer, column_id: &str) -> Vec<shared::Card> {
        server
            .get(&format!("/api/columns/{column_id}/cards"))
            .await
            .json()
    }

    /// Every `move` row recorded against a card on this board.
    async fn card_move_rows(
        server: &TestServer,
        board: &shared::Board,
    ) -> Vec<shared::AuditLogEntry> {
        server
            .get(&format!("/api/boards/{}/history", board.name))
            .await
            .json::<Vec<shared::AuditLogEntry>>()
            .into_iter()
            .filter(|e| e.entity_type == "card" && e.action == "move")
            .collect()
    }

    #[tokio::test]
    async fn reorder_cards_applies_and_persists_the_requested_order() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;
        let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
        let (a, b, g) = (
            cards[0].id.clone(),
            cards[1].id.clone(),
            cards[2].id.clone(),
        );

        let resp = server
            .put(&format!("/api/columns/{}/cards/reorder", column.id))
            .json(&shared::CardsReorderRequest {
                order: vec![g.clone(), a.clone(), b.clone()],
            })
            .await;
        resp.assert_status_ok();

        let returned: Vec<shared::Card> = resp.json();
        let ids: Vec<&str> = returned.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [g.as_str(), a.as_str(), b.as_str()]);

        // Re-reading must agree with the response: positions are what order the
        // column, so an ambiguous write would show up here and not above.
        let persisted = column_cards(&server, &column.id).await;
        let ids: Vec<&str> = persisted.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [g.as_str(), a.as_str(), b.as_str()]);
        // Distinct positions, strictly increasing — the property `ORDER BY
        // position ASC` needs to be deterministic.
        assert!(persisted.windows(2).all(|w| w[0].position < w[1].position));
    }

    #[tokio::test]
    async fn reordering_into_the_current_order_writes_nothing() {
        let server = test_app().await;
        let (board, column) = setup_board_and_column(&server).await;
        let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
        let before = card_positions(&column_cards(&server, &column.id).await);
        let moves_before = card_move_rows(&server, &board).await.len();

        // Cards created into a fresh column land on 256/512/1024 — bisected,
        // not GAP multiples. A handler that renumbered unconditionally would
        // rewrite all three here and look correct while churning history.
        assert_eq!(
            before.iter().map(|(_, pos)| *pos).collect::<Vec<_>>(),
            [256, 512, 1024]
        );

        server
            .put(&format!("/api/columns/{}/cards/reorder", column.id))
            .json(&shared::CardsReorderRequest {
                order: cards.iter().map(|c| c.id.clone()).collect(),
            })
            .await
            .assert_status_ok();

        let after = card_positions(&column_cards(&server, &column.id).await);
        assert_eq!(before, after, "a no-op reorder must not touch any card");
        assert_eq!(
            card_move_rows(&server, &board).await.len(),
            moves_before,
            "a no-op reorder must not write history"
        );
    }

    #[tokio::test]
    async fn reorder_cards_leaves_an_unmoved_cards_position_alone() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;
        let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
        let (a, b, g) = (
            cards[0].id.clone(),
            cards[1].id.clone(),
            cards[2].id.clone(),
        );
        let alpha_position = cards[0].position;

        // Swap the bottom two; alpha stays on top and must keep its exact value.
        server
            .put(&format!("/api/columns/{}/cards/reorder", column.id))
            .json(&shared::CardsReorderRequest {
                order: vec![a.clone(), g.clone(), b.clone()],
            })
            .await
            .assert_status_ok();

        let after = column_cards(&server, &column.id).await;
        assert_eq!(after[0].id, a);
        assert_eq!(after[0].position, alpha_position);
        let ids: Vec<&str> = after.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [a.as_str(), g.as_str(), b.as_str()]);
    }

    #[tokio::test]
    async fn reorder_cards_repairs_duplicate_positions() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;
        let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
        let (a, b, g) = (
            cards[0].id.clone(),
            cards[1].id.clone(),
            cards[2].id.clone(),
        );

        // `PUT /api/cards/:id` writes `position` verbatim and nothing enforces
        // uniqueness, so a column can legally reach this state.
        for id in [&b, &g] {
            server
                .put(&format!("/api/cards/{id}"))
                .json(&shared::UpdateCardRequest {
                    position: Some(cards[0].position),
                    ..Default::default()
                })
                .await
                .assert_status_ok();
        }

        server
            .put(&format!("/api/columns/{}/cards/reorder", column.id))
            .json(&shared::CardsReorderRequest {
                order: vec![g.clone(), b.clone(), a.clone()],
            })
            .await
            .assert_status_ok();

        let after = column_cards(&server, &column.id).await;
        let ids: Vec<&str> = after.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [g.as_str(), b.as_str(), a.as_str()]);
        // Slot reuse is impossible here, so every card is renumbered onto the
        // same GAP grid `rebalance_column` uses.
        assert_eq!(
            after.iter().map(|c| c.position).collect::<Vec<_>>(),
            [1024, 2048, 3072]
        );
    }

    #[tokio::test]
    async fn reorder_cards_ignores_ids_from_another_column() {
        let server = test_app().await;
        let (board, column) = setup_board_and_column(&server).await;
        let other: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.name))
            .json(&shared::CreateColumnRequest {
                name: "Other".to_string(),
                position: 1,
            })
            .await
            .json();
        let cards = seed_cards(&server, &column, &["alpha", "beta"]).await;
        let outsider = seed_cards(&server, &other, &["zulu"]).await[0].clone();
        let outsider_before = outsider.clone();

        server
            .put(&format!("/api/columns/{}/cards/reorder", column.id))
            .json(&shared::CardsReorderRequest {
                order: vec![
                    outsider.id.clone(),
                    cards[1].id.clone(),
                    cards[0].id.clone(),
                ],
            })
            .await
            .assert_status_ok();

        // The foreign id is dropped, not honoured and not fatal.
        let after = column_cards(&server, &column.id).await;
        let ids: Vec<&str> = after.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [cards[1].id.as_str(), cards[0].id.as_str()]);
        // …and the other column is untouched — this is the IDOR guard.
        assert_eq!(
            card_positions(&column_cards(&server, &other.id).await),
            [(outsider_before.id, outsider_before.position)]
        );
    }

    #[tokio::test]
    async fn reorder_cards_appends_omitted_ids_at_the_bottom() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;
        let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
        let (a, b, g) = (
            cards[0].id.clone(),
            cards[1].id.clone(),
            cards[2].id.clone(),
        );

        // Only two of the three named — as would happen if a card were created
        // between the client reading the column and pressing the button.
        server
            .put(&format!("/api/columns/{}/cards/reorder", column.id))
            .json(&shared::CardsReorderRequest {
                order: vec![g.clone(), b.clone()],
            })
            .await
            .assert_status_ok();

        let after = column_cards(&server, &column.id).await;
        let ids: Vec<&str> = after.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [g.as_str(), b.as_str(), a.as_str()]);
    }

    #[tokio::test]
    async fn reorder_cards_records_one_batch_group_for_the_whole_move() {
        let server = test_app().await;
        let (board, column) = setup_board_and_column(&server).await;
        let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;

        server
            .put(&format!("/api/columns/{}/cards/reorder", column.id))
            .json(&shared::CardsReorderRequest {
                order: cards.iter().rev().map(|c| c.id.clone()).collect(),
            })
            .await
            .assert_status_ok();

        let moves = card_move_rows(&server, &board).await;
        assert!(!moves.is_empty(), "reversing the column must record moves");
        let groups: HashSet<Option<String>> = moves.iter().map(|m| m.batch_group.clone()).collect();
        assert_eq!(groups.len(), 1, "all moves belong to one batch");
        assert!(
            groups.iter().all(Option::is_some),
            "batch group must be recorded, not left null"
        );
    }

    #[tokio::test]
    async fn reorder_cards_for_an_unknown_column_is_404() {
        let server = test_app().await;
        server
            .put("/api/columns/01hzzzzzzzzzzzzzzzzzzzzzzz/cards/reorder")
            .json(&shared::CardsReorderRequest { order: vec![] })
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
}
