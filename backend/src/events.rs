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
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::observability::metrics;
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
    /// A column was renumbered back onto the sparse position grid, because the
    /// gap at some slot was used up. The visible order is unchanged; only the
    /// stored `position` values are new. Receivers must adopt them, because
    /// they place every later `CardMoved` by comparing positions (card #393).
    ///
    /// One event for the whole column rather than a `CardMoved` per card: a
    /// renumbering touches nearly every card, and a large column's worth of
    /// individual events would overflow `BROADCAST_CAPACITY` — the channel is
    /// shared by every board — and be dropped, oldest first, without a trace.
    CardsRenumbered {
        column_id: String,
        positions: Vec<shared::CardPosition>,
    },

    // ── Card link events ─────────────────────────────────────────────────
    /// A predecessor/successor link was created between two cards. The full
    /// link is included so both cards' views can show it without a fetch.
    CardLinkCreated { link: shared::CardLink },
    /// A link's reason was changed.
    CardLinkUpdated { link: shared::CardLink },
    /// A link was removed — explicitly, or because one of its cards (or that
    /// card's column or board) was deleted.
    CardLinkDeleted { link_id: String },

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

    /// A new audit-log row was appended — drives the history drawer in real time.
    /// Boxed so the enum stays small for the broadcast channel (`clippy::large_enum_variant`).
    AuditAppended { entry: Box<shared::AuditLogEntry> },
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

/// One open SSE connection, for telemetry: its span, and its place in the
/// `bored.sse.subscribers` count.
///
/// Subscribe and unsubscribe are state changes worth a log line each (card
/// #415 §4); they are written inside the stream span so they carry its trace
/// id. The unsubscribe line and the counter decrement both happen in `Drop`,
/// which runs however the stream ends — the client closing the tab, a network
/// drop, or the server shutting down.
struct SseConnection {
    span: tracing::Span,
    /// Never read: held for its `Drop`, which decrements the counter.
    _subscription: metrics::SseSubscription,
}

impl SseConnection {
    fn open(span: tracing::Span) -> Self {
        span.in_scope(|| tracing::info!("sse subscribed"));
        Self {
            span,
            _subscription: metrics::SseSubscription::new(),
        }
    }
}

impl Drop for SseConnection {
    fn drop(&mut self) {
        self.span.in_scope(|| tracing::info!("sse unsubscribed"));
    }
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

    // The stream outlives this handler by as long as the tab stays open, so it
    // gets a span of its own: a child of the request span (whose trace it
    // belongs to), covering exactly the stream's life. The board id is a span
    // attribute — never a metric label. `Empty` would be wrong for "no
    // filter"; the field is simply left unset then.
    let stream_span = tracing::info_span!("sse stream", bored.board.id = board_filter.as_deref(),);
    // Counts this subscriber in `bored.sse.subscribers` until it is dropped,
    // and logs the unsubscribe then. Moved into the stream below, so it lives
    // exactly as long as the connection.
    let connection = SseConnection::open(stream_span.clone());

    // `BroadcastStream` converts the `Receiver` into a `Stream`. It yields
    // `Ok(T)` for each message and `Err(BroadcastStreamRecvError::Lagged(n))`
    // when the receiver fell behind and n messages were dropped.
    let stream = BroadcastStream::new(rx)
        // A lagged receiver skips what it missed — the client's next full-page
        // reload reconciles. The skip used to be silent; it is the event
        // channel's saturation signal, so it is now counted (the metric) and
        // said (a warning inside the stream's span, so it names the board).
        .filter_map(move |result| match result {
            Ok(event) => Some(event),
            Err(BroadcastStreamRecvError::Lagged(dropped)) => {
                metrics::sse_lagged(dropped);
                connection.span.in_scope(|| {
                    tracing::warn!(dropped, "sse subscriber lagged; events skipped");
                });
                None
            }
        })
        // Drop events that don't belong to the client's board. If no board_id
        // was supplied (e.g. an admin client), all events pass through.
        .filter(move |b| board_filter.as_ref().is_none_or(|bid| bid == &b.board_id))
        // Serialize the inner event (not the wrapper) to JSON and wrap in an SSE `Event`.
        .map(|b| {
            metrics::sse_event_delivered();
            let data = serde_json::to_string(&b.event)
                .unwrap_or_else(|_| r#"{"type":"error"}"#.to_string());
            Ok::<Event, Infallible>(Event::default().data(data))
        });
    // End the stream when the server starts shutting down (card #415). An SSE
    // stream never ends by itself, so without this every open tab would hold
    // the graceful drain for its whole timeout; ended, the response completes,
    // the connection closes, and the browser's EventSource reconnects to the
    // next container. `take_until` yields items until the future resolves,
    // then ends the stream — dropping `SseConnection` with it, which ends the
    // stream span and logs the unsubscribe before telemetry is flushed.
    let stream = futures_util::StreamExt::take_until(stream, state.draining.started());

    Sse::new(stream).keep_alive(
        // Send a comment ": ping" every 15 seconds to prevent idle disconnects.
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
