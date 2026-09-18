//! Card links (iteration 42 — card #76): creation rules, cycle rejection,
//! reasons, history and cascades.

use super::*;

/// Three cards in one column, ready to be linked.
async fn setup_three_cards(
    server: &TestServer,
) -> (shared::Board, shared::Column, [shared::Card; 3]) {
    let (board, column) = setup_board_and_column(server).await;
    let mut cards = Vec::new();
    for title in ["# A", "# B", "# C"] {
        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: title.to_string(),
                ..Default::default()
            })
            .await
            .json();
        cards.push(card);
    }
    let cards: [shared::Card; 3] = cards.try_into().expect("three cards");
    (board, column, cards)
}

/// `POST /api/cards/:id/links` with `other` as the successor of `card`.
async fn link_after(
    server: &TestServer,
    card: &shared::Card,
    other: &shared::Card,
    reason: Option<&str>,
) -> axum_test::TestResponse {
    server
        .post(&format!("/api/cards/{}/links", card.id))
        .json(&shared::CreateCardLinkRequest {
            direction: shared::LinkDirection::Successor,
            other_card_id: other.id.clone(),
            reason: reason.map(str::to_string),
        })
        .await
}

async fn board_links(server: &TestServer, board: &shared::Board) -> Vec<shared::CardLink> {
    server
        .get(&format!("/api/boards/{}/links", board.name))
        .await
        .json()
}

#[tokio::test]
async fn create_link_returns_it_with_both_card_numbers() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;

    let resp = link_after(&server, &a, &b, Some("  B needs A's API  ")).await;
    resp.assert_status(StatusCode::CREATED);
    let link: shared::CardLink = resp.json();
    assert_eq!(link.predecessor_id, a.id);
    assert_eq!(link.successor_id, b.id);
    // Numbers are projected from the cards, not stored on the link.
    assert_eq!(link.predecessor_number, a.number);
    assert_eq!(link.successor_number, b.number);
    // The reason is trimmed on the way in.
    assert_eq!(link.reason.as_deref(), Some("B needs A's API"));

    // The board listing round-trips the same row.
    let listed = board_links(&server, &board).await;
    assert_eq!(listed, vec![link]);
}

#[tokio::test]
async fn link_is_one_fact_whichever_end_creates_it() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;

    // "A is a predecessor of B", asked from B's side …
    let from_b: shared::CardLink = server
        .post(&format!("/api/cards/{}/links", b.id))
        .json(&shared::CreateCardLinkRequest {
            direction: shared::LinkDirection::Predecessor,
            other_card_id: a.id.clone(),
            reason: None,
        })
        .await
        .json();
    assert_eq!(from_b.predecessor_id, a.id);
    assert_eq!(from_b.successor_id, b.id);

    // … is the same row as "B is a successor of A" asked from A's side,
    // so the second request is a conflict rather than a second link.
    link_after(&server, &a, &b, None)
        .await
        .assert_status(StatusCode::CONFLICT);
    assert_eq!(board_links(&server, &board).await.len(), 1);
}

#[tokio::test]
async fn empty_reason_is_stored_as_none() {
    let server = test_app().await;
    let (_, _, [a, b, _]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, Some("   ")).await.json();
    assert_eq!(link.reason, None);
}

#[tokio::test]
async fn self_link_is_rejected_with_422() {
    let server = test_app().await;
    let (_, _, [a, _, _]) = setup_three_cards(&server).await;
    let resp = link_after(&server, &a, &a, None).await;
    resp.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(resp.text().contains("itself"));
}

#[tokio::test]
async fn reciprocal_link_is_rejected_as_a_cycle() {
    let server = test_app().await;
    let (_, _, [a, b, _]) = setup_three_cards(&server).await;
    link_after(&server, &a, &b, None)
        .await
        .assert_status(StatusCode::CREATED);
    let resp = link_after(&server, &b, &a, None).await;
    resp.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(resp.text().contains("loop"));
}

#[tokio::test]
async fn longer_cycle_is_rejected() {
    let server = test_app().await;
    let (board, _, [a, b, c]) = setup_three_cards(&server).await;
    link_after(&server, &a, &b, None)
        .await
        .assert_status(StatusCode::CREATED);
    link_after(&server, &b, &c, None)
        .await
        .assert_status(StatusCode::CREATED);
    // C → A would close A → B → C → A.
    link_after(&server, &c, &a, None)
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    // A → C points the same way as the chain and is fine.
    link_after(&server, &a, &c, None)
        .await
        .assert_status(StatusCode::CREATED);
    assert_eq!(board_links(&server, &board).await.len(), 3);
}

