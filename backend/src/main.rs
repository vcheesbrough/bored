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
}
