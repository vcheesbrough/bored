//! Card links (iteration 42 — card #76): creation rules, cycle rejection,
//! reasons, history and cascades — and, since iteration 78 (card #544), links
//! between cards on different boards.

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

/// A second board, `name`, with one column holding a card per title.
async fn setup_other_board(
    server: &TestServer,
    name: &str,
    titles: &[&str],
) -> (shared::Board, Vec<shared::Card>) {
    let board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: name.to_string(),
        })
        .await
        .json();
    let column: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Col".to_string(),
            position: 0,
        })
        .await
        .json();
    let mut cards = Vec::new();
    for title in titles {
        let card: shared::Card = server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: (*title).to_string(),
                ..Default::default()
            })
            .await
            .json();
        cards.push(card);
    }
    (board, cards)
}

/// History rows for `link_id` on a board, newest first, as their actions.
async fn board_link_actions(
    server: &TestServer,
    board: &shared::Board,
    link_id: &str,
) -> Vec<String> {
    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    hist.into_iter()
        .filter(|e| e.entity_type == "card_link" && e.entity_id == link_id)
        .map(|e| e.action)
        .collect()
}

// ── Cross-board links (iteration 78 — card #544) ────────────────────────────

#[tokio::test]
async fn cross_board_link_is_created_and_listed_on_both_boards() {
    let server = test_app().await;
    let (board, _, [a, _, _]) = setup_three_cards(&server).await;
    let (other, far) = setup_other_board(&server, "other-board", &["# Elsewhere"]).await;

    let resp = link_after(&server, &a, &far[0], None).await;
    resp.assert_status(StatusCode::CREATED);
    let link: shared::CardLink = resp.json();
    // Each end names its own board.
    assert_eq!(link.predecessor_board_slug, board.name);
    assert_eq!(link.successor_board_slug, "other-board");
    assert_eq!(link.successor_number, far[0].number);

    // The same row is listed from either board.
    assert_eq!(board_links(&server, &board).await, vec![link.clone()]);
    assert_eq!(board_links(&server, &other).await, vec![link]);
}

#[tokio::test]
async fn a_same_board_link_names_the_same_board_at_both_ends() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;
    let link: shared::CardLink = link_after(&server, &a, &b, None).await.json();
    assert_eq!(link.predecessor_board_slug, board.name);
    assert_eq!(link.successor_board_slug, board.name);
}

#[tokio::test]
async fn a_board_lists_only_links_that_touch_it() {
    let server = test_app().await;
    let (board, _, [a, b, _]) = setup_three_cards(&server).await;
    let (other, far) = setup_other_board(&server, "other-board", &["# X", "# Y"]).await;
    let (third, _) = setup_other_board(&server, "third-board", &[]).await;

    let local: shared::CardLink = link_after(&server, &a, &b, None).await.json();
    let crossing: shared::CardLink = link_after(&server, &b, &far[0], None).await.json();
    let remote: shared::CardLink = link_after(&server, &far[0], &far[1], None).await.json();

    // Oldest first, and the link entirely on the other board is not here.
    assert_eq!(
        board_links(&server, &board).await,
        vec![local, crossing.clone()]
    );
    assert_eq!(board_links(&server, &other).await, vec![crossing, remote]);
    assert!(board_links(&server, &third).await.is_empty());
}

/// The loop A1 → B1 → B2 → A2 → A1 has its middle edge entirely on board B.
/// Board A's own links (A1 → B1 and B2 → A2) show no path from A1 to A2, so
/// a check over one board's links would let the closing edge through.
#[tokio::test]
async fn a_loop_through_another_board_is_rejected() {
    let server = test_app().await;
    let (_, _, [a1, a2, _]) = setup_three_cards(&server).await;
    let (_, far) = setup_other_board(&server, "other-board", &["# B1", "# B2"]).await;
    let (b1, b2) = (&far[0], &far[1]);

    for (from, to) in [(&a1, b1), (b1, b2), (b2, &a2)] {
        link_after(&server, from, to, None)
            .await
            .assert_status(StatusCode::CREATED);
    }
    let resp = link_after(&server, &a2, &a1, None).await;
    resp.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(resp.text().contains("loop"));
}

#[tokio::test]
async fn a_duplicate_cross_board_link_is_a_conflict() {
    let server = test_app().await;
    let (_, _, [a, _, _]) = setup_three_cards(&server).await;
    let (_, far) = setup_other_board(&server, "other-board", &["# Elsewhere"]).await;
    link_after(&server, &a, &far[0], None)
        .await
        .assert_status(StatusCode::CREATED);

    // The same fact asked from the other board's card.
    let resp = server
        .post(&format!("/api/cards/{}/links", far[0].id))
        .json(&shared::CreateCardLinkRequest {
            direction: shared::LinkDirection::Predecessor,
            other_card_id: a.id.clone(),
            reason: None,
        })
        .await;
    resp.assert_status(StatusCode::CONFLICT);
}

