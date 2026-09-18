//! Card CRUD, moves between columns and position ordering.

use super::*;

#[tokio::test]
async fn create_card_and_list() {
    let server = test_app().await;
    // `_` discards the board; we only need the column.
    let (_, column) = setup_board_and_column(&server).await;

    let create_resp = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Fix bug\n\nDetails here".to_string(),
            ..Default::default()
        })
        .await;
    create_resp.assert_status(StatusCode::CREATED);
    let card: shared::Card = create_resp.json();
    assert_eq!(card.body, "# Fix bug\n\nDetails here");
    assert_eq!(card.column_id, column.id);

    let list_resp = server
        .get(&format!("/api/columns/{}/cards", column.id))
        .await;
    list_resp.assert_status_ok();
    let cards: Vec<shared::Card> = list_resp.json();
    assert!(cards.iter().any(|c| c.id == card.id));
}

#[tokio::test]
async fn update_card_body() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Old body".to_string(),
            ..Default::default()
        })
        .await
        .json();

    let update_resp = server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("# New body\n\nWith details".to_string()),
            ..Default::default()
        })
        .await;
    update_resp.assert_status_ok();
    let updated: shared::Card = update_resp.json();
    assert_eq!(updated.body, "# New body\n\nWith details");
}

#[tokio::test]
async fn move_card_between_columns() {
    let server = test_app().await;
    let (board, col_a) = setup_board_and_column(&server).await;

    let col_b: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Col B".to_string(),
            position: 1,
        })
        .await
        .json();

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", col_a.id))
        .json(&shared::CreateCardRequest {
            body: "Movable card".to_string(),
            ..Default::default()
        })
        .await
        .json();

    let move_resp = server
        .post(&format!("/api/cards/{}/move", card.id))
        .json(&shared::MoveCardRequest {
            column_id: col_b.id.clone(),
            position: 0,
        })
        .await;
    move_resp.assert_status_ok();
    let moved: shared::Card = move_resp.json();
    assert_eq!(moved.column_id, col_b.id);

    // Verify the card is no longer in col_a.
    let cards_a: Vec<shared::Card> = server
        .get(&format!("/api/columns/{}/cards", col_a.id))
        .await
        .json();
    assert!(!cards_a.iter().any(|c| c.id == card.id));

    // Verify the card is now in col_b.
    let cards_b: Vec<shared::Card> = server
        .get(&format!("/api/columns/{}/cards", col_b.id))
        .await
        .json();
    assert!(cards_b.iter().any(|c| c.id == card.id));
}

#[tokio::test]
async fn delete_card() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "To Delete".to_string(),
            ..Default::default()
        })
        .await
        .json();

    server
        .delete(&format!("/api/cards/{}", card.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let cards: Vec<shared::Card> = server
        .get(&format!("/api/columns/{}/cards", column.id))
        .await
        .json();
    assert!(!cards.iter().any(|c| c.id == card.id));
}

#[tokio::test]
async fn cards_returned_ordered_by_position() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let c1: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "Card 1".to_string(),
            ..Default::default()
        })
        .await
        .json();
    let c2: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "Card 2".to_string(),
            ..Default::default()
        })
        .await
        .json();
    let c3: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "Card 3".to_string(),
            ..Default::default()
        })
        .await
        .json();

    // Deliberately set positions out of insertion order to verify sorting.
    server
        .put(&format!("/api/cards/{}", c1.id))
        .json(&shared::UpdateCardRequest {
            position: Some(2),
            ..Default::default()
        })
        .await
        .assert_status_ok();
    server
        .put(&format!("/api/cards/{}", c2.id))
        .json(&shared::UpdateCardRequest {
            position: Some(0),
            ..Default::default()
        })
        .await
        .assert_status_ok();
    server
        .put(&format!("/api/cards/{}", c3.id))
        .json(&shared::UpdateCardRequest {
            position: Some(1),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let cards: Vec<shared::Card> = server
        .get(&format!("/api/columns/{}/cards", column.id))
        .await
        .json();

    assert_eq!(cards.len(), 3);
    assert_eq!(cards[0].body, "Card 2"); // position 0
    assert_eq!(cards[1].body, "Card 3"); // position 1
    assert_eq!(cards[2].body, "Card 1"); // position 2
}

#[tokio::test]
async fn create_card_in_nonexistent_column_returns_404() {
    let server = test_app().await;
    server
        .post("/api/columns/doesnotexist/cards")
        .json(&shared::CreateCardRequest {
            body: "Ghost card".to_string(),
            ..Default::default()
        })
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn move_card_to_nonexistent_column_returns_404() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "Movable card".to_string(),
            ..Default::default()
        })
        .await
        .json();

    server
        .post(&format!("/api/cards/{}/move", card.id))
        .json(&shared::MoveCardRequest {
            column_id: "doesnotexist".to_string(),
            position: 0,
        })
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_nonexistent_card_returns_404() {
    let server = test_app().await;
    server
        .put("/api/cards/doesnotexist")
        .json(&shared::UpdateCardRequest {
            body: Some("x".to_string()),
            ..Default::default()
        })
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_card_by_id() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Get me".to_string(),
            ..Default::default()
        })
        .await
        .json();

    let resp = server.get(&format!("/api/cards/{}", card.id)).await;
    resp.assert_status_ok();
    let fetched: shared::Card = resp.json();
    assert_eq!(fetched.id, card.id);
    assert_eq!(fetched.body, "# Get me");
}

#[tokio::test]
async fn get_card_by_number() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    // Create a card and capture its sequential number assigned by the backend.
    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Number me".to_string(),
            ..Default::default()
        })
        .await
        .json();

    // Fetch the same card via the human-readable number endpoint.
    let resp = server
        .get(&format!("/api/cards/by-number/{}", card.number))
        .await;
    resp.assert_status_ok();
    let fetched: shared::Card = resp.json();
    assert_eq!(fetched.id, card.id);
    assert_eq!(fetched.number, card.number);
    assert_eq!(fetched.body, "# Number me");
}

#[tokio::test]
async fn get_card_by_nonexistent_number_returns_404() {
    let server = test_app().await;
    // u32::MAX is extremely unlikely to be a real card number in tests.
    server
        .get(&format!("/api/cards/by-number/{}", u32::MAX))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_nonexistent_card_returns_404() {
    let server = test_app().await;
    server
        .get("/api/cards/doesnotexist")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_nonexistent_card_returns_404() {
    let server = test_app().await;
    server
        .delete("/api/cards/doesnotexist")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn card_response_has_body_not_title_or_description() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# My Card\n\nSome content".to_string(),
            ..Default::default()
        })
        .await
        .json();

    // Verify the full body is preserved verbatim.
    assert_eq!(card.body, "# My Card\n\nSome content");
}
