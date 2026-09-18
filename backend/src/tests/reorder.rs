//! `PUT /api/columns/:id/cards/reorder` — bulk card reordering within a column.

use super::*;
use std::collections::HashSet;

/// Create `bodies` as cards in `column`, top-to-bottom in the order given.
/// `create_card` inserts at the *top*, so the list is created back to front.
async fn seed_cards(
    server: &TestServer,
    column: &shared::Column,
    bodies: &[&str],
) -> Vec<shared::Card> {
    for body in bodies.iter().rev() {
        server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: (*body).to_string(),
                ..Default::default()
            })
            .await
            .assert_status(StatusCode::CREATED);
    }
    server
        .get(&format!("/api/columns/{}/cards", column.id))
        .await
        .json()
}

/// `(id, position)` per card — `shared::Card` has no `PartialEq`, and
/// these are the two fields a reorder is allowed to be judged on.
fn card_positions(cards: &[shared::Card]) -> Vec<(String, i32)> {
    cards.iter().map(|c| (c.id.clone(), c.position)).collect()
}

async fn column_cards(server: &TestServer, column_id: &str) -> Vec<shared::Card> {
    server
        .get(&format!("/api/columns/{column_id}/cards"))
        .await
        .json()
}

/// Every `move` row recorded against a card on this board.
async fn card_move_rows(server: &TestServer, board: &shared::Board) -> Vec<shared::AuditLogEntry> {
    server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json::<Vec<shared::AuditLogEntry>>()
        .into_iter()
        .filter(|e| e.entity_type == "card" && e.action == "move")
        .collect()
}

#[tokio::test]
async fn reorder_cards_applies_and_persists_the_requested_order() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;
    let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
    let (a, b, g) = (
        cards[0].id.clone(),
        cards[1].id.clone(),
        cards[2].id.clone(),
    );

    let resp = server
        .put(&format!("/api/columns/{}/cards/reorder", column.id))
        .json(&shared::CardsReorderRequest {
            order: vec![g.clone(), a.clone(), b.clone()],
        })
        .await;
    resp.assert_status_ok();

    let returned: Vec<shared::Card> = resp.json();
    let ids: Vec<&str> = returned.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, [g.as_str(), a.as_str(), b.as_str()]);

    // Re-reading must agree with the response: positions are what order the
    // column, so an ambiguous write would show up here and not above.
    let persisted = column_cards(&server, &column.id).await;
    let ids: Vec<&str> = persisted.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, [g.as_str(), a.as_str(), b.as_str()]);
    // Distinct positions, strictly increasing — the property `ORDER BY
    // position ASC` needs to be deterministic.
    assert!(persisted.windows(2).all(|w| w[0].position < w[1].position));
}

#[tokio::test]
async fn reordering_into_the_current_order_writes_nothing() {
    let server = test_app().await;
    let (board, column) = setup_board_and_column(&server).await;
    let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
    let before = card_positions(&column_cards(&server, &column.id).await);
    let moves_before = card_move_rows(&server, &board).await.len();

    // Cards created into a fresh column land on 256/512/1024 — bisected,
    // not GAP multiples. A handler that renumbered unconditionally would
    // rewrite all three here and look correct while churning history.
    assert_eq!(
        before.iter().map(|(_, pos)| *pos).collect::<Vec<_>>(),
        [256, 512, 1024]
    );

    server
        .put(&format!("/api/columns/{}/cards/reorder", column.id))
        .json(&shared::CardsReorderRequest {
            order: cards.iter().map(|c| c.id.clone()).collect(),
        })
        .await
        .assert_status_ok();

    let after = card_positions(&column_cards(&server, &column.id).await);
    assert_eq!(before, after, "a no-op reorder must not touch any card");
    assert_eq!(
        card_move_rows(&server, &board).await.len(),
        moves_before,
        "a no-op reorder must not write history"
    );
}

#[tokio::test]
async fn reorder_cards_leaves_an_unmoved_cards_position_alone() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;
    let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
    let (a, b, g) = (
        cards[0].id.clone(),
        cards[1].id.clone(),
        cards[2].id.clone(),
    );
    let alpha_position = cards[0].position;

    // Swap the bottom two; alpha stays on top and must keep its exact value.
    server
        .put(&format!("/api/columns/{}/cards/reorder", column.id))
        .json(&shared::CardsReorderRequest {
            order: vec![a.clone(), g.clone(), b.clone()],
        })
        .await
        .assert_status_ok();

    let after = column_cards(&server, &column.id).await;
    assert_eq!(after[0].id, a);
    assert_eq!(after[0].position, alpha_position);
    let ids: Vec<&str> = after.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, [a.as_str(), g.as_str(), b.as_str()]);
}

