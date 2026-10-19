//! What a failed request turns into: the status the caller sees, and — for an
//! internal failure — the log line that says why.
//!
//! These tests feed `ApiError` *real* Postgres errors, provoked against a live
//! test database, rather than hand-built fixtures. The classifier matches on
//! the server's SQLSTATE code and constraint name, so a fixture would only
//! prove that the test agrees with itself; a real violation proves the fields
//! are still where the classifier reads them after a driver upgrade.

use axum::body::to_bytes;
use axum::response::IntoResponse;

use super::*;
use crate::db::Db;
use crate::error::ApiError;

/// Insert a board row directly, bypassing the routes. Returns whatever the
/// driver said, so the caller can use it for either the happy or the failing
/// write.
async fn create_board_row(db: &Db, id: &str, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO boards (id, name) VALUES ($1, $2)")
        .bind(id)
        .bind(name)
        .execute(db)
        .await?;
    Ok(())
}

/// A board with one column (`col-1`) holding cards `card-a` and `card-b`,
/// written directly — the parents the foreign keys require.
async fn seed_cards(db: &Db) {
    create_board_row(db, "b1", "board").await.expect("board");
    sqlx::query(
        "INSERT INTO columns (id, board_id, name, position) VALUES ('col-1', 'b1', 'c', 0)",
    )
    .execute(db)
    .await
    .expect("column");
    for id in ["card-a", "card-b"] {
        sqlx::query(
            "INSERT INTO cards (id, column_id, body, position) VALUES ($1, 'col-1', 'b', 1)",
        )
        .bind(id)
        .execute(db)
        .await
        .expect("card");
    }
}

/// As [`create_board_row`] for a link between two card ids.
async fn create_link_row(
    db: &Db,
    id: &str,
    predecessor: &str,
    successor: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO card_links (id, predecessor_id, successor_id) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(predecessor)
        .bind(successor)
        .execute(db)
        .await?;
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
    let db = db::connect_test().await.expect("test db");
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
    let db = db::connect_test().await.expect("test db");
    seed_cards(&db).await;
    create_link_row(&db, "l1", "card-a", "card-b")
        .await
        .expect("first link is accepted");

    let error = create_link_row(&db, "l2", "card-a", "card-b")
        .await
        .expect_err("card_links_pair rejects the duplicate");

    let response = ApiError::from(error).into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    // The same body `routes::links` returns when its own duplicate check wins
    // the race against the constraint.
    assert_eq!(body_text(response).await, "these cards are already linked");
}

