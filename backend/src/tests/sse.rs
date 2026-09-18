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
