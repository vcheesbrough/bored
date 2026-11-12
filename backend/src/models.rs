//! Database row types (`Db*`) and their conversion to the public API types in
//! `shared`.
//!
//! Each `Db*` struct is one row as Postgres returns it: `#[derive(FromRow)]`
//! maps columns to fields by name, so a query reads `SELECT * FROM cards` (or
//! names its columns) and gets a `DbCard` back.
//!
//! Every `into_api()` here is a boundary: nothing storage-specific may cross it
//! (card #472). Ids are already the bare ULIDs the API uses, and timestamps go
//! out through [`api_timestamp`]. The contract tests in `tests/contract.rs`
//! hold both rules for every route and SSE event.

use chrono::{DateTime, Utc};
use sqlx::types::Json;

use crate::timestamp::api_timestamp;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbBoard {
    pub id: String,
    pub name: String,
    pub last_edited_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DbBoard {
    pub fn into_api(self) -> shared::Board {
        shared::Board {
            id: self.id,
            name: self.name,
            last_edited_by: self.last_edited_by,
            created_at: api_timestamp(&self.created_at),
            updated_at: api_timestamp(&self.updated_at),
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbColumn {
    pub id: String,
    pub board_id: String,
    pub name: String,
    pub position: i32,
    pub last_edited_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DbColumn {
    pub fn into_api(self) -> shared::Column {
        shared::Column {
            id: self.id,
            board_id: self.board_id,
            name: self.name,
            position: self.position,
            last_edited_by: self.last_edited_by,
            created_at: api_timestamp(&self.created_at),
            updated_at: api_timestamp(&self.updated_at),
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbCard {
    pub id: String,
    pub column_id: String,
    pub body: String,
    pub position: i32,
    /// Assigned from `card_number_seq` on insert, so never absent. Card
    /// numbers start at 1, which `into_api`'s `u32` relies on.
    pub number: i32,
    /// `text[]`, `NOT NULL DEFAULT '{}'` — a card with no tags has an empty
    /// list, never a NULL.
    pub tags: Vec<String>,
    pub last_edited_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DbCard {
    pub fn into_api(self) -> shared::Card {
        shared::Card {
            id: self.id,
            column_id: self.column_id,
            body: self.body,
            position: self.position,
            // `try_from` rather than `as`: a negative number cannot come from
            // the sequence, but if one were ever written by hand it should
            // read as the API's "no number" (0), not wrap to ~4 billion.
            number: u32::try_from(self.number).unwrap_or(0),
            tags: self.tags,
            last_edited_by: self.last_edited_by,
            created_at: api_timestamp(&self.created_at),
            updated_at: api_timestamp(&self.updated_at),
        }
    }
}

/// One row of `card_links`, as read back by [`crate::routes::links`].
///
/// The `*_number`, `*_board_id` and `*_board_slug` fields are not stored on
/// the row — they are joined in from the linked cards (and their columns and
/// boards) at read time (`links::link_select!`), so every query that loads a
/// link has to use that select rather than a bare `SELECT *`.
///
/// Card numbers never change, and a card can never move to another board
/// (`update_card` / `move_card` refuse a cross-board column), so the numbers
/// and board ids are stable for the life of the link. Only a slug can change
/// — when its board is renamed — and the join always reads the current one.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbCardLink {
    pub id: String,
    pub predecessor_id: String,
    pub successor_id: String,
    pub predecessor_number: i32,
    pub successor_number: i32,
    /// Board the predecessor card sits on (card #544: links may cross boards).
    pub predecessor_board_id: String,
    /// Board the successor card sits on.
    pub successor_board_id: String,
    pub predecessor_board_slug: String,
    pub successor_board_slug: String,
    pub reason: Option<String>,
    pub last_edited_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DbCardLink {
    /// The distinct boards this link touches: one for a same-board link, two
    /// for a cross-board one. Every audit row and SSE event about the link is
    /// written once per board in this list, so both boards' history and live
    /// views see it.
    pub fn boards(&self) -> Vec<String> {
        let mut boards = vec![self.predecessor_board_id.clone()];
        if self.successor_board_id != self.predecessor_board_id {
            boards.push(self.successor_board_id.clone());
        }
        boards
    }

    /// The API shape of this link. This is the **only** place a
    /// `shared::CardLink` is built from a row, for both HTTP responses and
    /// SSE events — so when boards become private (card #12) per-viewer
    /// redaction of the far board has exactly one place to go.
    pub fn into_api(self) -> shared::CardLink {
        shared::CardLink {
            id: self.id,
            predecessor_id: self.predecessor_id,
            successor_id: self.successor_id,
            predecessor_number: u32::try_from(self.predecessor_number).unwrap_or(0),
            successor_number: u32::try_from(self.successor_number).unwrap_or(0),
            predecessor_board_slug: self.predecessor_board_slug,
            successor_board_slug: self.successor_board_slug,
            reason: self.reason,
            last_edited_by: self.last_edited_by,
            created_at: api_timestamp(&self.created_at),
            updated_at: api_timestamp(&self.updated_at),
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbAuditLog {
    pub id: String,
    pub actor_sub: String,
    pub actor_display_name: String,
    pub entity_type: String,
    pub entity_id: String,
    pub board_id: String,
    pub action: String,
    /// `jsonb`. sqlx's `Json<T>` wrapper is what decodes a `jsonb` column into
    /// a serde type; `.0` is the value inside.
    pub snapshot_before: Option<Json<serde_json::Value>>,
    pub snapshot_after: Option<Json<serde_json::Value>>,
    pub restored_from: Option<String>,
    pub batch_group: Option<String>,
    pub audit_edit_session: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl DbAuditLog {
    pub fn into_api(self) -> shared::AuditLogEntry {
        shared::AuditLogEntry {
            id: self.id,
            created_at: api_timestamp(&self.created_at),
            actor_sub: self.actor_sub,
            actor_display_name: self.actor_display_name,
            entity_type: self.entity_type,
            entity_id: self.entity_id,
            board_id: self.board_id,
            action: self.action,
            snapshot_before: self.snapshot_before.map(|json| json.0),
            snapshot_after: self.snapshot_after.map(|json| json.0),
            restored_from: self.restored_from,
            batch_group: self.batch_group,
            audit_edit_session: self.audit_edit_session,
        }
    }
}