#[tokio::test]
async fn cross_board_link_is_rejected() {
    let server = test_app().await;
    let (_, _, [a, _, _]) = setup_three_cards(&server).await;

    let other_board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "other-board".to_string(),
        })
        .await
        .json();
    let other_col: shared::Column = server
        .post(&format!("/api/boards/{}/columns", other_board.name))
        .json(&shared::CreateColumnRequest {
            name: "Col".to_string(),
            position: 0,
        })
        .await
        .json();
    let foreign: shared::Card = server
        .post(&format!("/api/columns/{}/cards", other_col.id))
        .json(&shared::CreateCardRequest {
            body: "# Elsewhere".to_string(),
            ..Default::default()
        })
        .await
        .json();

    let resp = link_after(&server, &a, &foreign, None).await;
    resp.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(resp.text().contains("same board"));
}

#[tokio::test]
async fn over_long_reason_is_rejected() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;
    let long = "x".repeat(shared::links::MAX_REASON_CHARS + 1);
    link_after(&server, &a, &b, Some(&long))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    // Nothing was written.
    assert!(board_links(&server, &board).await.is_empty());
}

#[tokio::test]
async fn linking_an_unknown_card_is_404() {
    let server = test_app().await;
    let (_, _, [a, _, _]) = setup_three_cards(&server).await;
    server
        .post(&format!("/api/cards/{}/links", a.id))
        .json(&shared::CreateCardLinkRequest {
            direction: shared::LinkDirection::Successor,
            other_card_id: "01hzzzzzzzzzzzzzzzzzzzzzzz".to_string(),
            reason: None,
        })
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .post("/api/cards/01hzzzzzzzzzzzzzzzzzzzzzzz/links")
        .json(&shared::CreateCardLinkRequest {
            direction: shared::LinkDirection::Successor,
            other_card_id: a.id.clone(),
            reason: None,
        })
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_link_changes_only_the_reason() {
    let server = test_app().await;
    let (_, _, [a, b, _]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, Some("first")).await.json();

    let updated: shared::CardLink = server
        .put(&format!("/api/links/{}", link.id))
        .json(&shared::UpdateCardLinkRequest {
            reason: Some("second".to_string()),
        })
        .await
        .json();
    assert_eq!(updated.id, link.id);
    assert_eq!(updated.predecessor_id, a.id);
    assert_eq!(updated.reason.as_deref(), Some("second"));

    // An empty reason clears it.
    let cleared: shared::CardLink = server
        .put(&format!("/api/links/{}", link.id))
        .json(&shared::UpdateCardLinkRequest {
            reason: Some(String::new()),
        })
        .await
        .json();
    assert_eq!(cleared.reason, None);

    // Unknown link → 404; over-long reason → 422.
    server
        .put("/api/links/01hzzzzzzzzzzzzzzzzzzzzzzz")
        .json(&shared::UpdateCardLinkRequest::default())
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put(&format!("/api/links/{}", link.id))
        .json(&shared::UpdateCardLinkRequest {
            reason: Some("x".repeat(shared::links::MAX_REASON_CHARS + 1)),
        })
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn omitting_reason_on_update_clears_it() {
    // Pins the current behaviour: `UpdateCardLinkRequest.reason` is
    // `#[serde(default)]`, so a body with the key left out entirely
    // deserialises the same as `reason: None` and wipes it — unlike
    // `update_card`, which treats an absent field as untouched. If this
    // ever changes to match that convention, this test should change too.
    let server = test_app().await;
    let (_, _, [a, b, _]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, Some("first")).await.json();

    let cleared: shared::CardLink = server
        .put(&format!("/api/links/{}", link.id))
        .json(&serde_json::json!({}))
        .await
        .json();
    assert_eq!(cleared.reason, None);
}

#[tokio::test]
async fn re_sending_the_same_reason_writes_no_history() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, Some("why")).await.json();

    server
        .put(&format!("/api/links/{}", link.id))
        .json(&shared::UpdateCardLinkRequest {
            reason: Some("  why  ".to_string()),
        })
        .await
        .assert_status_ok();

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    let link_rows: Vec<_> = hist
        .iter()
        .filter(|e| e.entity_type == "card_link")
        .collect();
    assert_eq!(link_rows.len(), 1, "only the create row");
    assert_eq!(link_rows[0].action, "create");
}

