// Real-time event broadcasting over Server-Sent Events (SSE).
//
// Every mutation route calls `state.events.send(event)` after writing to the
// database. This module defines the event enum, the broadcast channel capacity,
// and the Axum handler that subscribes a client to the stream.
//
// The broadcast channel is a Tokio multi-producer, multi-consumer channel where
// every *active* receiver gets a copy of every message. Slow receivers that fall
// more than BROADCAST_CAPACITY events behind will receive a `Lagged` error on
// their next recv(); we treat that as a skip and continue (the client will
// reconcile on its next full-reload if necessary).

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;

use crate::routes::boards::AppState;

/// How many undelivered events a slow receiver can queue up before
/// the channel starts dropping events for that receiver.
pub const BROADCAST_CAPACITY: usize = 128;

/// Every mutation on a board, column, or card emits exactly one of these
/// variants. The `#[serde(tag = "type", rename_all = "snake_case")]` encoding
/// produces JSON like `{"type":"card_created","card":{...}}` which the
/// frontend parses with a `type` discriminator.
#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BoardEvent {
    // ── Card events ──────────────────────────────────────────────────────
    /// A card was created in a column. The full card is included so receivers
    /// can append it without an additional fetch.
    CardCreated { card: shared::Card },
    /// A card's body, position, or column was updated.
    CardUpdated { card: shared::Card },
    /// A card was hard-deleted.
    CardDeleted { card_id: String },
    /// A card was moved to a different column or position. `from_column_id`
    /// tells the source column to remove the card; `card.column_id` tells
    /// the destination column to insert it at `card.position`.
    CardMoved {
        card: shared::Card,
        from_column_id: String,
    },

    // ── Column events ─────────────────────────────────────────────────────
    /// A column was added to a board.
    ColumnCreated { column: shared::Column },
    /// A column's name or position was updated.
    ColumnUpdated { column: shared::Column },
    /// A column was deleted (along with all its cards).
    ColumnDeleted { column_id: String },
    /// The full reordered column list after a bulk reorder. Receivers replace
    /// their entire columns array with this list to stay in sync.
    ColumnsReordered { columns: Vec<shared::Column> },

    // ── Board events ──────────────────────────────────────────────────────
    /// A new board was created.
    BoardCreated { board: shared::Board },
    /// A board's name was updated.
    BoardUpdated { board: shared::Board },
    /// A board was deleted.
    BoardDeleted { board_id: String },
}

/// Wraps a `BoardEvent` with the ID of the board it originated from.
///
/// The broadcast channel carries these wrappers so the SSE handler can filter
/// to only the events that belong to the board the client subscribed to.
/// Without this, a client viewing board A would receive every event for every
/// board in the system — a data leak between unrelated boards.
#[derive(Clone)]
pub struct BroadcastEvent {
    /// ID of the board this event belongs to.
    pub board_id: String,
    /// The actual event payload.
    pub event: BoardEvent,
}

/// Query parameters accepted by `GET /api/events`.
#[derive(Deserialize)]
pub struct SseQuery {
    /// If present, the stream only delivers events for this board ID.
    ///
    /// Clients should always supply this to avoid receiving mutations for
    /// boards they are not currently viewing. The board ID comes from the
    /// URL of the board page (e.g. `/boards/:id`).
    board_id: Option<String>,
}

/// `GET /api/events` — subscribe to the board event stream.
///
/// Returns an SSE response that streams JSON-encoded `BoardEvent` payloads.
/// Keepalive pings are sent every 15 seconds so the connection stays open
/// through idle periods and through most load-balancer timeouts.
///
/// Connection lifecycle:
///   1. Client connects → we subscribe to the broadcast channel.
///   2. Every mutation fires a `send` on the channel → all subscribers receive it.
///   3. Client disconnects → Axum drops the stream → the `Receiver` is dropped,
///      freeing the slot in the broadcast channel automatically.
pub async fn sse_handler(
    State(state): State<AppState>,
    Query(query): Query<SseQuery>,
) -> Sse<impl futures_util::stream::Stream<Item = Result<Event, Infallible>>> {
    // `subscribe()` creates a new `Receiver` that will see all events sent
    // *after* this point. Events sent before this call are not replayed.
    let rx = state.events.subscribe();
    // Move the optional board filter into the stream combinator.
    let board_filter = query.board_id;

    // `BroadcastStream` converts the `Receiver` into a `Stream`. It yields
    // `Ok(T)` for each message and `Err(BroadcastStreamRecvError::Lagged(n))`
    // when the receiver fell behind and n messages were dropped.
    let stream = BroadcastStream::new(rx)
        // Skip lagged errors — the client's next full-page reload will reconcile.
        .filter_map(|result| result.ok())
        // Drop events that don't belong to the client's board. If no board_id
        // was supplied (e.g. an admin client), all events pass through.
        .filter(move |b| {
            board_filter
                .as_ref()
                .map_or(true, |bid| bid == &b.board_id)
        })
        // Serialize the inner event (not the wrapper) to JSON and wrap in an SSE `Event`.
        .map(|b| {
            let data = serde_json::to_string(&b.event)
                .unwrap_or_else(|_| r#"{"type":"error"}"#.to_string());
            Ok::<Event, Infallible>(Event::default().data(data))
        });

    Sse::new(stream).keep_alive(
        // Send a comment ": ping" every 15 seconds to prevent idle disconnects.
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
