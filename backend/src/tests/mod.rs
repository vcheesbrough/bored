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
mod errors;
mod links;
mod outbound_http;
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
    let router = app(state, "./dist", DeploymentInfo::new("dev", None)).await;
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

// ── Log capture, shared by `errors` and `outbound_http` ─────────────────────

/// A log destination that keeps everything in memory.
///
/// `tracing` writes through a `MakeWriter`, which hands out a fresh writer per
/// event; this one hands out clones of the same `Arc`, so every event lands in
/// the one buffer the test reads afterwards.
#[derive(Clone)]
struct BufferWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `f` with a subscriber that writes into a buffer, and return what it
/// logged. `with_default` installs the subscriber for this thread only, so
/// tests running in parallel cannot capture each other's output.
fn capture_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufferWriter(std::sync::Arc::clone(&buffer)))
        .with_ansi(false)
        .finish();

    let value = tracing::subscriber::with_default(subscriber, f);

    let logs = String::from_utf8(buffer.lock().expect("log buffer poisoned").clone())
        .expect("log output is UTF-8");
    (value, logs)
}

/// As `capture_logs`, for work that has to be awaited — a whole request, say.
///
/// `set_default` returns a guard rather than taking a closure, so the
/// subscriber stays installed across `.await`. `#[tokio::test]` runs on a
/// current-thread runtime, so the request future stays on this thread and
/// inside this thread's subscriber.
///
/// The default `fmt()` filter is INFO, which is deliberate: it is the level
/// deployments run at, so anything this test sees is something production
/// would see too.
async fn capture_logs_async<T>(f: impl std::future::Future<Output = T>) -> (T, String) {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufferWriter(std::sync::Arc::clone(&buffer)))
        .with_ansi(false)
        .finish();

    let guard = tracing::subscriber::set_default(subscriber);
    let value = f.await;
    drop(guard);

    let logs = String::from_utf8(buffer.lock().expect("log buffer poisoned").clone())
        .expect("log output is UTF-8");
    (value, logs)
}