#[tokio::test]
async fn any_other_database_error_is_a_logged_500() {
    let db = db::connect_test().await.expect("test db");

    // "SELEKT" stands in for anything a query text could hold. A syntax
    // error quotes the query near the fault, so it must not reach the log.
    let error = sqlx::query("SELEKT nonsense FROM")
        .execute(&db)
        .await
        .expect_err("a malformed query fails to parse");
    assert!(
        format!("{error:?}").contains("SELEKT"),
        "premise: the server quotes the query text back, got: {error:?}"
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
    // 42601 is `syntax_error`: the class an operator needs, from a fixed table.
    assert!(
        logs.contains("sqlstate=42601"),
        "the log line must carry the SQLSTATE code, got: {logs}"
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
    let db = db::connect_test().await.expect("test db");
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
    let rows: Vec<models::DbAuditLog> =
        sqlx::query_as("SELECT * FROM audit_log WHERE entity_type = 'board' AND action = 'delete'")
            .fetch_all(&db)
            .await
            .expect("audit log is readable");
    let delete_row = rows
        .iter()
        .find(|row| row.entity_id == board.id)
        .expect("the first board's delete row");

    server
        .post(&format!("/api/audit/{}/restore", delete_row.id))
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
    let db = db::connect_test().await.expect("test db");
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

    // Make the stored row impossible to read back: drop the constraint that
    // guarantees `body`, then clear the value. Listing the column's cards now
    // fails decoding a NULL into a `String`, which is as close to a production
    // database fault as a test can get.
    break_card_bodies(&db).await;

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

/// User text must not be able to impersonate a constraint violation.
///
/// The classifier reads the constraint *name* from a structured field, never
/// from the message — but the message and detail do quote values, and those
/// are things a card body, tag or link reason can hold. Here the duplicate
/// *value* spells `card_links_pair` under a constraint the API does not
/// translate; it must stay a 500, not become the 409 (a status this crate
/// deliberately does not log) that would make the server fault vanish.
#[tokio::test]
async fn user_text_cannot_impersonate_a_constraint_violation() {
    let db = db::connect_test().await.expect("test db");
    seed_cards(&db).await;
    sqlx::query(
        "CREATE UNIQUE INDEX test_body_unique ON cards (body) WHERE body = 'card_links_pair'",
    )
    .execute(&db)
    .await
    .expect("index created");
    let insert = |id: &'static str| {
        sqlx::query(
            "INSERT INTO cards (id, column_id, body, position) \
             VALUES ($1, 'col-1', 'card_links_pair', 1)",
        )
        .bind(id)
        .execute(&db)
    };
    insert("c1").await.expect("first card accepted");
    let error = insert("c2")
        .await
        .expect_err("the unique index rejects the second");

    // The premise of the test: the name really is in the error, as a value.
    // Without this the assertion below would pass for the wrong reason.
    assert!(
        format!("{error:?}").contains("card_links_pair"),
        "expected the driver to quote the value back, got: {error:?}"
    );

    assert!(
        matches!(ApiError::from(error), ApiError::Internal(_)),
        "a value that merely spells a constraint name is still a server fault"
    );
}

// ── What a 500's log line may say (card #366) ───────────────────────────────
//
// Each test below provokes a *real* driver error that quotes a sensitive
// value, and checks the emitted line from two sides:
//
// * the value is absent — the redaction works;
// * the SQLSTATE code, the identifiers and the `error.type` are present — the
//   redaction did not throw away what #348 added.
//
// The premise is asserted too (the raw driver error really does contain the
// value), so a driver upgrade that stopped quoting values would make these
// tests fail loudly rather than pass for the wrong reason.

/// A string no route, schema or identifier contains, so finding it anywhere
/// in a log line can only mean user content leaked into it.
const SENSITIVE: &str = "SECRET-card-body-7f3a";

/// Drop `cards.body`'s NOT NULL and clear every body, so any read of a card
/// fails to decode — a genuine 500 from inside the driver.
async fn break_card_bodies(db: &Db) {
    sqlx::query("ALTER TABLE cards ALTER COLUMN body DROP NOT NULL")
        .execute(db)
        .await
        .expect("constraint dropped");
    sqlx::query("UPDATE cards SET body = NULL")
        .execute(db)
        .await
        .expect("bodies cleared");
}

#[tokio::test]
async fn a_failed_check_keeps_constraint_and_table_but_not_the_row() {
    let db = db::connect_test().await.expect("test db");
    seed_cards(&db).await;

    // A CHECK violation's detail is "Failing row contains (…)" — every value
    // of the rejected row, the card body included.
    sqlx::query("ALTER TABLE cards ADD CONSTRAINT probe CHECK (body NOT LIKE 'SECRET%')")
        .execute(&db)
        .await
        .expect("constraint added");
    let error = sqlx::query(
        "INSERT INTO cards (id, column_id, body, position) VALUES ('c1', 'col-1', $1, 1)",
    )
    .bind(SENSITIVE)
    .execute(&db)
    .await
    .expect_err("the check rejects the value");
    assert!(
        format!("{error:?}").contains(SENSITIVE),
        "premise: the driver carries the row's values, got: {error:?}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("db_constraint"), "error.type missing: {logs}");
    // 23514 is `check_violation`.
    assert!(logs.contains("sqlstate=23514"), "code missing: {logs}");
    assert!(
        logs.contains("constraint=probe"),
        "constraint missing: {logs}"
    );
    assert!(logs.contains("table=cards"), "table missing: {logs}");
}

/// The driver's own side (`sqlx`): decoding a value into our types. A JSON
/// decode error quotes the value it choked on, so it is reduced to the
/// variant and the column name.
#[tokio::test]
async fn a_client_side_decode_error_keeps_only_variant_and_column() {
    let db = db::connect_test().await.expect("test db");

    // Decoded by column *name*, as every row type in `models.rs` is.
    #[derive(Debug, sqlx::FromRow)]
    struct Probe {
        #[allow(dead_code)] // only ever fails to decode
        probe: sqlx::types::Json<u32>,
    }

    // A string cannot become a `u32`; serde_json says so, quoting the string.
    let error = sqlx::query_as::<_, Probe>("SELECT to_jsonb($1::text) AS probe")
        .bind(SENSITIVE)
        .fetch_one(&db)
        .await
        .expect_err("a string is not a u32");
    assert!(
        format!("{error:?}").contains(SENSITIVE),
        "premise: the error carries the value, got: {error:?}"
    );
    assert!(
        format!("{error:?}").starts_with("ColumnDecode"),
        "premise: this is the decode arm, got: {error:?}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("db_client"), "error.type missing: {logs}");
    assert!(logs.contains("ColumnDecode"), "variant missing: {logs}");
    assert!(logs.contains("column=probe"), "column missing: {logs}");
}

#[tokio::test]
async fn a_duplicate_primary_key_names_the_constraint_not_the_key() {
    let db = db::connect_test().await.expect("test db");
    seed_cards(&db).await;
    // The duplicate *key* is quoted in the error's detail ("Key (id)=(…)
    // already exists"), so the id itself stands in for user content here.
    let create = || {
        sqlx::query(
            "INSERT INTO cards (id, column_id, body, position) VALUES ($1, 'col-1', 'b', 1)",
        )
        .bind(SENSITIVE)
        .execute(&db)
    };
    create().await.expect("first card accepted");
    let error = create().await.expect_err("the id is taken");
    assert!(
        format!("{error:?}").contains(SENSITIVE),
        "premise: the detail quotes the key, got: {error:?}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("db_constraint"), "error.type missing: {logs}");
    assert!(logs.contains("sqlstate=23505"), "code missing: {logs}");
    assert!(
        logs.contains("constraint=cards_pkey"),
        "constraint missing: {logs}"
    );
}

/// Turn a database error into the log line an operator would see.
fn logged_line_for(error: sqlx::Error) -> String {
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
async fn a_card_body_quoted_in_a_conversion_error_never_reaches_the_log() {
    let db = db::connect_test().await.expect("test db");

    // Text where an integer belongs: Postgres answers `invalid input syntax
    // for type integer: "<text>"`.
    let error = sqlx::query("SELECT $1::integer")
        .bind(SENSITIVE)
        .execute(&db)
        .await
        .expect_err("the text is not an integer");
    assert!(
        format!("{error:?}").contains(SENSITIVE),
        "premise: the server quotes the value back, got: {error:?}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    // What an operator still gets: the kind and the code (22P02 is
    // `invalid_text_representation`, a data error rather than a constraint).
    assert!(logs.contains("db_engine"), "error.type missing: {logs}");
    assert!(logs.contains("sqlstate=22P02"), "code missing: {logs}");
}

#[tokio::test]
async fn an_unmapped_unique_index_keeps_index_and_table_but_not_the_value() {
    let db = db::connect_test().await.expect("test db");
    seed_cards(&db).await;

    // A unique index `classify` does not translate into a 409, so a
    // violation of it is a 500 — the path whose log line must be redacted.
    sqlx::query("CREATE UNIQUE INDEX test_body_unique ON cards (body) WHERE body <> 'b'")
        .execute(&db)
        .await
        .expect("index created");
    let create = |id: &'static str| {
        sqlx::query("INSERT INTO cards (id, column_id, body, position) VALUES ($1, 'col-1', $2, 1)")
            .bind(id)
            .bind(SENSITIVE)
            .execute(&db)
    };
    create("c1").await.expect("first card accepted");
    let error = create("c2")
        .await
        .expect_err("the unique index rejects the duplicate body");
    assert!(
        format!("{error:?}").contains(SENSITIVE),
        "premise: the detail quotes the value back, got: {error:?}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("db_constraint"), "error.type missing: {logs}");
    assert!(
        logs.contains("constraint=test_body_unique"),
        "index missing: {logs}"
    );
    assert!(logs.contains("table=cards"), "table missing: {logs}");
}

/// A foreign key the API does not translate (here: a link to a card that does
/// not exist) is a 500 the log line can place: constraint and table, no
/// values.
#[tokio::test]
async fn a_foreign_key_violation_names_its_constraint() {
    let db = db::connect_test().await.expect("test db");
    seed_cards(&db).await;
    let error = sqlx::query(
        "INSERT INTO card_links (id, predecessor_id, successor_id) VALUES ('l1', 'card-a', $1)",
    )
    .bind(SENSITIVE)
    .execute(&db)
    .await
    .expect_err("there is no such card");
    assert!(
        format!("{error:?}").contains(SENSITIVE),
        "premise: the detail quotes the missing key, got: {error:?}"
    );

    let logs = logged_line_for(error);

    assert!(!logs.contains(SENSITIVE), "user content leaked: {logs}");
    assert!(logs.contains("db_constraint"), "error.type missing: {logs}");
    assert!(logs.contains("sqlstate=23503"), "code missing: {logs}");
    assert!(
        logs.contains("constraint=card_links_successor_id_fkey"),
        "constraint missing: {logs}"
    );
}

/// A card written into a column that is gone — a create racing the column's
/// delete — is the 404 the route gives a missing column, not a 500.
#[tokio::test]
async fn a_card_for_a_vanished_column_is_not_found() {
    let db = db::connect_test().await.expect("test db");
    let error = sqlx::query(
        "INSERT INTO cards (id, column_id, body, position) VALUES ('c1', 'gone', 'b', 1)",
    )
    .execute(&db)
    .await
    .expect_err("there is no such column");
    // At a card-write call site, the missing column is the caller's 404 …
    assert!(matches!(
        crate::error::card_column_write_error(error),
        ApiError::NotFound
    ));
    // … but the general classifier does not guess: the same constraint also
    // blocks a column delete that still has cards, which is no 404.
    let error = sqlx::query(
        "INSERT INTO cards (id, column_id, body, position) VALUES ('c2', 'gone', 'b', 1)",
    )
    .execute(&db)
    .await
    .expect_err("there is no such column");
    assert!(matches!(ApiError::from(error), ApiError::Internal(_)));
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
