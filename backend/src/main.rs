// Declare submodules — Rust looks for each in a file named `src/<name>.rs`.
// These are private by default; the route handlers are reached via `routes::boards::...`.
mod db;
mod events;
mod models;
mod observability;
mod routes;

use axum::{
    routing::{delete, get, post, put}, // HTTP method helpers for the router
    Router,
};
use axum_server::tls_rustls::RustlsConfig; // TLS support using rustls (pure-Rust TLS)
use routes::boards::AppState;
use std::net::SocketAddr;
use tower_http::{services::ServeDir, trace::TraceLayer}; // Middleware: static files + request tracing

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
// in-memory DB, call `app(state).await`, and pass the router to `TestServer`.
pub async fn app(state: AppState) -> Router {
    // Build the `/api/*` sub-router. All routes share `state` via `.with_state()`.
    // Axum resolves routes in registration order for the same path+method pair,
    // but here each path+method combination is unique.
    let api = Router::new()
        // SSE stream — clients subscribe here to receive real-time board events.
        .route("/events", get(events::sse_handler))
        .route("/boards", get(routes::boards::list_boards))
        .route("/boards", post(routes::boards::create_board))
        .route("/boards/:id", get(routes::boards::get_board))
        .route("/boards/:id", put(routes::boards::update_board))
        .route("/boards/:id", delete(routes::boards::delete_board))
        .route("/boards/:id/columns", get(routes::columns::list_columns))
        .route("/boards/:id/columns", post(routes::columns::create_column))
        // Bulk reorder: PUT replaces the entire column order in one round-trip.
        .route(
            "/boards/:id/columns/reorder",
            put(routes::columns::reorder_columns),
        )
        .route("/columns/:id", put(routes::columns::update_column))
        .route("/columns/:id", delete(routes::columns::delete_column))
        .route("/columns/:id/cards", get(routes::cards::list_cards))
        .route("/columns/:id/cards", post(routes::cards::create_card))
        .route("/cards/:id", get(routes::cards::get_card))
        .route("/cards/:id", put(routes::cards::update_card))
        .route("/cards/:id", delete(routes::cards::delete_card))
        .route("/cards/:id/move", post(routes::cards::move_card))
        .with_state(state);

    // `STATIC_DIR` lets the Docker image override where the compiled WASM frontend
    // lives without rebuilding. Falls back to `./dist` for local development.
    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "./dist".to_string());

    Router::new()
        .route("/health", get(health))
        // `/api/info` is intentionally public — the frontend fetches it
        // unauthenticated on every page load to populate the version watermark.
        // It must stay outside any auth-gated sub-router.
        .route("/api/info", get(info))
        // `.nest("/api", api)` mounts the api sub-router under `/api`, so
        // `/api/boards` maps to the `list_boards` handler above.
        .nest("/api", api)
        // `SpaSvc` serves static files from the dist directory and falls back to
        // index.html for any path that isn't a real file on disk, enabling
        // Leptos client-side routing to handle deep-links (e.g. /boards/123).
        .fallback_service(SpaSvc::new(&static_dir))
        // `TraceLayer` logs every request (method, path, status, latency) using
        // the `tracing` crate — visible as structured JSON in production.
        .layer(TraceLayer::new_for_http())
}

// `#[tokio::main]` is a macro that sets up the Tokio async runtime and runs
// this function as the entry point. Without it, `async fn main` wouldn't work
// because Rust's standard runtime is synchronous.
#[tokio::main]
async fn main() {
    // rustls needs a crypto provider installed before any TLS handshakes.
    // `ring` is the default provider — this call must happen before any
    // TLS config is created.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // Initialise structured logging / tracing (returns a guard that flushes on drop).
    let _obs = observability::init();

    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "/data/bored.db".to_string());
    let db = db::connect_persistent(&db_path)
        .await
        .expect("failed to connect to database");

    let state = AppState::new(db);

    // Check for TLS certificate/key paths in environment variables.
    // If both are present, serve HTTPS on port 443.
    // If either is missing, fall back to plain HTTP on port 3000 (dev mode).
    let cert = std::env::var("TLS_CERT");
    let key = std::env::var("TLS_KEY");

    match (cert, key) {
        (Ok(cert), Ok(key)) => {
            let config = RustlsConfig::from_pem_file(&cert, &key)
                .await
                .expect("failed to load TLS config");
            // `[0, 0, 0, 0]` means bind to all network interfaces (0.0.0.0).
            let addr = SocketAddr::from(([0, 0, 0, 0], 443));
            tracing::info!(%addr, "bored backend listening (TLS)");
            axum_server::bind_rustls(addr, config)
                .serve(app(state).await.into_make_service())
                .await
                .unwrap();
        }
        _ => {
            let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
            tracing::info!(%addr, "bored backend listening (plain HTTP)");
            // `tokio::net::TcpListener` is the async equivalent of the standard
            // library's `TcpListener` — it doesn't block the thread while waiting.
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app(state).await).await.unwrap();
        }
    }
}

