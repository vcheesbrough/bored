//! Server-sent events emitted by board mutations.

use super::*;

/// Every mutation now records an `AuditAppended` event before the domain
/// `BoardEvent` — tests that care about the latter skip audit noise here.
async fn recv_next_non_audit_board_event(
    rx: &mut tokio::sync::broadcast::Receiver<crate::events::BroadcastEvent>,
) -> crate::events::BoardEvent {
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("SSE recv timed out")
            .expect("broadcast channel closed");
        match msg.event {
            crate::events::BoardEvent::AuditAppended { .. } => continue,
            other => return other,
        }
    }
}

// Verifies that mutation routes emit the expected SSE events. We subscribe
// to the broadcast channel before performing a mutation and check that the
// correct event arrives with the right payload.
#[tokio::test]
async fn mutations_emit_sse_events() {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    // Subscribe *before* making requests so we don't miss any events.
    let mut rx = state.events.subscribe();

    let server = TestServer::new(app(state, "./dist", "dev").await).unwrap();

    // CREATE board → BoardCreated
    let board: shared::Board = server
        .post("/api/boards")
        .json(&shared::CreateBoardRequest {
            name: "event-board".to_string(),
        })
        .await
        .json();

    // Use a bounded async wait instead of try_recv so the test doesn't race
    // the handler. The send always happens before the HTTP response returns,
    // but relying on try_recv returning Ok rather than Empty is fragile under
    // a busy executor. 1 s is generous — in practice the channel is ready
    // in microseconds.
    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::BoardCreated { .. }));

    // CREATE column → ColumnCreated
    let col: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Col".to_string(),
            position: 0,
        })
        .await
        .json();

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::ColumnCreated { .. }));

    // Create a second column so there is somewhere to move the card to.
    let other_col: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Other Col".to_string(),
            position: 1,
        })
        .await
        .json();

    // Drain audit + ColumnCreated for other_col so it doesn't interfere.
    let _ = recv_next_non_audit_board_event(&mut rx).await;

    // CREATE card → CardCreated
    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", col.id))
        .json(&shared::CreateCardRequest {
            body: "hello".to_string(),
            ..Default::default()
        })
        .await
        .json();

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::CardCreated { .. }));

    server
        .post(&format!("/api/cards/{}/move", card.id))
        .json(&shared::MoveCardRequest {
            column_id: other_col.id.clone(),
            position: 0,
        })
        .await
        .assert_status_ok();

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::CardMoved { .. }));

    // UPDATE card → CardUpdated
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("updated body".to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::CardUpdated { .. }));

    // DELETE card → CardDeleted
    server
        .delete(&format!("/api/cards/{}", card.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::CardDeleted { .. }));

    // UPDATE column → ColumnUpdated
    server
        .put(&format!("/api/columns/{}", col.id))
        .json(&shared::UpdateColumnRequest {
            name: Some("Renamed".to_string()),
            position: None,
        })
        .await
        .assert_status_ok();

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::ColumnUpdated { .. }));

    // REORDER columns → ColumnsReordered
    let cols: Vec<shared::Column> = server
        .get(&format!("/api/boards/{}/columns", board.name))
        .await
        .json();
    let order: Vec<String> = cols.iter().rev().map(|c| c.id.clone()).collect();
    server
        .put(&format!("/api/boards/{}/columns/reorder", board.name))
        .json(&shared::ColumnsReorderRequest { order })
        .await
        .assert_status_ok();

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::ColumnsReordered { .. }));

    // DELETE column → ColumnDeleted
    server
        .delete(&format!("/api/columns/{}", col.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::ColumnDeleted { .. }));

    // UPDATE board → BoardUpdated
    server
        .put(&format!("/api/boards/{}", board.name))
        .json(&shared::UpdateBoardRequest {
            name: "renamed-board".to_string(),
        })
        .await
        .assert_status_ok();

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::BoardUpdated { .. }));

    // DELETE board → BoardDeleted (use the updated name from the rename above)
    server
        .delete("/api/boards/renamed-board")
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let event = recv_next_non_audit_board_event(&mut rx).await;
    assert!(matches!(event, events::BoardEvent::BoardDeleted { .. }));
}

/// Drain every event already in the channel, keeping the `CardMoved` ones.
///
/// Each request's events are sent before its response returns, so once a
/// request has completed everything it announced is already queued.
fn drain_card_moves(
    rx: &mut tokio::sync::broadcast::Receiver<crate::events::BroadcastEvent>,
) -> Vec<(shared::Card, String)> {
    let mut moves = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let crate::events::BoardEvent::CardMoved {
            card,
            from_column_id,
        } = msg.event
        {
            moves.push((card, from_column_id));
        }
    }
    moves
}

