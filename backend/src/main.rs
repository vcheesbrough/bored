// Declare submodules — Rust looks for each in a file named `src/<name>.rs`.
// These are private by default; the route handlers are reached via `routes::boards::...`.
mod db;
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

// `app` is extracted from `main` so integration tests can call it directly
// without spinning up a real TCP listener. Tests construct `AppState` with an
// in-memory DB, call `app(state).await`, and pass the router to `TestServer`.
pub async fn app(state: AppState) -> Router {
    // Build the `/api/*` sub-router. All routes share `state` via `.with_state()`.
    // Axum resolves routes in registration order for the same path+method pair,
    // but here each path+method combination is unique.
    let api = Router::new()
        .route("/boards", get(routes::boards::list_boards))
        .route("/boards", post(routes::boards::create_board))
        .route("/boards/:id", get(routes::boards::get_board))
        .route("/boards/:id", put(routes::boards::update_board))
        .route("/boards/:id", delete(routes::boards::delete_board))
        .route("/boards/:id/columns", get(routes::columns::list_columns))
        .route("/boards/:id/columns", post(routes::columns::create_column))
        .route("/columns/:id", put(routes::columns::update_column))
        .route("/columns/:id", delete(routes::columns::delete_column))
        .route("/columns/:id/cards", get(routes::cards::list_cards))
        .route("/columns/:id/cards", post(routes::cards::create_card))
        .route("/cards/:id", put(routes::cards::update_card))
        .route("/cards/:id", delete(routes::cards::delete_card))
        .route("/cards/:id/move", post(routes::cards::move_card))
        .with_state(state);

    // `STATIC_DIR` lets the Docker image override where the compiled WASM frontend
    // lives without rebuilding. Falls back to `./dist` for local development.
    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "./dist".to_string());

    Router::new()
        .route("/health", get(health))
        .route("/api/info", get(info))
        // `.nest("/api", api)` mounts the api sub-router under `/api`, so
        // `/api/boards` maps to the `list_boards` handler above.
        .nest("/api", api)
        // `ServeDir` serves static files (the Leptos WASM bundle). Any request
        // that doesn't match `/health` or `/api/*` falls through to here.
        // This makes the SPA's `index.html` serve for all unknown paths, enabling
        // client-side routing.
        .fallback_service(ServeDir::new(static_dir))
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

    let state = AppState { db };

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
    // `axum_test::TestServer` wraps the router and lets us make HTTP requests
    // in tests without opening a real TCP socket.
    use axum_test::TestServer;

    // Helper: create a TestServer backed by an in-memory database.
    // Called at the start of each test that needs a server.
    async fn test_app() -> TestServer {
        let db = db::connect_mem().await.expect("failed to connect mem db");
        let state = AppState { db };
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
}