#[tokio::test]
async fn delete_link_removes_it_from_the_board() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, None).await.json();

    server
        .delete(&format!("/api/links/{}", link.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(board_links(&server, &board).await.is_empty());
    server
        .delete(&format!("/api/links/{}", link.id))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // Once the link is gone the reverse direction is legal again.
    link_after(&server, &b, &a, None)
        .await
        .assert_status(StatusCode::CREATED);
}

#[tokio::test]
async fn link_changes_appear_in_board_and_both_card_histories() {
    let server = test_app().await;
    let (board, _, [a, b, c]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, Some("why")).await.json();
    server
        .put(&format!("/api/links/{}", link.id))
        .json(&shared::UpdateCardLinkRequest {
            reason: Some("because".to_string()),
        })
        .await
        .assert_status_ok();
    server
        .delete(&format!("/api/links/{}", link.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let actions = |rows: &[shared::AuditLogEntry]| -> Vec<String> {
        rows.iter()
            .filter(|e| e.entity_type == "card_link" && e.entity_id == link.id)
            .map(|e| e.action.clone())
            .collect()
    };

    // Board history: newest first.
    let board_hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    assert_eq!(actions(&board_hist), vec!["delete", "update", "create"]);

    // Both cards see the same three rows; the unrelated card sees none.
    for card in [&a, &b] {
        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        assert_eq!(actions(&hist), vec!["delete", "update", "create"]);
        // Interleaved correctly with the card's own rows: the card's
        // create row is the oldest thing in its history.
        assert_eq!(hist.last().map(|e| e.entity_type.as_str()), Some("card"));
    }
    let unrelated: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", c.id))
        .await
        .json();
    assert!(actions(&unrelated).is_empty());

    // The snapshots carry the card numbers so the drawer can label the row.
    let create_row = board_hist
        .iter()
        .find(|e| e.entity_type == "card_link" && e.action == "create")
        .expect("create row");
    let after = create_row.snapshot_after.as_ref().expect("snapshot");
    assert_eq!(after["predecessor_number"], a.number);
    assert_eq!(after["successor_number"], b.number);
    assert_eq!(after["reason"], "why");
}

#[tokio::test]
async fn deleting_a_card_removes_its_links_and_records_them() {
    let server = test_app().await;
    let (board, _, [a, b, c]) = setup_three_cards(&server).await;
    let ab: shared::CardLink = link_after(&server, &a, &b, None).await.json();
    let bc: shared::CardLink = link_after(&server, &b, &c, None).await.json();

    server
        .delete(&format!("/api/cards/{}", b.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Both links touched B, so both are gone.
    assert!(board_links(&server, &board).await.is_empty());

    // A's history records the loss of its link even though A itself was
    // never touched.
    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", a.id))
        .await
        .json();
    assert!(
        hist.iter()
            .any(|e| e.entity_type == "card_link" && e.entity_id == ab.id && e.action == "delete")
    );
    assert!(!hist.iter().any(|e| e.entity_id == bc.id));

    // The link delete rows land before the card delete row in time, so a
    // newest-first listing shows the card going last.
    let board_hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    assert_eq!(board_hist[0].entity_type, "card");
    assert_eq!(board_hist[0].action, "delete");
    assert_eq!(board_hist[1].entity_type, "card_link");
    assert_eq!(board_hist[2].entity_type, "card_link");
}

#[tokio::test]
async fn deleting_a_column_cascades_links_and_its_restore_skips_them() {
    let server = test_app().await;
    let (board, column, [a, b, _]) = setup_three_cards(&server).await;
    link_after(&server, &a, &b, None)
        .await
        .assert_status(StatusCode::CREATED);

    server
        .delete(&format!("/api/columns/{}", column.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(board_links(&server, &board).await.is_empty());

    // The cascade grouped the link delete with the column delete …
    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    let col_delete = hist
        .iter()
        .find(|e| e.entity_type == "column" && e.action == "delete")
        .expect("column delete row");
    let link_delete = hist
        .iter()
        .find(|e| e.entity_type == "card_link" && e.action == "delete")
        .expect("link delete row");
    assert!(col_delete.batch_group.is_some());
    assert_eq!(link_delete.batch_group, col_delete.batch_group);

    // … but restoring the column brings back the cards and not the link.
    server
        .post(&format!("/api/audit/{}/restore", col_delete.id))
        .await
        .assert_status_ok();
    let cards: Vec<shared::Card> = server
        .get(&format!("/api/columns/{}/cards", column.id))
        .await
        .json();
    assert_eq!(cards.len(), 3);
    assert!(board_links(&server, &board).await.is_empty());
}

#[tokio::test]
async fn deleting_a_board_cascades_its_links() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, None).await.json();

    server
        .delete(&format!("/api/boards/{}", board.name))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // The row itself is gone — a fresh board with the same slug has no links.
    let again: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: board.name.clone(),
        })
        .await
        .json();
    assert!(board_links(&server, &again).await.is_empty());
    server
        .delete(&format!("/api/links/{}", link.id))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn link_audit_rows_are_not_restorable() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, None).await.json();
    server
        .delete(&format!("/api/links/{}", link.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    for row in hist.iter().filter(|e| e.entity_type == "card_link") {
        server
            .post(&format!("/api/audit/{}/restore", row.id))
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert!(board_links(&server, &board).await.is_empty());
}

#[tokio::test]
async fn links_for_an_unknown_board_are_404() {
    let server = test_app().await;
    server
        .get("/api/boards/no-such-board/links")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
