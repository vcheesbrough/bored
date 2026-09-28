//! What a failed request turns into: the status the caller sees, and — for an
//! internal failure — the log line that says why.
//!
//! These tests feed `ApiError` *real* SurrealDB errors, provoked against a live
//! in-memory database, rather than hand-built fixtures. The classifier matches
//! on the driver's error text, so a fixture would only prove that the test
//! agrees with itself; a real violation proves the substring is still there
//! after a driver upgrade.

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

    // "nonsense" stands in for anything a query text could hold. A parse
    // error quotes the query back, so it must not reach the log.
    let error = db
        .query("SELEKT nonsense FROM")
        .await
        .expect_err("a malformed query fails to parse");
    assert!(
        format!("{error:?}").contains("nonsense"),
        "premise: the driver carries the query text, got: {error:?}"
    );

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
    // What the old `map_err(|_| …)` threw away and #348 restored — *what kind*
    // of failure this was — survives the redaction of card #366 …
    assert!(
        logs.contains("error.type") && logs.contains("db_engine"),
        "the log line must carry a typed error.type, got: {logs}"
    );
    assert!(
        logs.contains("Db::InvalidQuery"),
        "the log line must name the driver's error variant, got: {logs}"
    );
    // … but the query text does not.
    assert!(
        !logs.contains("nonsense") && !logs.contains("SELEKT"),
        "query text leaked into the log: {logs}"
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
    // A query string the route ignores, standing in for the OAuth `code` that
    // `/auth/callback` receives: it must not be copied into the span every
    // log line of the request carries (card #366).
    let target = format!("{path}?code={SENSITIVE}");
    // `TestRequest` is `IntoFuture`, not `Future`, so it is awaited inside an
    // async block rather than handed over directly.
    let (response, logs) = capture_logs_async(async { server.get(&target).await }).await;

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
    assert!(
        !logs.contains(SENSITIVE),
        "the query string must not reach the log, got: {logs}"
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

// ── What a 500's log line may say (card #366) ───────────────────────────────
//
// Each test below provokes a *real* driver error whose message quotes a
// sensitive value, and checks the emitted line from two sides:
//
// * the value is absent — the redaction works;
// * the variant, the identifiers and the `error.type` are present — the
//   redaction did not throw away what #348 added.
//
// The premise is asserted too (the raw driver message really does contain the
// value), so a driver upgrade that stopped quoting values would make these
// tests fail loudly rather than pass for the wrong reason.

/// A string no route, schema or identifier contains, so finding it anywhere
/// in a log line can only mean user content leaked into it.
const SENSITIVE: &str = "SECRET-card-body-7f3a";

#[tokio::test]
async fn a_failed_field_assert_keeps_field_and_record_but_not_the_value() {
    let db = db::connect_mem().await.expect("mem db");

    // An ASSERT clause makes the driver raise `FieldValue` (rather than
    // `FieldCheck`, which is the type check): "Found '<value>' for field
    // `probe`, with record `cards:c1`, but field must conform to: …".
    db.query(
        "DEFINE FIELD probe ON cards TYPE option<string> ASSERT $value = NONE OR $value = 'ok'",
    )
    .await
    .expect("field defined")
    .check()
    .expect("no statement error");
    let error = db
        .query(
            "CREATE type::thing('cards', 'c1') SET \
             column = type::thing('columns', 'col-1'), \
             body = 'b', position = 1, probe = $text",
        )
        .bind(("text", SENSITIVE))
        .await
        .expect("dispatched")
        .check()
        .expect_err("the assert rejects the value");
    assert!(
        format!("{error:?}").starts_with("Db(FieldValue"),
        "premise: this is the FieldValue arm, got: {error:?}"
    );
    assert!(
        error.to_string().contains(SENSITIVE),
        "premise: the driver quotes the value back, got: {error}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("Db::FieldValue"), "variant missing: {logs}");
    assert!(logs.contains("field=probe"), "field missing: {logs}");
    assert!(
        logs.contains("record=cards:c1"),
        "record id missing: {logs}"
    );
}

/// The driver's client-layer arm (`surrealdb::Error::Api`). `LossyTake` —
/// asking for one row from a result holding several — carries the whole
/// response in the error, card bodies included, so it is reduced to its
/// variant path only.
#[tokio::test]
async fn a_client_layer_error_keeps_only_its_variant() {
    let db = db::connect_mem().await.expect("mem db");
    for id in ["c1", "c2"] {
        db.query(
            "CREATE type::thing('cards', $id) SET \
             column = type::thing('columns', 'col-1'), \
             body = $text, position = 1",
        )
        .bind(("id", id))
        .bind(("text", SENSITIVE))
        .await
        .expect("dispatched")
        .check()
        .expect("card accepted");
    }

    // `Option<_>` asks for at most one row; there are two.
    let error = db
        .query("SELECT * FROM cards")
        .await
        .expect("dispatched")
        .take::<Option<crate::models::DbCard>>(0)
        .expect_err("two rows cannot become one");
    assert!(
        format!("{error:?}").contains(SENSITIVE),
        "premise: the error carries the rows, got: {error:?}"
    );
    assert!(
        format!("{error:?}").starts_with("Api("),
        "premise: this is the client-layer arm, got: {error:?}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("db_client"), "error.type missing: {logs}");
    assert!(logs.contains("Api::LossyTake"), "variant missing: {logs}");
}

#[tokio::test]
async fn a_duplicate_record_id_names_the_record() {
    let db = db::connect_mem().await.expect("mem db");
    let create = || {
        db.query(
            "CREATE type::thing('cards', 'c1') SET \
             column = type::thing('columns', 'col-1'), \
             body = $text, position = 1",
        )
        .bind(("text", SENSITIVE))
    };
    create()
        .await
        .expect("dispatched")
        .check()
        .expect("first card accepted");
    let error = create()
        .await
        .expect("dispatched")
        .check()
        .expect_err("the id is taken");
    assert!(
        format!("{error:?}").starts_with("Db(RecordExists"),
        "premise: this is the RecordExists arm, got: {error:?}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("db_constraint"), "error.type missing: {logs}");
    assert!(logs.contains("Db::RecordExists"), "variant missing: {logs}");
    assert!(
        logs.contains("record=cards:c1"),
        "record id missing: {logs}"
    );
}

/// Turn a database error into the log line an operator would see.
fn logged_line_for(error: surrealdb::Error) -> String {
    let api_error = ApiError::from(error);
    assert!(
        matches!(api_error, ApiError::Internal(_)),
        "the test needs a 500, which is the only status that logs"
    );
    let (response, logs) = capture_logs(|| api_error.into_response());
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    logs
}

#[tokio::test]
async fn a_card_body_quoted_in_a_field_error_never_reaches_the_log() {
    let db = db::connect_mem().await.expect("mem db");

    // `position` is an int; putting user text there makes SurrealDB answer
    // "Found '<text>' for field `position`, with record `cards:c1`, …".
    let error = db
        .query(
            "CREATE type::thing('cards', 'c1') SET \
             column = type::thing('columns', 'col-1'), \
             body = 'b', \
             position = $text",
        )
        .bind(("text", SENSITIVE))
        .await
        .expect("query dispatched")
        .check()
        .expect_err("position is an int, not a string");
    assert!(
        error.to_string().contains(SENSITIVE),
        "premise: the driver quotes the value back, got: {error}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    // What an operator still gets: the kind, the variant, which field and
    // which record.
    assert!(logs.contains("db_constraint"), "error.type missing: {logs}");
    assert!(logs.contains("Db::FieldCheck"), "variant missing: {logs}");
    assert!(logs.contains("field=position"), "field missing: {logs}");
    assert!(
        logs.contains("record=cards:c1"),
        "record id missing: {logs}"
    );
}

#[tokio::test]
async fn an_unmapped_unique_index_keeps_index_and_record_but_not_the_value() {
    let db = db::connect_mem().await.expect("mem db");

    // A unique index `classify` does not translate into a 409, so a
    // violation of it is a 500 — the path whose log line must be redacted.
    db.query("DEFINE INDEX test_body_unique ON cards FIELDS body UNIQUE")
        .await
        .expect("index defined")
        .check()
        .expect("no statement error");
    let create = |id: &'static str| {
        db.query(
            "CREATE type::thing('cards', $id) SET \
             column = type::thing('columns', 'col-1'), \
             body = $text, \
             position = 1",
        )
        .bind(("id", id))
        .bind(("text", SENSITIVE))
    };
    create("c1")
        .await
        .expect("dispatched")
        .check()
        .expect("first card accepted");
    let error = create("c2")
        .await
        .expect("dispatched")
        .check()
        .expect_err("the unique index rejects the duplicate body");
    assert!(
        error.to_string().contains(SENSITIVE),
        "premise: the driver quotes the value back, got: {error}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("Db::IndexExists"), "variant missing: {logs}");
    assert!(
        logs.contains("index=test_body_unique"),
        "index missing: {logs}"
    );
    // SurrealDB names the record that already holds the value.
    assert!(
        logs.contains("record=cards:c1"),
        "record id missing: {logs}"
    );
}

#[tokio::test]
async fn a_serde_json_error_keeps_its_position_but_not_the_value() {
    // serde_json quotes the value it choked on: `invalid type: string "…"`.
    let error = serde_json::from_str::<u32>(&format!("\"{SENSITIVE}\""))
        .expect_err("a string is not a u32");
    assert!(
        error.to_string().contains(SENSITIVE),
        "premise: serde_json quotes the value back, got: {error}"
    );

    let (response, logs) = capture_logs(|| ApiError::from(error).into_response());

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("serialization"), "error.type missing: {logs}");
    assert!(logs.contains("line 1 column"), "position missing: {logs}");
}
