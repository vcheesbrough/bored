//! HTTP-level tests for the whole router, one submodule per feature area.
//!
//! These live in-crate (rather than in `backend/tests/`) because they reach
//! private items such as `health()`, `db::connect_mem` and `models::DbAuditLog`.
//! Helpers used by more than one area are defined here; each submodule pulls
//! them in, together with the crate-root items, via `use super::*`.

mod audit_log;
mod boards;
mod cards;
mod columns;
mod links;
mod reorder;
mod router;
mod sse;
mod tags;

// `super::*` imports everything from the parent module (the crate root,
// `main.rs`). Child modules can see these imports too: a glob import picks up
// every name that is *accessible* from where it is written, and private items
// are accessible to all descendants of the module that declares them.
use super::*;
use axum::http::StatusCode;
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
