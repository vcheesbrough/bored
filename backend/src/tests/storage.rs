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

/// A restored card whose number has since been taken — possible for a
/// snapshot written before the import renumbered a duplicate — gets a fresh
/// number instead of failing on `cards_number_unique`.
#[tokio::test]
async fn restoring_a_card_whose_number_was_taken_draws_a_fresh_one() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (_board, column) = setup_board_and_column(&server).await;
    let path = format!("/api/columns/{}/cards", column.id);
    let deleted: shared::Card = server
        .post(&path)
        .json(&shared::CreateCardRequest {
            body: "deleted".to_string(),
            ..Default::default()
        })
        .await
        .json();
    let live: shared::Card = server
        .post(&path)
        .json(&shared::CreateCardRequest {
            body: "live".to_string(),
            ..Default::default()
        })
        .await
        .json();
    server
        .delete(&format!("/api/cards/{}", deleted.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Give the live card the deleted card's number, as a renumbering import
    // could have left things.
    sqlx::query("UPDATE cards SET number = $1 WHERE id = $2")
        .bind(i32::try_from(deleted.number).unwrap())
        .bind(&live.id)
        .execute(&db)
        .await
        .unwrap();

    let delete_row: String = sqlx::query_scalar(
        "SELECT id FROM audit_log WHERE entity_type = 'card' AND action = 'delete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    server
        .post(&format!("/api/audit/{delete_row}/restore"))
        .await
        .assert_status_ok();

    let restored: shared::Card = server
        .get(&format!("/api/cards/{}", deleted.id))
        .await
        .json();
    assert_ne!(
        restored.number, deleted.number,
        "the taken number is not reused"
    );
    assert!(
        restored.number > live.number,
        "a fresh number from the sequence: {} vs {}",
        restored.number,
        live.number
    );
}

/// Make every card delete in the test database insert a new card into the
/// same column — a stand-in for a create landing mid-cascade — at most
/// `times` times (a counter table bounds it).
async fn add_a_card_after_each_delete(db: &crate::db::Db, times: i32) {
    for statement in [
        "CREATE TABLE late_inserts (remaining integer NOT NULL)".to_string(),
        format!("INSERT INTO late_inserts VALUES ({times})"),
        "CREATE FUNCTION add_late_card() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF (SELECT remaining FROM late_inserts) > 0 THEN \
             UPDATE late_inserts SET remaining = remaining - 1; \
             INSERT INTO cards (id, column_id, body, position) \
               VALUES ('late-' || gen_random_uuid(), OLD.column_id, 'late', 1); \
           END IF; \
           RETURN OLD; \
         END $$"
            .to_string(),
        "CREATE TRIGGER add_late_card AFTER DELETE ON cards \
         FOR EACH ROW EXECUTE FUNCTION add_late_card()"
            .to_string(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(db)
            .await
            .expect("test trigger");
    }
}

/// A card created in a column while it is being deleted is swept too, and
/// the column's delete row is written once — after the column is gone.
#[tokio::test]
async fn a_card_added_mid_delete_is_swept_and_the_column_still_goes() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (_board, column) = setup_board_and_column(&server).await;
    server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "first".to_string(),
            ..Default::default()
        })
        .await
        .assert_status(StatusCode::CREATED);
    add_a_card_after_each_delete(&db, 1).await;

    server
        .delete(&format!("/api/columns/{}", column.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let columns: i64 = sqlx::query_scalar("SELECT count(*) FROM columns")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(columns, 0);
    let card_deletes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE entity_type = 'card' AND action = 'delete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(card_deletes, 2, "the late card is audited like the first");
    let column_deletes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE entity_type = 'column' AND action = 'delete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(column_deletes, 1);
}

/// A column that keeps filling faster than the delete empties it is a 409,
/// and history does not claim the column was deleted.
#[tokio::test]
async fn a_column_that_keeps_filling_is_a_conflict_with_no_false_history() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (board, column) = setup_board_and_column(&server).await;
    server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "first".to_string(),
            ..Default::default()
        })
        .await
        .assert_status(StatusCode::CREATED);
    add_a_card_after_each_delete(&db, 1_000).await;

    server
        .delete(&format!("/api/columns/{}", column.id))
        .await
        .assert_status(StatusCode::CONFLICT);
    server
        .delete(&format!("/api/boards/{}", board.name))
        .await
        .assert_status(StatusCode::CONFLICT);

    let parent_deletes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE entity_type IN ('column', 'board') \
         AND action = 'delete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(parent_deletes, 0);
    let columns: i64 = sqlx::query_scalar("SELECT count(*) FROM columns")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(columns, 1, "the column is still there");
}