/// Every history row for a cross-board link is written on both boards, so each
/// board's history and each card's history show it — and the snapshots leave
/// the board slugs out.
#[tokio::test]
async fn cross_board_link_history_is_on_both_boards_and_both_cards() {
    let server = test_app().await;
    let (board, _, [a, _, _]) = setup_three_cards(&server).await;
    let (other, far) = setup_other_board(&server, "other-board", &["# Elsewhere"]).await;
    let link: shared::CardLink = link_after(&server, &a, &far[0], Some("why")).await.json();
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

    let expected = vec!["delete", "update", "create"];
    assert_eq!(
        board_link_actions(&server, &board, &link.id).await,
        expected
    );
    assert_eq!(
        board_link_actions(&server, &other, &link.id).await,
        expected
    );

    for card in [&a, &far[0]] {
        let hist: Vec<shared::AuditLogEntry> = server
            .get(&format!("/api/cards/{}/history", card.id))
            .await
            .json();
        let actions: Vec<_> = hist
            .iter()
            .filter(|e| e.entity_type == "card_link" && e.entity_id == link.id)
            .map(|e| e.action.as_str())
            .collect();
        assert_eq!(actions, expected, "history of card #{}", card.number);
        for row in hist.iter().filter(|e| e.entity_type == "card_link") {
            for snapshot in [&row.snapshot_before, &row.snapshot_after]
                .into_iter()
                .flatten()
            {
                assert!(snapshot.get("predecessor_board_slug").is_none());
                assert!(snapshot.get("successor_board_slug").is_none());
                // The numbers the summary renders are still there.
                assert_eq!(snapshot["successor_number"], far[0].number);
            }
        }
    }
}

/// Every link event reaches both boards' subscribers: each event goes out once
/// per board, tagged with that board's id. A same-board link's events go out
/// exactly once.
#[tokio::test]
async fn cross_board_link_events_reach_both_boards() {
    use crate::events::{BoardEvent, BroadcastEvent};
    use tokio::sync::broadcast::{Receiver, error::TryRecvError};

    /// The board ids of every link event waiting in `rx`, in order, tagged
    /// with the event's kind. Audit events are skipped.
    fn drain(rx: &mut Receiver<BroadcastEvent>) -> Vec<(&'static str, String)> {
        let mut seen = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    let kind = match msg.event {
                        BoardEvent::CardLinkCreated { .. } => "created",
                        BoardEvent::CardLinkUpdated { .. } => "updated",
                        BoardEvent::CardLinkDeleted { .. } => "deleted",
                        _ => continue,
                    };
                    seen.push((kind, msg.board_id));
                }
                Err(TryRecvError::Empty) => return seen,
                Err(other) => panic!("broadcast channel: {other}"),
            }
        }
    }

    let db = db::connect_test().await.expect("failed to connect test db");
    let state = AppState::new(db);
    let mut rx = state.events.subscribe();
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();

    let (board, _, [a, b, _]) = setup_three_cards(&server).await;
    let (other, far) = setup_other_board(&server, "other-board", &["# Elsewhere"]).await;
    drain(&mut rx);

    // Each request's events are sent before its response returns, so
    // `try_recv` after the `.await` sees all of them.
    let link: shared::CardLink = link_after(&server, &a, &far[0], None).await.json();
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
    let both = |kind| vec![(kind, board.id.clone()), (kind, other.id.clone())];
    let expected: Vec<_> = ["created", "updated", "deleted"]
        .into_iter()
        .flat_map(both)
        .collect();
    assert_eq!(drain(&mut rx), expected);

    // A same-board link is announced once, not twice to the same board.
    link_after(&server, &a, &b, None)
        .await
        .assert_status(StatusCode::CREATED);
    assert_eq!(drain(&mut rx), vec![("created", board.id.clone())]);
}

/// Deleting the far board removes its cross-board links and says so on the
/// near board: the link leaves the near board's list and a delete row lands
/// in its history.
#[tokio::test]
async fn deleting_the_other_board_removes_cross_board_links_from_this_one() {
    let server = test_app().await;
    let (board, _, [a, _, _]) = setup_three_cards(&server).await;
    let (other, far) = setup_other_board(&server, "other-board", &["# Elsewhere"]).await;
    let link: shared::CardLink = link_after(&server, &a, &far[0], None).await.json();

    server
        .delete(&format!("/api/boards/{}", other.name))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    assert!(board_links(&server, &board).await.is_empty());
    assert_eq!(
        board_link_actions(&server, &board, &link.id).await,
        vec!["delete", "create"]
    );
    // The surviving card's own history records the loss too.
    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", a.id))
        .await
        .json();
    assert!(
        hist.iter()
            .any(|e| e.entity_id == link.id && e.action == "delete")
    );
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

/// A cascade on the far board tags the near board's link delete row with the
/// cascade's own batch group, so the near board's history can say which
/// cascade removed the link. A column delete rather than a board delete, so
/// the far board — and its history — is still there to compare against.
#[tokio::test]
async fn a_far_cascade_groups_the_near_boards_link_row_with_it() {
    let server = test_app().await;
    let (board, _, [a, _, _]) = setup_three_cards(&server).await;
    let (other, far) = setup_other_board(&server, "other-board", &["# Elsewhere"]).await;
    let link: shared::CardLink = link_after(&server, &a, &far[0], None).await.json();

    server
        .delete(&format!("/api/columns/{}", far[0].column_id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let history = |slug: String| {
        let server = &server;
        async move {
            server
                .get(&format!("/api/boards/{slug}/history"))
                .await
                .json::<Vec<shared::AuditLogEntry>>()
        }
    };
    let far_hist = history(other.name.clone()).await;
    let column_group = far_hist
        .iter()
        .find(|e| e.entity_type == "column" && e.action == "delete")
        .and_then(|e| e.batch_group.clone())
        .expect("the column delete is grouped");
    let near_hist = history(board.name.clone()).await;
    let near_delete = near_hist
        .iter()
        .find(|e| e.entity_id == link.id && e.action == "delete")
        .expect("the near board records the link's removal");
    assert_eq!(
        near_delete.batch_group.as_deref(),
        Some(column_group.as_str())
    );
    assert!(board_links(&server, &board).await.is_empty());
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
