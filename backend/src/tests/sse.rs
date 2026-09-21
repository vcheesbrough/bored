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

/// Every position change a browser would learn from what is queued in `rx`.
struct PositionNews {
    /// `(card id, new position)`, in the order announced.
    updates: Vec<(String, i32)>,
    /// How many `CardsRenumbered` events were in there.
    renumberings: usize,
    /// How many events of any kind were queued — the burst one request put
    /// into the shared broadcast channel.
    events: usize,
}

/// Drain everything already in the channel. Each request's events are sent
/// before its response returns, so once a request has completed everything it
/// announced is already queued.
///
/// Panics on `Lagged`: a receiver that fell behind lost events without being
/// told which, which is exactly the failure these tests exist to rule out.
fn drain_positions(
    rx: &mut tokio::sync::broadcast::Receiver<crate::events::BroadcastEvent>,
    column_id: &str,
) -> PositionNews {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut news = PositionNews {
        updates: Vec::new(),
        renumberings: 0,
        events: 0,
    };
    loop {
        let msg = match rx.try_recv() {
            Ok(msg) => msg,
            Err(TryRecvError::Empty) => return news,
            Err(TryRecvError::Lagged(n)) => panic!("receiver lagged; {n} events dropped"),
            Err(TryRecvError::Closed) => panic!("broadcast channel closed"),
        };
        news.events += 1;
        match msg.event {
            crate::events::BoardEvent::CardMoved {
                card,
                from_column_id,
            } => {
                assert_eq!(from_column_id, column_id, "moves stay in this column");
                news.updates.push((card.id, card.position));
            }
            crate::events::BoardEvent::CardsRenumbered {
                column_id: renumbered,
                positions,
            } => {
                assert_eq!(renumbered, column_id, "a renumbering stays in its column");
                news.renumberings += 1;
                news.updates
                    .extend(positions.into_iter().map(|p| (p.id, p.position)));
            }
            _ => {}
        }
    }
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

/// A column's cards as `id → position`, as the server has them.
async fn server_positions(
    server: &TestServer,
    column_id: &str,
) -> std::collections::HashMap<String, i32> {
    list_column(server, column_id)
        .await
        .into_iter()
        .map(|c| (c.id, c.position))
        .collect()
}

/// Build a server on a fresh in-memory database, keeping a handle on its event
/// channel so receivers can subscribe at any point.
async fn server_with_events() -> (
    TestServer,
    tokio::sync::broadcast::Sender<crate::events::BroadcastEvent>,
) {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db);
    // `Sender` is a cheap handle onto the same channel; the router takes
    // ownership of `state`, so keep our own copy of the sender first.
    let events = state.events.clone();
    let server = TestServer::new(app(state, "./dist", "dev").await).unwrap();
    (server, events)
}

async fn create_cards(server: &TestServer, column_id: &str, count: usize) {
    for i in 0..count {
        server
            .post(&format!("/api/columns/{column_id}/cards"))
            .json(&shared::CreateCardRequest {
                body: format!("card {i}"),
                ..Default::default()
            })
            .await
            .assert_status(StatusCode::CREATED);
    }
}

/// Move the column's bottom card to the top, `times` times, playing the part
/// of a browser throughout: it starts from the list it would have fetched and
/// learns of changes only from the broadcast events. After every move its
/// positions must be the server's own, as `GET` reports them — an oracle
/// independent of the rebalance arithmetic. Returns how many renumberings were
/// announced, and the largest burst any one move put into the channel.
async fn move_bottom_to_top(
    server: &TestServer,
    events: &tokio::sync::broadcast::Sender<crate::events::BroadcastEvent>,
    column_id: &str,
    times: usize,
) -> (usize, usize) {
    let mut rx = events.subscribe();
    let mut client = server_positions(server, column_id).await;
    let mut renumberings = 0;
    let mut largest_burst = 0;
    for _ in 0..times {
        // Always the current bottom card, so every move changes the order.
        let bottom = list_column(server, column_id)
            .await
            .pop()
            .expect("column has cards");
        server
            .post(&format!("/api/cards/{}/move", bottom.id))
            .json(&shared::MoveCardRequest {
                column_id: column_id.to_string(),
                position: 0,
            })
            .await
            .assert_status_ok();

        let news = drain_positions(&mut rx, column_id);
        renumberings += news.renumberings;
        largest_burst = largest_burst.max(news.events);
        client.extend(news.updates);
        assert_eq!(
            client,
            server_positions(server, column_id).await,
            "SSE-only view drifted from the server"
        );
    }
    (renumberings, largest_burst)
}

/// Card #393: a column rebalance must announce the positions it rewrites.
///
/// Repeated moves to the top bisect the gap above the first card until it is
/// used up (~10 moves), and the server then renumbers the whole column. A
/// browser holds the other cards' old positions and slots the moved card by
/// comparing against them, so a silent renumber puts "Move to top" somewhere
/// else until a reload.
#[tokio::test]
async fn a_rebalance_announces_every_renumbered_card() {
    let (server, events) = server_with_events().await;
    let (_, column) = setup_board_and_column(&server).await;
    create_cards(&server, &column.id, 3).await;

    // Enough top moves to force at least one rebalance; see
    // `position::tests::bisecting_the_same_slot_survives_about_ten_inserts`.
    let (renumberings, _) = move_bottom_to_top(&server, &events, &column.id, 24).await;
    assert!(renumberings > 0, "the loop never forced a rebalance");
}

/// The renumbering of a large column is one event, not one per card.
///
/// The broadcast channel holds `BROADCAST_CAPACITY` events and is shared by
/// every board. Announcing a big column card by card overflowed it, and a
/// lagging receiver drops the oldest events without a trace — the very
/// renumbers this fix exists to deliver. `drain_positions` panics on `Lagged`,
/// and the burst from any one move is bounded well below the capacity.
#[tokio::test]
async fn a_large_column_rebalance_fits_in_the_broadcast_channel() {
    let (server, events) = server_with_events().await;
    let (_, column) = setup_board_and_column(&server).await;
    // Comfortably more cards than the channel has slots.
    let cards = crate::events::BROADCAST_CAPACITY + 72;
    create_cards(&server, &column.id, cards).await;

    let (renumberings, largest_burst) = move_bottom_to_top(&server, &events, &column.id, 24).await;
    assert!(renumberings > 0, "the loop never forced a rebalance");
    // Audit row, renumbering, the move itself — and nothing that grows with
    // the column.
    assert!(
        largest_burst <= 4,
        "one move put {largest_burst} events into the channel"
    );
}

/// Card #393, create path: a new card at the top can force the rebalance too,
/// and every existing card whose position it changed must be announced with
/// its new value.
#[tokio::test]
async fn a_rebalance_on_create_announces_the_renumbered_cards() {
    let (server, events) = server_with_events().await;
    let (_, column) = setup_board_and_column(&server).await;
    let mut rx = events.subscribe();

    let mut renumbered_any = false;
    for _ in 0..16 {
        let before = server_positions(&server, &column.id).await;
        create_cards(&server, &column.id, 1).await;
        let after = server_positions(&server, &column.id).await;

        let announced: std::collections::HashMap<String, i32> =
            drain_positions(&mut rx, &column.id)
                .updates
                .into_iter()
                .collect();
        for (id, old) in &before {
            let new = after[id];
            if new != *old {
                renumbered_any = true;
                assert_eq!(
                    announced.get(id),
                    Some(&new),
                    "renumbered card {id} was not announced"
                );
            }
        }
    }
    assert!(renumbered_any, "the loop never forced a rebalance");
}
