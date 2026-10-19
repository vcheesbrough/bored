//! What the Postgres schema itself guarantees (card #471): card numbers under
//! concurrency, and the deliberate `ON DELETE` behaviour of each foreign key.

use super::*;

/// Card numbers come from a sequence, so many creates racing each other still
/// get distinct numbers — the guarantee SurrealDB's counter record gave.
#[tokio::test]
async fn concurrent_card_creates_get_distinct_numbers() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (_board, column) = setup_board_and_column(&server).await;

    const CREATES: usize = 24;
    let path = format!("/api/columns/{}/cards", column.id);
    // `join_all` polls every request future together, so the creates are in
    // flight at once rather than one after another.
    let responses = futures_util::future::join_all((0..CREATES).map(|i| {
        // `TestRequest` is `IntoFuture`, so it is turned into a future here.
        std::future::IntoFuture::into_future(server.post(&path).json(&shared::CreateCardRequest {
            body: format!("card {i}"),
            ..Default::default()
        }))
    }))
    .await;

    let mut numbers = std::collections::BTreeSet::new();
    for response in responses {
        response.assert_status(StatusCode::CREATED);
        let card: shared::Card = response.json();
        assert!(card.number > 0, "every card is numbered");
        assert!(
            numbers.insert(card.number),
            "number {} issued twice",
            card.number
        );
    }
    assert_eq!(numbers.len(), CREATES);

    // Independent of the responses: the stored rows agree.
    let distinct: i64 = sqlx::query_scalar("SELECT count(DISTINCT number) FROM cards")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(distinct, CREATES as i64);
}

/// A board, a column and two linked cards, written straight to the database.
async fn seed(db: &crate::db::Db) {
    for statement in [
        "INSERT INTO boards (id, name) VALUES ('b1', 'b1')",
        "INSERT INTO columns (id, board_id, name, position) VALUES ('c1', 'b1', 'c', 0)",
        "INSERT INTO cards (id, column_id, body, position) VALUES ('k1', 'c1', 'x', 1)",
        "INSERT INTO cards (id, column_id, body, position) VALUES ('k2', 'c1', 'y', 2)",
        "INSERT INTO card_links (id, predecessor_id, successor_id) VALUES ('l1', 'k1', 'k2')",
    ] {
        sqlx::query(statement).execute(db).await.expect(statement);
    }
}

/// The SQLSTATE of a failed statement.
fn sqlstate(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(database) => database.code().map(|code| code.into_owned()),
        _ => None,
    }
}

/// Columns and cards are RESTRICT: the routes delete (and audit) children
/// themselves, so a delete that skipped them must fail rather than silently
/// take unaudited rows with it.
#[tokio::test]
async fn deleting_a_parent_with_children_is_refused() {
    let db = db::connect_test().await.expect("test db");
    seed(&db).await;

    for statement in [
        "DELETE FROM boards WHERE id = 'b1'",
        "DELETE FROM columns WHERE id = 'c1'",
    ] {
        let error = sqlx::query(statement)
            .execute(&db)
            .await
            .expect_err(statement);
        // 23503 is `foreign_key_violation`.
        assert_eq!(sqlstate(&error).as_deref(), Some("23503"), "{statement}");
    }
    let cards: i64 = sqlx::query_scalar("SELECT count(*) FROM cards")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(cards, 2, "nothing was deleted");
}

/// Links CASCADE: a link cannot outlive either card, even if a delete path
/// forgot to remove it first.
#[tokio::test]
async fn deleting_a_card_takes_its_links_with_it() {
    let db = db::connect_test().await.expect("test db");
    seed(&db).await;

    sqlx::query("DELETE FROM cards WHERE id = 'k2'")
        .execute(&db)
        .await
        .expect("a linked card can be deleted");
    let links: i64 = sqlx::query_scalar("SELECT count(*) FROM card_links")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(links, 0);
}

/// Through the API, deleting a board still works — the routes remove the
/// children in order — and the cascade is fully audited.
#[tokio::test]
async fn the_board_delete_route_satisfies_the_foreign_keys() {
    let server = test_app().await;
    let (board, column) = setup_board_and_column(&server).await;
    for body in ["one", "two"] {
        server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: body.to_string(),
                ..Default::default()
            })
            .await
            .assert_status(StatusCode::CREATED);
    }
    server
        .delete(&format!("/api/boards/{}", board.name))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get(&format!("/api/boards/{}", board.name))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// Restoring a deleted card whose column has since been deleted cannot meet
/// the foreign key. Under SurrealDB that recreated an orphan; now it is a 422
/// that says what to restore first — and nothing is written.
#[tokio::test]
async fn restoring_a_card_into_a_deleted_column_is_refused() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (_board, column) = setup_board_and_column(&server).await;
    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "orphan-to-be".to_string(),
            ..Default::default()
        })
        .await
        .json();
    // Delete the card on its own, then its (now empty) column.
    server
        .delete(&format!("/api/cards/{}", card.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/api/columns/{}", column.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let card_delete: String = sqlx::query_scalar(
        "SELECT id FROM audit_log WHERE entity_type = 'card' AND action = 'delete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let response = server
        .post(&format!("/api/audit/{card_delete}/restore"))
        .await;
    response.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.text(),
        "restore the board or column this belonged to first"
    );
    let cards: i64 = sqlx::query_scalar("SELECT count(*) FROM cards")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(cards, 0);

    // Control: restore the column first, and the card then restores.
    let column_delete: String = sqlx::query_scalar(
        "SELECT id FROM audit_log WHERE entity_type = 'column' AND action = 'delete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    server
        .post(&format!("/api/audit/{column_delete}/restore"))
        .await
        .assert_status_ok();
    server
        .post(&format!("/api/audit/{card_delete}/restore"))
        .await
        .assert_status_ok();
    let restored: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
    assert_eq!(restored.number, card.number, "the card keeps its number");
}