#[tokio::test]
async fn reorder_cards_repairs_duplicate_positions() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;
    let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
    let (a, b, g) = (
        cards[0].id.clone(),
        cards[1].id.clone(),
        cards[2].id.clone(),
    );

    // `PUT /api/cards/:id` writes `position` verbatim and nothing enforces
    // uniqueness, so a column can legally reach this state.
    for id in [&b, &g] {
        server
            .put(&format!("/api/cards/{id}"))
            .json(&shared::UpdateCardRequest {
                position: Some(cards[0].position),
                ..Default::default()
            })
            .await
            .assert_status_ok();
    }

    server
        .put(&format!("/api/columns/{}/cards/reorder", column.id))
        .json(&shared::CardsReorderRequest {
            order: vec![g.clone(), b.clone(), a.clone()],
        })
        .await
        .assert_status_ok();

    let after = column_cards(&server, &column.id).await;
    let ids: Vec<&str> = after.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, [g.as_str(), b.as_str(), a.as_str()]);
    // Slot reuse is impossible here, so every card is renumbered onto the
    // same GAP grid `rebalance_column` uses.
    assert_eq!(
        after.iter().map(|c| c.position).collect::<Vec<_>>(),
        [1024, 2048, 3072]
    );
}

#[tokio::test]
async fn reorder_cards_ignores_ids_from_another_column() {
    let server = test_app().await;
    let (board, column) = setup_board_and_column(&server).await;
    let other: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Other".to_string(),
            position: 1,
        })
        .await
        .json();
    let cards = seed_cards(&server, &column, &["alpha", "beta"]).await;
    let outsider = seed_cards(&server, &other, &["zulu"]).await[0].clone();
    let outsider_before = outsider.clone();

    server
        .put(&format!("/api/columns/{}/cards/reorder", column.id))
        .json(&shared::CardsReorderRequest {
            order: vec![
                outsider.id.clone(),
                cards[1].id.clone(),
                cards[0].id.clone(),
            ],
        })
        .await
        .assert_status_ok();

    // The foreign id is dropped, not honoured and not fatal.
    let after = column_cards(&server, &column.id).await;
    let ids: Vec<&str> = after.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, [cards[1].id.as_str(), cards[0].id.as_str()]);
    // …and the other column is untouched — this is the IDOR guard.
    assert_eq!(
        card_positions(&column_cards(&server, &other.id).await),
        [(outsider_before.id, outsider_before.position)]
    );
}

#[tokio::test]
async fn reorder_cards_appends_omitted_ids_at_the_bottom() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;
    let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;
    let (a, b, g) = (
        cards[0].id.clone(),
        cards[1].id.clone(),
        cards[2].id.clone(),
    );

    // Only two of the three named — as would happen if a card were created
    // between the client reading the column and pressing the button.
    server
        .put(&format!("/api/columns/{}/cards/reorder", column.id))
        .json(&shared::CardsReorderRequest {
            order: vec![g.clone(), b.clone()],
        })
        .await
        .assert_status_ok();

    let after = column_cards(&server, &column.id).await;
    let ids: Vec<&str> = after.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, [g.as_str(), b.as_str(), a.as_str()]);
}

#[tokio::test]
async fn reorder_cards_records_one_batch_group_for_the_whole_move() {
    let server = test_app().await;
    let (board, column) = setup_board_and_column(&server).await;
    let cards = seed_cards(&server, &column, &["alpha", "beta", "gamma"]).await;

    server
        .put(&format!("/api/columns/{}/cards/reorder", column.id))
        .json(&shared::CardsReorderRequest {
            order: cards.iter().rev().map(|c| c.id.clone()).collect(),
        })
        .await
        .assert_status_ok();

    let moves = card_move_rows(&server, &board).await;
    assert!(!moves.is_empty(), "reversing the column must record moves");
    let groups: HashSet<Option<String>> = moves.iter().map(|m| m.batch_group.clone()).collect();
    assert_eq!(groups.len(), 1, "all moves belong to one batch");
    assert!(
        groups.iter().all(Option::is_some),
        "batch group must be recorded, not left null"
    );
}

#[tokio::test]
async fn reorder_cards_for_an_unknown_column_is_404() {
    let server = test_app().await;
    server
        .put("/api/columns/01hzzzzzzzzzzzzzzzzzzzzzzz/cards/reorder")
        .json(&shared::CardsReorderRequest { order: vec![] })
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
