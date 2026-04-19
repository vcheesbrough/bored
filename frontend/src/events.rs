// Types shared across the frontend for real-time board updates and drag-and-drop.
//
// `BoardSseEvent` mirrors the backend `BoardEvent` enum — both use the same
// `#[serde(tag = "type", rename_all = "snake_case")]` encoding so the JSON
// produced by the backend deserializes here without any manual mapping.
//
// `DragPayload` is stored in a context `RwSignal` so any component in the
// tree can read the drag state without passing it down through props.

use serde::Deserialize;

/// A typed representation of every JSON event the backend pushes over SSE.
///
/// The backend serializes these as `{"type":"card_created","card":{...}}`.
/// The `#[serde(tag = "type")]` attribute tells serde to use the `"type"`
/// field as the discriminator, and `rename_all = "snake_case"` converts
/// `CardCreated` → `"card_created"` automatically.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BoardSseEvent {
    // ── Card events ──────────────────────────────────────────────────────
    CardCreated {
        card: shared::Card,
    },
    CardUpdated {
        card: shared::Card,
    },
    CardDeleted {
        card_id: String,
    },
    /// `from_column_id` is the column the card was in before the move.
    /// Having it avoids an extra API call on the receiving column.
    CardMoved {
        card: shared::Card,
        from_column_id: String,
    },

    // ── Column events ─────────────────────────────────────────────────────
    ColumnCreated {
        column: shared::Column,
    },
    ColumnUpdated {
        column: shared::Column,
    },
    ColumnDeleted {
        column_id: String,
    },
    /// Full ordered column list after a bulk reorder — receivers replace
    /// their column array rather than trying to patch individual positions.
    ColumnsReordered {
        columns: Vec<shared::Column>,
    },

    // ── Board events ──────────────────────────────────────────────────────
    // These variants are deserialized from SSE but not yet matched in the
    // frontend; the fields are retained for future use.
    #[allow(dead_code)]
    BoardCreated {
        board: shared::Board,
    },
    #[allow(dead_code)]
    BoardUpdated {
        board: shared::Board,
    },
    #[allow(dead_code)]
    BoardDeleted {
        board_id: String,
    },
}

/// What is currently being dragged — stored in a context `RwSignal` provided
/// by `BoardView` so every component in the tree can read it without prop
/// drilling, and event handlers can write to it without callbacks.
#[derive(Debug, Clone, PartialEq)]
pub enum DragPayload {
    /// Nothing is being dragged.
    None,
    /// A card is in flight. `from_column_id` records the column where the
    /// drag started so the drop handler knows which column to remove it from.
    Card {
        card_id: String,
        from_column_id: String,
    },
    /// A column header grip is being dragged for column reordering.
    Column { column_id: String },
}

/// Try to deserialize a raw SSE `data:` payload into a `BoardSseEvent`.
///
/// Returns `None` for keep-alive pings (`"ping"`) and any malformed JSON —
/// both are safe to ignore.
pub fn parse_sse_event(data: &str) -> Option<BoardSseEvent> {
    serde_json::from_str(data).ok()
}
