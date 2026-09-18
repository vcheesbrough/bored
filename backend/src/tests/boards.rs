//! Board CRUD and board-delete cascades.

use super::*;

#[tokio::test]
async fn create_board_starts_empty() {
    // New boards have no default columns — the user creates them manually.
    let server = test_app().await;

    let create_resp = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "empty-board".to_string(),
        })
        .await;
    create_resp.assert_status(StatusCode::CREATED);
    let board: shared::Board = create_resp.json();

    let list_resp = server
        .get(&format!("/api/boards/{}/columns", board.name))
        .await;
    list_resp.assert_status_ok();
    let columns: Vec<shared::Column> = list_resp.json();
    assert_eq!(columns.len(), 0, "new board must have no default columns");
}

#[tokio::test]
async fn create_board_and_list() {
    let server = test_app().await;

    let create_resp = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "test-board".to_string(),
        })
        .await;
    create_resp.assert_status(StatusCode::CREATED);
    let board: shared::Board = create_resp.json();
    assert_eq!(board.name, "test-board");

    let list_resp = server.get("/api/boards").await;
    list_resp.assert_status_ok();
    let boards: Vec<shared::Board> = list_resp.json();
    // `.any(...)` returns true if at least one element satisfies the predicate.
    assert!(boards.iter().any(|b| b.id == board.id));
}

#[tokio::test]
async fn create_board_invalid_name_returns_422() {
    let server = test_app().await;
    // Names with spaces, uppercase, or leading/trailing hyphens are rejected.
    for bad in &["My Board", "UPPER", "-leading", "trailing-", ""] {
        server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: bad.to_string(),
            })
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[tokio::test]
async fn create_board_duplicate_name_returns_409() {
    let server = test_app().await;
    server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "dupe-board".to_string(),
        })
        .await
        .assert_status(StatusCode::CREATED);
    server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "dupe-board".to_string(),
        })
        .await
        .assert_status(StatusCode::CONFLICT);
}

#[tokio::test]
async fn get_board_by_slug() {
    let server = test_app().await;

    let create_resp = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "get-me".to_string(),
        })
        .await;
    let board: shared::Board = create_resp.json();

    let get_resp = server.get(&format!("/api/boards/{}", board.name)).await;
    get_resp.assert_status_ok();
    let fetched: shared::Board = get_resp.json();
    assert_eq!(fetched.id, board.id);
    assert_eq!(fetched.name, "get-me");
}

#[tokio::test]
async fn update_board_name() {
    let server = test_app().await;

    let create_resp = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "old-name".to_string(),
        })
        .await;
    let board: shared::Board = create_resp.json();

    let update_resp = server
        .put(&format!("/api/boards/{}", board.name))
        .json(&shared::UpdateBoardRequest {
            name: "new-name".to_string(),
        })
        .await;
    update_resp.assert_status_ok();
    let updated: shared::Board = update_resp.json();
    assert_eq!(updated.name, "new-name");

    // After rename, fetch by the new slug.
    let get_resp = server.get(&format!("/api/boards/{}", updated.name)).await;
    let fetched: shared::Board = get_resp.json();
    assert_eq!(fetched.name, "new-name");
}

#[tokio::test]
async fn delete_board_returns_404_on_get() {
    let server = test_app().await;

    let create_resp = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "delete-me".to_string(),
        })
        .await;
    let board: shared::Board = create_resp.json();

    let del_resp = server.delete(&format!("/api/boards/{}", board.name)).await;
    del_resp.assert_status(StatusCode::NO_CONTENT);

    let get_resp = server.get(&format!("/api/boards/{}", board.name)).await;
    get_resp.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_board_cascades_columns() {
    let server = test_app().await;

    let create_board_resp = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "board-for-cascade".to_string(),
        })
        .await;
    let board: shared::Board = create_board_resp.json();

    let create_col_resp = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Col 1".to_string(),
            position: 0,
        })
        .await;
    let column: shared::Column = create_col_resp.json();

    server
        .delete(&format!("/api/boards/{}", board.name))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let update_resp = server
        .put(&format!("/api/columns/{}", column.id))
        .json(&shared::UpdateColumnRequest {
            name: Some("Updated".to_string()),
            position: None,
        })
        .await;
    update_resp.assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_board_cascades_columns_and_cards() {
    let server = test_app().await;
    let (board, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "Deep Orphan card".to_string(),
            ..Default::default()
        })
        .await
        .json();

    server
        .delete(&format!("/api/boards/{}", board.name))
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
            ..Default::default()
        })
        .await;
    card_resp.assert_status(StatusCode::NOT_FOUND);
}