/// A card moved to another column while its old column is being deleted is
/// left alone in its new home: the cascade re-reads each card and deletes by
/// id *and* column. A trigger moves `k-moved` out as the first card goes.
#[tokio::test]
async fn a_card_moved_out_mid_delete_survives_in_its_new_column() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (board, column) = setup_board_and_column(&server).await;
    let other: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Other".to_string(),
            position: 1,
        })
        .await
        .json();
    for statement in [
        format!(
            "INSERT INTO cards (id, column_id, body, position) VALUES \
             ('k-first', '{0}', 'first', 1), ('k-moved', '{0}', 'moved', 2)",
            column.id
        ),
        format!(
            "CREATE FUNCTION move_out() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF OLD.id = 'k-first' THEN \
                 UPDATE cards SET column_id = '{}' WHERE id = 'k-moved'; \
               END IF; \
               RETURN OLD; \
             END $$",
            other.id
        ),
        "CREATE TRIGGER move_out AFTER DELETE ON cards \
         FOR EACH ROW EXECUTE FUNCTION move_out()"
            .to_string(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&db)
            .await
            .expect("setup");
    }

    server
        .delete(&format!("/api/columns/{}", column.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let moved: shared::Card = server.get("/api/cards/k-moved").await.json();
    assert_eq!(
        moved.column_id, other.id,
        "the moved card keeps its new home"
    );
    let deleted_cards: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id FROM audit_log WHERE entity_type = 'card' AND action = 'delete'",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(deleted_cards, vec!["k-first".to_string()]);
}

/// Restoring a card whose number is above the sequence (deleted before the
/// import, so only history held its number) moves the sequence past it: the
/// next new card gets a fresh number rather than a duplicate.
#[tokio::test]
async fn restoring_a_card_numbered_above_the_sequence_moves_the_sequence() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (_board, column) = setup_board_and_column(&server).await;
    let path = format!("/api/columns/{}/cards", column.id);
    let card: shared::Card = server
        .post(&path)
        .json(&shared::CreateCardRequest {
            body: "historic".to_string(),
            ..Default::default()
        })
        .await
        .json();
    server
        .delete(&format!("/api/cards/{}", card.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Make the deleted card's snapshot carry a number far above the sequence.
    sqlx::query(
        "UPDATE audit_log SET snapshot_before = jsonb_set(snapshot_before, '{number}', '500') \
         WHERE action = 'delete'",
    )
    .execute(&db)
    .await
    .unwrap();
    let delete_row: String = sqlx::query_scalar("SELECT id FROM audit_log WHERE action = 'delete'")
        .fetch_one(&db)
        .await
        .unwrap();
    server
        .post(&format!("/api/audit/{delete_row}/restore"))
        .await
        .assert_status_ok();
    let restored: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
    assert_eq!(restored.number, 500, "the free number is kept");

    let next: shared::Card = server
        .post(&path)
        .json(&shared::CreateCardRequest {
            body: "new".to_string(),
            ..Default::default()
        })
        .await
        .json();
    assert_eq!(next.number, 501);
}

/// The cascade holds each card's row lock from its re-read until its delete,
/// so a move cannot land between the card's audit row and the delete. Proved
/// from *inside* that window: a trigger on the audit insert (which runs while
/// the lock is held) tries to move the card with a short `lock_timeout` and
/// records whether it was blocked. Without the lock the move would go
/// through, and the cascade would then audit a card that is no longer there.
#[tokio::test]
async fn a_move_during_a_cascade_card_step_waits_for_the_row_lock() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (board, column) = setup_board_and_column(&server).await;
    let other: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Other".to_string(),
            position: 1,
        })
        .await
        .json();
    for statement in [
        format!(
            "INSERT INTO cards (id, column_id, body, position) VALUES ('k1', '{}', 'x', 1)",
            column.id
        ),
        "CREATE TABLE move_attempts (outcome text NOT NULL)".to_string(),
        format!(
            "CREATE FUNCTION try_move() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF NEW.entity_type = 'card' AND NEW.action = 'delete' THEN \
                 BEGIN \
                   SET LOCAL lock_timeout = '200ms'; \
                   UPDATE cards SET column_id = '{}' WHERE id = 'k1'; \
                   INSERT INTO move_attempts VALUES ('moved'); \
                 EXCEPTION WHEN lock_not_available THEN \
                   INSERT INTO move_attempts VALUES ('blocked'); \
                 END; \
               END IF; \
               RETURN NEW; \
             END $$",
            other.id
        ),
        "CREATE TRIGGER try_move AFTER INSERT ON audit_log \
         FOR EACH ROW EXECUTE FUNCTION try_move()"
            .to_string(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&db)
            .await
            .expect("setup");
    }

    server
        .delete(&format!("/api/columns/{}", column.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let outcomes: Vec<String> = sqlx::query_scalar("SELECT outcome FROM move_attempts")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(outcomes, vec!["blocked".to_string()]);
}

/// A restore whose parent is deleted in the instant between its existence
/// check and its insert is the same 422 as a parent already gone. A trigger
/// deletes the board as the column row is inserted — exactly that window.
#[tokio::test]
async fn a_parent_deleted_during_a_restore_is_the_same_422() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (_board, column) = setup_board_and_column(&server).await;
    server
        .delete(&format!("/api/columns/{}", column.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    for statement in [
        "CREATE FUNCTION drop_parent() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN DELETE FROM boards WHERE id = NEW.board_id; RETURN NEW; END $$",
        "CREATE TRIGGER drop_parent BEFORE INSERT ON columns \
         FOR EACH ROW EXECUTE FUNCTION drop_parent()",
    ] {
        sqlx::query(statement).execute(&db).await.expect("setup");
    }
    let delete_row: String = sqlx::query_scalar(
        "SELECT id FROM audit_log WHERE entity_type = 'column' AND action = 'delete'",
    )
    .fetch_one(&db)
    .await
    .unwrap();

    let response = server
        .post(&format!("/api/audit/{delete_row}/restore"))
        .await;
    response.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.text(),
        "restore the board or column this belonged to first"
    );
}

/// A link create racing the deletion of one of its cards — the card goes
/// after the route's re-check but before the INSERT — is a 404, not the
/// foreign-key 500. A trigger deletes the successor in exactly that window.
#[tokio::test]
async fn a_link_to_a_card_deleted_mid_create_is_not_found() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (_board, column) = setup_board_and_column(&server).await;
    let path = format!("/api/columns/{}/cards", column.id);
    let mut ids = Vec::new();
    for body in ["a", "b"] {
        let card: shared::Card = server
            .post(&path)
            .json(&shared::CreateCardRequest {
                body: body.to_string(),
                ..Default::default()
            })
            .await
            .json();
        ids.push(card.id);
    }
    for statement in [
        "CREATE FUNCTION drop_successor() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN DELETE FROM cards WHERE id = NEW.successor_id; RETURN NEW; END $$",
        "CREATE TRIGGER drop_successor BEFORE INSERT ON card_links \
         FOR EACH ROW EXECUTE FUNCTION drop_successor()",
    ] {
        sqlx::query(statement).execute(&db).await.expect("setup");
    }

    server
        .post(&format!("/api/cards/{}/links", ids[0]))
        .json(&shared::CreateCardLinkRequest {
            other_card_id: ids[1].clone(),
            direction: shared::LinkDirection::Successor,
            reason: None,
        })
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// A column create racing its board's delete is a 404, not the foreign-key
/// 500: a trigger deletes the board in the window after the route's lookup.
#[tokio::test]
async fn a_column_for_a_board_deleted_mid_create_is_not_found() {
    let db = db::connect_test().await.expect("test db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "doomed".to_string(),
        })
        .await
        .json();
    for statement in [
        "CREATE FUNCTION drop_board() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN DELETE FROM boards WHERE id = NEW.board_id; RETURN NEW; END $$",
        "CREATE TRIGGER drop_board BEFORE INSERT ON columns \
         FOR EACH ROW EXECUTE FUNCTION drop_board()",
    ] {
        sqlx::query(statement).execute(&db).await.expect("setup");
    }
    server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "c".to_string(),
            position: 0,
        })
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
