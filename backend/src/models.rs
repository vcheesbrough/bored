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
/// The two `*_number` fields are not stored on the row — they are joined in
/// from the linked cards at read time (`links::link_select!`), so every query
/// that loads a link has to use that select rather than a bare `SELECT *`.
/// Card numbers never change, so the join is stable for the life of the link.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DbCardLink {
    pub id: String,
    pub predecessor_id: String,
    pub successor_id: String,
    pub predecessor_number: i32,
    pub successor_number: i32,
    pub reason: Option<String>,
    pub last_edited_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl DbCardLink {
    pub fn into_api(self) -> shared::CardLink {
        shared::CardLink {
            id: self.id,
            predecessor_id: self.predecessor_id,
            successor_id: self.successor_id,
            predecessor_number: u32::try_from(self.predecessor_number).unwrap_or(0),
            successor_number: u32::try_from(self.successor_number).unwrap_or(0),
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