/// `GET` a column's cards, top first — what a browser fetches on load.
///
/// A plain `async fn` rather than a closure: a closure returning an `async`
/// block cannot tie the block's borrow of `server` to its argument's lifetime.
async fn list_column(server: &TestServer, column_id: &str) -> Vec<shared::Card> {
    server
        .get(&format!("/api/columns/{column_id}/cards"))
        .await
        .json()
}

/// Card #393: a column rebalance must announce every card it renumbers.
///
/// Repeated moves to the top bisect the gap above the first card until it is
/// used up (~10 moves), and the server then renumbers the whole column. A
/// browser holds the other cards' old positions and slots the moved card by
/// comparing against them, so a silent renumber puts "Move to top" somewhere
/// else until a reload.
///
/// This plays the part of that browser: it starts from the list it would have
/// fetched and applies nothing but the broadcast `CardMoved` events. After
/// every move its positions must be the server's own, as `GET` reports them —
/// an oracle independent of the rebalance arithmetic.
#[tokio::test]
async fn a_rebalance_announces_every_renumbered_card() {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    let mut rx = state.events.subscribe();
    let server = TestServer::new(app(state, "./dist", "dev").await).unwrap();
    let (_, column) = setup_board_and_column(&server).await;

    for body in ["one", "two", "three"] {
        server
            .post(&format!("/api/columns/{}/cards", column.id))
            .json(&shared::CreateCardRequest {
                body: body.to_string(),
                ..Default::default()
            })
            .await
            .assert_status(StatusCode::CREATED);
    }

    // The "browser": card id → position, seeded from a fetch.
    let mut client: std::collections::HashMap<String, i32> = list_column(&server, &column.id)
        .await
        .into_iter()
        .map(|c| (c.id, c.position))
        .collect();
    let _ = drain_card_moves(&mut rx);

    // Enough top moves to force at least one rebalance; see
    // `position::tests::bisecting_the_same_slot_survives_about_ten_inserts`.
    let mut renumbered_any = false;
    for _ in 0..24 {
        // Always move the current bottom card, so every move changes the order.
        let bottom = list_column(&server, &column.id)
            .await
            .pop()
            .expect("column has cards");
        server
            .post(&format!("/api/cards/{}/move", bottom.id))
            .json(&shared::MoveCardRequest {
                column_id: column.id.clone(),
                position: 0,
            })
            .await
            .assert_status_ok();

        let moves = drain_card_moves(&mut rx);
        // A move that announces a card other than the moved one is a rebalance.
        renumbered_any |= moves.iter().any(|(card, _)| card.id != bottom.id);
        for (card, from_column_id) in moves {
            assert_eq!(from_column_id, column.id, "a rebalance stays in its column");
            client.insert(card.id, card.position);
        }

        let server_view: std::collections::HashMap<String, i32> = list_column(&server, &column.id)
            .await
            .into_iter()
            .map(|c| (c.id, c.position))
            .collect();
        assert_eq!(client, server_view, "SSE-only view drifted from the server");
    }
    assert!(renumbered_any, "the loop never forced a rebalance");
}

/// Card #393, create path: a new card at the top can force the rebalance too.
#[tokio::test]
async fn a_rebalance_on_create_announces_the_renumbered_cards() {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    let mut rx = state.events.subscribe();
    let server = TestServer::new(app(state, "./dist", "dev").await).unwrap();
    let (_, column) = setup_board_and_column(&server).await;
    let url = format!("/api/columns/{}/cards", column.id);

    let mut renumbered_any = false;
    for i in 0..16 {
        let before: Vec<shared::Card> = server.get(&url).await.json();
        server
            .post(&url)
            .json(&shared::CreateCardRequest {
                body: format!("card {i}"),
                ..Default::default()
            })
            .await
            .assert_status(StatusCode::CREATED);

        // Every existing card whose position the create changed must have been
        // announced, and with its new value.
        let after: Vec<shared::Card> = server.get(&url).await.json();
        let announced: std::collections::HashMap<String, i32> = drain_card_moves(&mut rx)
            .into_iter()
            .map(|(card, _)| (card.id, card.position))
            .collect();
        for old in &before {
            let new = after
                .iter()
                .find(|c| c.id == old.id)
                .expect("card survives");
            if new.position != old.position {
                renumbered_any = true;
                assert_eq!(
                    announced.get(&new.id),
                    Some(&new.position),
                    "renumbered card {} was not announced",
                    new.id
                );
            }
        }
    }
    assert!(renumbered_any, "the loop never forced a rebalance");
}
