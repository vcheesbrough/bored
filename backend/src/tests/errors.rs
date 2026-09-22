//! What a failed request turns into: the status the caller sees, and — for an
//! internal failure — the log line that says why.
//!
//! These tests feed `ApiError` *real* SurrealDB errors, provoked against a live
//! in-memory database, rather than hand-built fixtures. The classifier matches
//! on the driver's error text, so a fixture would only prove that the test
//! agrees with itself; a real violation proves the substring is still there
//! after a driver upgrade.

use std::sync::{Arc, Mutex};

use axum::body::to_bytes;
use axum::response::IntoResponse;
use surrealdb::{Surreal, engine::local::Db};

use super::*;
use crate::error::ApiError;

/// Insert a board row directly, bypassing the routes. Returns whatever the
/// driver said, so the caller can use it for either the happy or the failing
/// write.
async fn create_board_row(db: &Surreal<Db>, id: &str, name: &str) -> surrealdb::Result<()> {
    db.query("CREATE type::thing('boards', $id) SET name = $name")
        .bind(("id", id.to_string()))
        .bind(("name", name.to_string()))
        .await?
        // Constraint violations arrive as a per-statement error inside an
        // otherwise successful response; `check()` is what promotes them.
        .check()?;
    Ok(())
}

/// As above for a link between two card ids. The ids need not exist — the
/// `record<cards>` field type constrains the table, not the row.
async fn create_link_row(
    db: &Surreal<Db>,
    id: &str,
    predecessor: &str,
    successor: &str,
) -> surrealdb::Result<()> {
    db.query(
        "CREATE type::thing('card_links', $id) SET \
         predecessor = type::thing('cards', $pred), \
         successor = type::thing('cards', $succ)",
    )
    .bind(("id", id.to_string()))
    .bind(("pred", predecessor.to_string()))
    .bind(("succ", successor.to_string()))
    .await?
    .check()?;
    Ok(())
}

/// Read a response body as a string. Every error body in this crate is tiny,
/// so the "limit" on `to_bytes` is nominal.
async fn body_text(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("error bodies are always fully buffered");
    String::from_utf8(bytes.to_vec()).expect("error bodies are UTF-8")
}

/// A log destination that keeps everything in memory.
///
/// `tracing` writes through a `MakeWriter`, which hands out a fresh writer per
/// event; this one hands out clones of the same `Arc`, so every event lands in
/// the one buffer the test reads afterwards.
#[derive(Clone)]
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

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
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufferWriter(Arc::clone(&buffer)))
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
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufferWriter(Arc::clone(&buffer)))
        .with_ansi(false)
        .finish();

    let guard = tracing::subscriber::set_default(subscriber);
    let value = f.await;
    drop(guard);

    let logs = String::from_utf8(buffer.lock().expect("log buffer poisoned").clone())
        .expect("log output is UTF-8");
    (value, logs)
}

#[tokio::test]
async fn a_duplicate_board_name_is_a_bodiless_conflict() {
    let db = db::connect_mem().await.expect("mem db");
    create_board_row(&db, "b1", "same-name")
        .await
        .expect("first board is accepted");

    let error = create_board_row(&db, "b2", "same-name")
        .await
        .expect_err("board_name_unique rejects the second board");

    let api_error = ApiError::from(error);
    assert!(
        matches!(api_error, ApiError::Conflict(None)),
        "a unique board name must stay a bodiless 409, not become a 500"
    );

    let response = api_error.into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(body_text(response).await, "");
}

#[tokio::test]
async fn a_duplicate_link_pair_is_a_conflict_that_says_why() {
    let db = db::connect_mem().await.expect("mem db");
    create_link_row(&db, "l1", "card-a", "card-b")
        .await
        .expect("first link is accepted");

    let error = create_link_row(&db, "l2", "card-a", "card-b")
        .await
        .expect_err("card_links_pair rejects the duplicate");

    let response = ApiError::from(error).into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    // The same body `routes::links` returns when its own duplicate check wins
    // the race against the index.
    assert_eq!(body_text(response).await, "these cards are already linked");
}

#[tokio::test]
async fn any_other_database_error_is_a_logged_500() {
    let db = db::connect_mem().await.expect("mem db");

    let error = db
        .query("SELEKT nonsense FROM")
        .await
        .expect_err("a malformed query fails to parse");
    // What the old `map_err(|_| …)` threw away, and what the log must carry.
    let cause = error.to_string();

    let api_error = ApiError::from(error);
    assert!(
        matches!(api_error, ApiError::Internal(_)),
        "an ordinary database failure is ours, not the caller's"
    );

    let (response, logs) = capture_logs(|| api_error.into_response());

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_text(response).await,
        "",
        "the caller is told nothing beyond the status"
    );

    assert!(
        logs.contains("ERROR"),
        "an internal error must be logged at ERROR level, got: {logs}"
    );
    let first_line = cause.lines().next().expect("the cause is not empty");
    assert!(
        logs.contains(first_line),
        "the log line must carry the cause {first_line:?}, got: {logs}"
    );
}

