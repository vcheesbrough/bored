mod db;
mod models;
mod observability;
mod routes;

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use routes::boards::AppState;
use std::net::SocketAddr;
use tower_http::{services::ServeDir, trace::TraceLayer};

pub async fn app(state: AppState) -> Router {
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

    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "./dist".to_string());

    Router::new()
        .route("/health", get(health))
        .nest("/api", api)
        .fallback_service(ServeDir::new(static_dir))
        .layer(TraceLayer::new_for_http())
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let _obs = observability::init();

    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "/data/bored.db".to_string());
    let db = db::connect_persistent(&db_path)
        .await
        .expect("failed to connect to database");

    let state = AppState { db };

    let cert = std::env::var("TLS_CERT");
    let key = std::env::var("TLS_KEY");

    match (cert, key) {
        (Ok(cert), Ok(key)) => {
            let config = RustlsConfig::from_pem_file(&cert, &key)
                .await
                .expect("failed to load TLS config");
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
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, app(state).await).await.unwrap();
        }
    }
}

async fn health() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum_test::TestServer;

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

        // Delete the board
        server
            .delete(&format!("/api/boards/{}", board.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Column should be gone (404 or no longer in list)
        // We verify via direct column update returning 404
        let update_resp = server
            .put(&format!("/api/columns/{}", column.id))
            .json(&shared::UpdateColumnRequest {
                name: Some("Updated".to_string()),
                position: None,
            })
            .await;
        update_resp.assert_status(StatusCode::NOT_FOUND);
    }

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
        let (_, column) = setup_board_and_column(&server).await;

        let create_resp = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                title: "Fix bug".to_string(),
                description: Some("Details here".to_string()),
            })
            .await;
        create_resp.assert_status(StatusCode::CREATED);
        let card: shared::Card = create_resp.json();
        assert_eq!(card.title, "Fix bug");
        assert_eq!(card.description, Some("Details here".to_string()));
        assert_eq!(card.column_id, column.id);

        let list_resp = server
            .get(&format!("/api/columns/{}/cards", column.id))
            .await;
        list_resp.assert_status_ok();
        let cards: Vec<shared::Card> = list_resp.json();
        assert!(cards.iter().any(|c| c.id == card.id));
    }

    #[tokio::test]
    async fn update_card_title_and_description() {
        let server = test_app().await;
        let (_, column) = setup_board_and_column(&server).await;

        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                title: "Old Title".to_string(),
                description: None,
            })
            .await
            .json();

        let update_resp = server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                title: Some("New Title".to_string()),
                description: Some(Some("Added description".to_string())),
                position: None,
                column_id: None,
            })
            .await;
        update_resp.assert_status_ok();
        let updated: shared::Card = update_resp.json();
        assert_eq!(updated.title, "New Title");
        assert_eq!(updated.description, Some("Added description".to_string()));
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
                title: "Movable".to_string(),
                description: None,
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

        let cards_a: Vec<shared::Card> = server
            .get(&format!("/api/columns/{}/cards", col_a.id))
            .await
            .json();
        assert!(!cards_a.iter().any(|c| c.id == card.id));

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
                title: "To Delete".to_string(),
                description: None,
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
                title: "Orphan".to_string(),
                description: None,
            })
            .await
            .json();

        server
            .delete(&format!("/api/columns/{}", column.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let resp = server
            .put(&format!("/api/cards/{}", card.id))
            .json(&shared::UpdateCardRequest {
                title: Some("x".to_string()),
                description: None,
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
                title: "Deep Orphan".to_string(),
                description: None,
            })
            .await
            .json();

        server
            .delete(&format!("/api/boards/{}", board.id))
            .await
            .assert_status(StatusCode::NO_CONTENT);

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
                title: Some("x".to_string()),
                description: None,
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
                title: "Card 1".to_string(),
                description: None,
            })
            .await
            .json();
        let c2: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                title: "Card 2".to_string(),
                description: None,
            })
            .await
            .json();
        let c3: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                title: "Card 3".to_string(),
                description: None,
            })
            .await
            .json();

        // Set positions out of natural insertion order
        server
            .put(&format!("/api/cards/{}", c1.id))
            .json(&shared::UpdateCardRequest {
                title: None,
                description: None,
                position: Some(2),
                column_id: None,
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", c2.id))
            .json(&shared::UpdateCardRequest {
                title: None,
                description: None,
                position: Some(0),
                column_id: None,
            })
            .await
            .assert_status_ok();
        server
            .put(&format!("/api/cards/{}", c3.id))
            .json(&shared::UpdateCardRequest {
                title: None,
                description: None,
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
        assert_eq!(cards[0].title, "Card 2"); // position 0
        assert_eq!(cards[1].title, "Card 3"); // position 1
        assert_eq!(cards[2].title, "Card 1"); // position 2
    }
}
