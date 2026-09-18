//! Column CRUD, column-delete cascades and column reordering.

use super::*;

#[tokio::test]
async fn create_column_and_list() {
    let server = test_app().await;

    let create_board_resp = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "board-with-columns".to_string(),
        })
        .await;
    let board: shared::Board = create_board_resp.json();

    let create_col_resp = server
        .post(&format!("/api/boards/{}/columns", board.name))
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
        .get(&format!("/api/boards/{}/columns", board.name))
        .await;
    list_resp.assert_status_ok();
    let columns: Vec<shared::Column> = list_resp.json();
    assert!(columns.iter().any(|c| c.id == column.id));
}

#[tokio::test]
async fn delete_column_cascades_cards() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "Orphan card".to_string(),
            ..Default::default()
        })
        .await
        .json();

    server
        .delete(&format!("/api/columns/{}", column.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // The card should be gone — trying to update it should 404.
    let resp = server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("x".to_string()),
            ..Default::default()
        })
        .await;
    resp.assert_status(StatusCode::NOT_FOUND);
}

// Verifies that reorder_columns assigns positions matching the supplied order
// and returns the columns sorted by their new positions.
#[tokio::test]
async fn reorder_columns_assigns_positions() {
    let server = test_app().await;

    let board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "reorder-board".to_string(),
        })
        .await
        .json();

    // Create three columns explicitly (no default columns since iteration 13).
    let col_a: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Todo".to_string(),
            position: 0,
        })
        .await
        .json();
    let col_b: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Done".to_string(),
            position: 1,
        })
        .await
        .json();
    let col_c: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "In Progress".to_string(),
            position: 2,
        })
        .await
        .json();

    let col_todo = col_a.id.clone();
    let col_done = col_b.id.clone();
    let col_ip = col_c.id.clone();

    // Reorder to: In Progress, Todo, Done.
    let reorder_resp = server
        .put(&format!("/api/boards/{}/columns/reorder", board.name))
        .json(&shared::ColumnsReorderRequest {
            order: vec![col_ip.clone(), col_todo.clone(), col_done.clone()],
        })
        .await;
    reorder_resp.assert_status_ok();

    let reordered: Vec<shared::Column> = reorder_resp.json();
    assert_eq!(reordered.len(), 3);
    assert_eq!(reordered[0].id, col_ip);
    assert_eq!(reordered[0].position, 0);
    assert_eq!(reordered[1].id, col_todo);
    assert_eq!(reordered[1].position, 1);
    assert_eq!(reordered[2].id, col_done);
    assert_eq!(reordered[2].position, 2);
}

// Verifies that reorder_columns ignores column IDs that belong to a
// different board, preventing cross-board IDOR position writes.
#[tokio::test]
async fn reorder_columns_rejects_foreign_column_ids() {
    let server = test_app().await;

    // Board A — we will try to tamper with its column from board B's endpoint.
    let board_a: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "board-a".to_string(),
        })
        .await
        .json();

    // Create a column on board A explicitly (no default columns since iteration 13).
    let col_a_todo: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board_a.name))
        .json(&shared::CreateColumnRequest {
            name: "Todo".to_string(),
            position: 0,
        })
        .await
        .json();
    let original_position = col_a_todo.position;

    // Board B — the attacker's board. Submit board A's column ID in the order.
    let board_b: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "board-b".to_string(),
        })
        .await
        .json();

    // Create two columns on board B explicitly.
    let col_b_todo: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board_b.name))
        .json(&shared::CreateColumnRequest {
            name: "Todo".to_string(),
            position: 0,
        })
        .await
        .json();
    let col_b_done: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board_b.name))
        .json(&shared::CreateColumnRequest {
            name: "Done".to_string(),
            position: 1,
        })
        .await
        .json();

    // Inject board A's column into board B's reorder request.
    // The WHERE board = … clause should make this a no-op for col_a_todo.
    let resp = server
        .put(&format!("/api/boards/{}/columns/reorder", board_b.name))
        .json(&shared::ColumnsReorderRequest {
            order: vec![
                col_b_done.id.clone(),
                col_a_todo.id.clone(), // foreign — must be ignored
                col_b_todo.id.clone(),
            ],
        })
        .await;
    resp.assert_status_ok();

    // Board A's column must still have its original position.
    let cols_a_after: Vec<shared::Column> = server
        .get(&format!("/api/boards/{}/columns", board_a.name))
        .await
        .json();
    let col_a_todo_after = cols_a_after.iter().find(|c| c.id == col_a_todo.id).unwrap();
    assert_eq!(
        col_a_todo_after.position, original_position,
        "foreign column position must be unchanged after cross-board reorder"
    );
}