async fn health() -> &'static str {
    "ok"
}

// Returns runtime version and environment — read from env vars injected by the deploy pipeline.
// Falls back to the compile-time crate version and "dev" when running locally.
async fn info() -> axum::Json<shared::AppInfo> {
    axum::Json(shared::AppInfo {
        version: std::env::var("APP_VERSION")
            .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string()),
        env: std::env::var("APP_ENV").unwrap_or_else(|_| "dev".to_string()),
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
    // `axum_test::TestServer` wraps the router and lets us make HTTP requests
    // in tests without opening a real TCP socket.
    use axum_test::TestServer;

    // Helper: create a TestServer backed by an in-memory database.
    // Called at the start of each test that needs a server.
    async fn test_app() -> TestServer {
        let db = db::connect_mem().await.expect("failed to connect mem db");
        let state = AppState::new(db);
        let router = app(state).await;
        TestServer::new(router).unwrap()
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
    async fn create_board_seeds_default_columns() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Seeded".to_string(),
            })
            .await;
        create_resp.assert_status(StatusCode::CREATED);
        let board: shared::Board = create_resp.json();

        let list_resp = server
            .get(&format!("/api/boards/{}/columns", board.id))
            .await;
        list_resp.assert_status_ok();
        let columns: Vec<shared::Column> = list_resp.json();
        // `.iter().map(|c| c.name.as_str()).collect()` builds a Vec<&str> from the
        // column names so we can compare against a string slice literal.
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Todo", "Done"]);
        assert_eq!(columns[0].position, 0);
        assert_eq!(columns[1].position, 1);
    }

    #[tokio::test]
    async fn create_board_and_list() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Test Board".to_string(),
            })
            .await;
        create_resp.assert_status(StatusCode::CREATED);
        let board: shared::Board = create_resp.json();
        assert_eq!(board.name, "Test Board");

        let list_resp = server.get("/api/boards").await;
        list_resp.assert_status_ok();
        let boards: Vec<shared::Board> = list_resp.json();
        // `.any(...)` returns true if at least one element satisfies the predicate.
        assert!(boards.iter().any(|b| b.id == board.id));
    }

    #[tokio::test]
    async fn get_board_by_id() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Get Me".to_string(),
            })
            .await;
        let board: shared::Board = create_resp.json();

        let get_resp = server.get(&format!("/api/boards/{}", board.id)).await;
        get_resp.assert_status_ok();
        let fetched: shared::Board = get_resp.json();
        assert_eq!(fetched.id, board.id);
        assert_eq!(fetched.name, "Get Me");
    }

    #[tokio::test]
    async fn update_board_name() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Old Name".to_string(),
            })
            .await;
        let board: shared::Board = create_resp.json();

        let update_resp = server
            .put(&format!("/api/boards/{}", board.id))
            .json(&shared::UpdateBoardRequest {
                name: "New Name".to_string(),
            })
            .await;
        update_resp.assert_status_ok();
        let updated: shared::Board = update_resp.json();
        assert_eq!(updated.name, "New Name");

        let get_resp = server.get(&format!("/api/boards/{}", board.id)).await;
        let fetched: shared::Board = get_resp.json();
        assert_eq!(fetched.name, "New Name");
    }

    #[tokio::test]
    async fn delete_board_returns_404_on_get() {
        let server = test_app().await;

        let create_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Delete Me".to_string(),
            })
            .await;
        let board: shared::Board = create_resp.json();

        let del_resp = server.delete(&format!("/api/boards/{}", board.id)).await;
        del_resp.assert_status(StatusCode::NO_CONTENT);

        let get_resp = server.get(&format!("/api/boards/{}", board.id)).await;
        get_resp.assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_column_and_list() {
        let server = test_app().await;

        let create_board_resp = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Board With Columns".to_string(),
            })
            .await;
        let board: shared::Board = create_board_resp.json();

        let create_col_resp = server
            .post(&format!("/api/boards/{}/columns", board.id))
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
            .get(&format!("/api/boards/{}/columns", board.id))
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
                name: "Board For Cascade".to_string(),
            })
            .await;
        let board: shared::Board = create_board_resp.json();

        let create_col_resp = server
            .post(&format!("/api/boards/{}/columns", board.id))
            .json(&shared::CreateColumnRequest {
                name: "Col 1".to_string(),
                position: 0,
            })
            .await;
        let column: shared::Column = create_col_resp.json();

        server
            .delete(&format!("/api/boards/{}", board.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Verify the column no longer exists by trying to update it.
        let update_resp = server
            .put(&format!("/api/columns/{}", column.id))
            .json(&shared::UpdateColumnRequest {
                name: Some("Updated".to_string()),
                position: None,
            })
            .await;
        update_resp.assert_status(StatusCode::NOT_FOUND);
    }

    // Shared helper used by several card tests. Creates a board (with its default
    // Todo/Done columns) and then adds a fresh column named "Col".
    async fn setup_board_and_column(server: &TestServer) -> (shared::Board, shared::Column) {
        let board: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Test Board".to_string(),
            })
            .await
            .json();
        let column: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.id))
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
            })
            .await
            .json();

        let update_resp = server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                body: Some("# New body\n\nWith details".to_string()),
                position: None,
                column_id: None,
            })
            .await;
        update_resp.assert_status_ok();
        let updated: shared::Card = update_resp.json();
        assert_eq!(updated.body, "# New body\n\nWith details");
    }

    #[tokio::test]
    async fn move_card_between_columns() {
        let server = test_app().await;
        let (board, col_a) = setup_board_and_column(&server).await;

        let col_b: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.id))
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
                position: None,
                column_id: None,
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
            })
            .await
            .json();

        server
            .delete(&format!("/api/boards/{}", board.id))
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
                position: None,
                column_id: None,
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
            })
            .await
            .json();
        let c2: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "Card 2".to_string(),
            })
            .await
            .json();
        let c3: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "Card 3".to_string(),
            })
            .await
            .json();

        // Deliberately set positions out of insertion order to verify sorting.
        server
            .put(&format!("/api/cards/{}", c1.id))
            .json(&shared::UpdateCardRequest {
                body: None,
                position: Some(2),
                column_id: None,
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", c2.id))
            .json(&shared::UpdateCardRequest {
                body: None,
                position: Some(0),
                column_id: None,
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", c3.id))
            .json(&shared::UpdateCardRequest {
                body: None,
                position: Some(1),
                column_id: None,
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
                position: None,
                column_id: None,
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
    async fn create_card_with_empty_body_returns_400() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;
        server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "   ".to_string(),
            })
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn card_response_has_body_not_title_or_description() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: "# My Card\n\nSome content".to_string(),
            })
            .await
            .json();

        // Verify the full body is preserved verbatim.
        assert_eq!(card.body, "# My Card\n\nSome content");
    }

    #[tokio::test]
    #[serial]
    async fn info_route_returns_version_and_env() {
        let server = test_app().await;
        let resp = server.get("/api/info").await;
        resp.assert_status_ok();
        let info: shared::AppInfo = resp.json();
        // Falls back to compile-time CARGO_PKG_VERSION when APP_VERSION is unset.
        assert!(!info.version.is_empty());
        // Falls back to "dev" when APP_ENV is unset.
        assert_eq!(info.env, "dev");
    }

    #[tokio::test]
    #[serial]
    async fn info_route_uses_env_vars_when_set() {
        std::env::set_var("APP_VERSION", "1.2.3");
        std::env::set_var("APP_ENV", "production");
        let server = test_app().await;
        let resp = server.get("/api/info").await;
        resp.assert_status_ok();
        let body = resp.text();
        std::env::remove_var("APP_VERSION");
        std::env::remove_var("APP_ENV");
        let info: shared::AppInfo = serde_json::from_str(&body).expect("valid AppInfo JSON");
        assert_eq!(info.version, "1.2.3");
        assert_eq!(info.env, "production");
    }

    // Verifies that a deep-link path (e.g. /boards/abc) returns 200 with index.html
    // rather than 404 when the SPA fallback is active.
    #[tokio::test]
    #[serial]
    async fn spa_deep_link_returns_index_html() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<html></html>").unwrap();
        std::env::set_var("STATIC_DIR", dir.path().to_str().unwrap());
        let server = test_app().await;
        let resp = server.get("/boards/some-deep-link").await;
        std::env::remove_var("STATIC_DIR"); // remove before assert so cleanup runs even on failure
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
                name: "Reorder Board".to_string(),
            })
            .await
            .json();

        // Add a third column alongside the two default ones (Todo, Done).
        let col_c: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.id))
            .json(&shared::CreateColumnRequest {
                name: "In Progress".to_string(),
                position: 2,
            })
            .await
            .json();

        let cols: Vec<shared::Column> = server
            .get(&format!("/api/boards/{}/columns", board.id))
            .await
            .json();

        // Capture the auto-seeded column IDs.
        let col_todo = cols.iter().find(|c| c.name == "Todo").unwrap().id.clone();
        let col_done = cols.iter().find(|c| c.name == "Done").unwrap().id.clone();
        let col_ip = col_c.id.clone();

        // Reorder to: In Progress, Todo, Done.
        let reorder_resp = server
            .put(&format!("/api/boards/{}/columns/reorder", board.id))
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
                name: "Board A".to_string(),
            })
            .await
            .json();

        let cols_a: Vec<shared::Column> = server
            .get(&format!("/api/boards/{}/columns", board_a.id))
            .await
            .json();
        let col_a_todo = cols_a.iter().find(|c| c.name == "Todo").unwrap();
        let original_position = col_a_todo.position;

        // Board B — the attacker's board. Submit board A's column ID in the order.
        let board_b: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Board B".to_string(),
            })
            .await
            .json();

        let cols_b: Vec<shared::Column> = server
            .get(&format!("/api/boards/{}/columns", board_b.id))
            .await
            .json();
        let col_b_todo = cols_b.iter().find(|c| c.name == "Todo").unwrap();
        let col_b_done = cols_b.iter().find(|c| c.name == "Done").unwrap();

        // Inject board A's column into board B's reorder request.
        // The WHERE board = … clause should make this a no-op for col_a_todo.
        let resp = server
            .put(&format!("/api/boards/{}/columns/reorder", board_b.id))
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
            .get(&format!("/api/boards/{}/columns", board_a.id))
            .await
            .json();
        let col_a_todo_after = cols_a_after.iter().find(|c| c.name == "Todo").unwrap();
        assert_eq!(
            col_a_todo_after.position, original_position,
            "foreign column position must be unchanged after cross-board reorder"
        );
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

        let server = TestServer::new(app(state).await).unwrap();

        // CREATE board → BoardCreated
        let board: shared::Board = server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "Event Board".to_string(),
            })
            .await
            .json();

        // Use a bounded async wait instead of try_recv so the test doesn't race
        // the handler. The send always happens before the HTTP response returns,
        // but relying on try_recv returning Ok rather than Empty is fragile under
        // a busy executor. 1 s is generous — in practice the channel is ready
        // in microseconds.
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("BoardCreated event timed out")
            .expect("broadcast channel closed");
        assert!(matches!(
            event.event,
            events::BoardEvent::BoardCreated { .. }
        ));

        // CREATE column → ColumnCreated
        let col: shared::Column = server
            .post(&format!("/api/boards/{}/columns", board.id))
            .json(&shared::CreateColumnRequest {
                name: "Col".to_string(),
                position: 2,
            })
            .await
            .json();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("ColumnCreated event timed out")
            .expect("broadcast channel closed");
        assert!(matches!(
            event.event,
            events::BoardEvent::ColumnCreated { .. }
        ));

        // CREATE card → CardCreated
        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", col.id))
            .json(&shared::CreateCardRequest {
                body: "hello".to_string(),
            })
            .await
            .json();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("CardCreated event timed out")
            .expect("broadcast channel closed");
        assert!(matches!(
            event.event,
            events::BoardEvent::CardCreated { .. }
        ));

        // MOVE card → CardMoved
        let cols: Vec<shared::Column> = server
            .get(&format!("/api/boards/{}/columns", board.id))
            .await
            .json();
        let other_col = cols.iter().find(|c| c.id != col.id).unwrap();

        server
            .post(&format!("/api/cards/{}/move", card.id))
            .json(&shared::MoveCardRequest {
                column_id: other_col.id.clone(),
                position: 0,
            })
            .await
            .assert_status_ok();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("CardMoved event timed out")
            .expect("broadcast channel closed");
        assert!(matches!(event.event, events::BoardEvent::CardMoved { .. }));
    }
}