#[tokio::test]
async fn a_client_error_is_not_logged() {
    // The counterpart to the test above: a 404 is the caller's business, and
    // logging one per bad URL would bury the failures that matter.
    let (response, logs) = capture_logs(|| ApiError::NotFound.into_response());

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(logs.is_empty(), "a 404 must not write a log line: {logs}");
}

/// The one end-to-end route whose answer the central classifier changes.
///
/// Restoring a deleted board re-creates it under its old name. If another board
/// has taken that name since, the unique index rejects the write — which the
/// restore handler used to report as a 500, because only the two board CRUD
/// handlers classified their own database errors. It now gets the 409 the
/// handler already returns for the neighbouring "that board still exists" case.
#[tokio::test]
async fn restoring_a_board_whose_name_was_taken_is_a_conflict() {
    let db = db::connect_mem().await.expect("mem db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();

    let board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "taken-name".to_string(),
        })
        .await
        .json();
    server
        .delete(&format!("/api/boards/{}", board.name))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // A different board takes the freed name.
    server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "taken-name".to_string(),
        })
        .await
        .assert_status(StatusCode::CREATED);

    // The delete row is no longer reachable through `/history` — that slug now
    // resolves to the replacement board — so read it out of the log directly.
    let rows: Vec<models::DbAuditLog> = db
        .query("SELECT * FROM audit_log WHERE entity_type = 'board' AND action = 'delete'")
        .await
        .expect("audit log is readable")
        .take(0)
        .expect("audit rows deserialize");
    let delete_row = rows
        .iter()
        .find(|row| row.entity_id == board.id)
        .expect("the first board's delete row");

    server
        .post(&format!("/api/audit/{}/restore", delete_row.id.id.to_raw()))
        .await
        .assert_status(StatusCode::CONFLICT);
}

/// The log line an operator actually reads must say *which* request failed.
///
/// This drives a genuine 500 through the real router, with a subscriber
/// filtered at INFO — the level deployments run at — so it fails if the
/// tower-http span is ever made below that level again. A span opened below
/// the filter is never created, and the error event inside it would then carry
/// no method or path at all.
#[tokio::test]
async fn a_failing_request_logs_its_method_and_path() {
    let db = db::connect_mem().await.expect("mem db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (_board, column) = setup_board_and_column(&server).await;
    server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "a card".to_string(),
            ..Default::default()
        })
        .await
        .assert_status(StatusCode::CREATED);

    // Make the stored row impossible to read back: drop the field definition
    // that guarantees `body`, then clear the value. Listing the column's cards
    // now fails inside the driver, which is as close to a production database
    // fault as a test can get.
    db.query("REMOVE FIELD body ON TABLE cards")
        .await
        .expect("field removed")
        .check()
        .expect("no statement error");
    db.query("UPDATE cards SET body = NONE")
        .await
        .expect("body cleared")
        .check()
        .expect("no statement error");

    let path = format!("/api/columns/{}/cards", column.id);
    // `TestRequest` is `IntoFuture`, not `Future`, so it is awaited inside an
    // async block rather than handed over directly.
    let (response, logs) = capture_logs_async(async { server.get(&path).await }).await;

    response.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        logs.contains("request failed"),
        "the failure must be logged, got: {logs}"
    );
    assert!(
        logs.contains(&path),
        "the log line must name the request path {path}, got: {logs}"
    );
    assert!(
        logs.contains("GET"),
        "the log line must name the method, got: {logs}"
    );
}

/// User text must not be able to impersonate an index violation.
///
/// SurrealDB quotes the offending value back in several of its messages, and
/// those values are things a card body, tag or link reason can hold. A bare
/// `contains("card_links_pair")` would read the value below as a duplicate
/// link and answer 409 — a status this crate deliberately does not log, so the
/// server fault would vanish. Matching the index name in its own position
/// keeps that unreachable.
#[tokio::test]
async fn user_text_cannot_impersonate_an_index_violation() {
    let db = db::connect_mem().await.expect("mem db");

    let error = db
        .query(
            "CREATE type::thing('cards', 'c1') SET \
             column = type::thing('columns', 'col-1'), \
             body = 'b', \
             position = 'card_links_pair'",
        )
        .await
        .expect("query dispatched")
        .check()
        .expect_err("position is an int, not a string");

    // The premise of the test: the index name really is in the message, as a
    // value. Without this the assertion below would pass for the wrong reason.
    assert!(
        error.to_string().contains("card_links_pair"),
        "expected the driver to quote the value back, got: {error}"
    );

    assert!(
        matches!(ApiError::from(error), ApiError::Internal(_)),
        "a value that merely spells an index name is still a server fault"
    );
}
