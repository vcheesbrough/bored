use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

pub mod history;
pub mod links;
pub mod tags;

/// Deployed application version.
///
/// The release pipeline passes `RELEASE_TAG` as a build arg and exports it as an
/// environment variable for the compile, so `option_env!` captures the exact
/// semver that was tagged in git and pushed to the registry. Local/dev builds
/// (no `RELEASE_TAG`) fall back to the crate version.
pub fn app_version() -> &'static str {
    match option_env!("RELEASE_TAG") {
        Some(tag) if !tag.is_empty() => tag,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub id: String,
    pub name: String,
    pub last_edited_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub id: String,
    pub board_id: String,
    pub name: String,
    pub position: i32,
    pub last_edited_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBoardRequest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateBoardRequest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateColumnRequest {
    pub name: String,
    pub position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateColumnRequest {
    pub name: Option<String>,
    pub position: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    pub column_id: String,
    pub body: String,
    pub position: i32,
    pub number: u32,
    /// Free-form labels attached to this card. Always present in API output —
    /// `#[serde(default)]` covers rows written before tags existed, which
    /// deserialize as an empty list rather than failing.
    #[serde(default)]
    pub tags: Vec<String>,
    pub last_edited_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreateCardRequest {
    pub body: String,
    /// Optional tags for the new card; omitted means "no tags".
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateCardRequest {
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub position: Option<i32>,
    #[serde(default)]
    pub column_id: Option<String>,
    /// Full replacement for the card's tag list. `None` leaves tags untouched;
    /// `Some(list)` replaces them wholesale (there is no add/remove verb — the
    /// client always sends the complete set it wants the card to end up with).
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Client-generated id for one uninterrupted editing stretch; repeated
    /// body saves with the same token merge into a single audit row.
    #[serde(default)]
    pub audit_edit_session: Option<String>,
}

/// One predecessor/successor link between two cards on the same board.
///
/// A link is a single bidirectional fact — it is created from either end,
/// visible and editable from both, and the two `*_id` fields say which card
/// comes first. The card numbers ride along so the browser can label a link
/// (`#12 → #15`) without looking both cards up, and so an audit snapshot of the
/// link stays readable after either card is gone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CardLink {
    pub id: String,
    /// The card that comes first.
    pub predecessor_id: String,
    /// The card that comes after.
    pub successor_id: String,
    /// Human-readable number of `predecessor_id`.
    pub predecessor_number: u32,
    /// Human-readable number of `successor_id`.
    pub successor_number: u32,
    /// Optional note on *why* one card precedes the other. Never an empty
    /// string — see [`links::normalize_reason`].
    #[serde(default)]
    pub reason: Option<String>,
    pub last_edited_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl CardLink {
    /// True when `card_id` is either end of this link.
    pub fn touches(&self, card_id: &str) -> bool {
        self.predecessor_id == card_id || self.successor_id == card_id
    }
}

/// Which end of a new link the *other* card takes, relative to the card the
/// request is addressed to (`POST /api/cards/:id/links`).
///
/// `Predecessor` means "the other card comes before this one"; `Successor`
/// means "the other card comes after this one". Both produce the same kind of
/// row — the direction only decides which id lands in which column.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkDirection {
    Predecessor,
    Successor,
}

/// Body of `POST /api/cards/:id/links`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCardLinkRequest {
    /// Role of `other_card_id` relative to the card in the URL.
    pub direction: LinkDirection,
    /// The card at the other end of the link. Must be on the same board and
    /// must not be the card in the URL.
    pub other_card_id: String,
    /// Optional reason; trimmed, and an empty reason is stored as none.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Body of `PUT /api/links/:id`. Only the reason is editable — changing an
/// end of a link is a delete plus a create, so the history stays honest about
/// which two cards were linked when.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateCardLinkRequest {
    /// Full replacement for the reason. `None` or an empty string clears it.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveCardRequest {
    pub column_id: String,
    pub position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub version: String,
    pub env: String,
}

/// Public-facing user identity returned by `GET /api/me`.
/// Trimmed projection of the JWT claims — the navbar only needs these three fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    /// Display name (from the `preferred_username` claim).
    pub name: String,
    /// Email address (from the `email` claim) — used for Gravatar fallback.
    pub email: Option<String>,
    /// Avatar URL (from the `picture` claim) when the IdP provides one.
    pub picture: Option<String>,
}

/// Body of `PUT /api/boards/:id/columns/reorder`.
/// The server assigns `position = index` for each column ID in the list,
/// allowing the client to express a complete ordering in one round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnsReorderRequest {
    /// Full ordered list of column IDs for the board. Every column must be
    /// present; missing IDs are silently skipped (no partial reorder).
    pub order: Vec<String>,
}

/// Body of `PUT /api/columns/:id/cards/reorder`.
/// The complete desired top-to-bottom order of one column's cards; the server
/// applies it and knows nothing about *why* that order was chosen.
///
/// Unlike [`ColumnsReorderRequest`] the contract is deliberately tolerant:
/// ids that are not in the column (including ids from another column) are
/// ignored, and cards the client did not mention keep their relative order at
/// the **end** of the column. Cards are created at the top, so a card added
/// between the client reading the column and sending this request would
/// otherwise turn a reorder into an error; instead it simply sinks to the
/// bottom and the user can move it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardsReorderRequest {
    /// Desired order of the column's card IDs, top first.
    pub order: Vec<String>,
}

/// One append-only row from `audit_log` — returned by history endpoints and
/// pushed over SSE as `audit_appended`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditLogEntry {
    pub id: String,
    pub created_at: String,
    pub actor_sub: String,
    pub actor_display_name: String,
    /// `"board"` | `"column"` | `"card"` | `"card_link"`
    pub entity_type: String,
    pub entity_id: String,
    /// Denormalised board ULID every mutation touches — scopes SSE + queries.
    pub board_id: String,
    /// `"create"` | `"update"` | `"delete"` | `"move"` | `"restore"`
    pub action: String,
    pub snapshot_before: Option<JsonValue>,
    pub snapshot_after: Option<JsonValue>,
    pub restored_from: Option<String>,
    pub batch_group: Option<String>,
    #[serde(default)]
    pub audit_edit_session: Option<String>,
}

impl AuditLogEntry {
    fn snapshot_column_id(snap: &Option<JsonValue>) -> Option<&str> {
        snap.as_ref()?.get("column_id")?.as_str()
    }

    /// Rows relevant when filtering board audit history down to one column (matches snapshots).
    pub fn matches_history_column_scope(&self, column_id: &str) -> bool {
        if self.entity_type == "column" && self.entity_id == column_id {
            return true;
        }
        self.entity_type == "card"
            && (Self::snapshot_column_id(&self.snapshot_before) == Some(column_id)
                || Self::snapshot_column_id(&self.snapshot_after) == Some(column_id))
    }

    /// Rows relevant to one card's history: the card's own rows, plus every
    /// link row that has the card at either end. A link belongs to two cards,
    /// so it shows up in both histories.
    pub fn matches_history_card_scope(&self, card_id: &str) -> bool {
        if self.entity_type == "card" {
            return self.entity_id == card_id;
        }
        self.entity_type == "card_link"
            && (Self::snapshot_touches_card(&self.snapshot_before, card_id)
                || Self::snapshot_touches_card(&self.snapshot_after, card_id))
    }

    /// True when a `card_link` snapshot names `card_id` as either end.
    fn snapshot_touches_card(snap: &Option<JsonValue>, card_id: &str) -> bool {
        let Some(snap) = snap.as_ref() else {
            return false;
        };
        ["predecessor_id", "successor_id"]
            .iter()
            .any(|key| snap.get(key).and_then(JsonValue::as_str) == Some(card_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(entity_type: &str, entity_id: &str, after: Option<JsonValue>) -> AuditLogEntry {
        AuditLogEntry {
            id: "audit-1".into(),
            created_at: "x".into(),
            actor_sub: "alice".into(),
            actor_display_name: "Alice".into(),
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            board_id: "board-1".into(),
            action: "create".into(),
            snapshot_before: None,
            snapshot_after: after,
            restored_from: None,
            batch_group: None,
            audit_edit_session: None,
        }
    }

    /// A link between two cards. Only the two `*_id` ends matter to
    /// [`CardLink::touches`]; everything else is filler.
    fn link(predecessor_id: &str, successor_id: &str) -> CardLink {
        CardLink {
            id: "link-1".into(),
            predecessor_id: predecessor_id.into(),
            successor_id: successor_id.into(),
            predecessor_number: 1,
            successor_number: 2,
            reason: None,
            last_edited_by: None,
            created_at: "x".into(),
            updated_at: "x".into(),
        }
    }

    #[test]
    fn touches_matches_the_predecessor_end() {
        assert!(link("a", "b").touches("a"));
    }

    #[test]
    fn touches_matches_the_successor_end() {
        assert!(link("a", "b").touches("b"));
    }

    #[test]
    fn touches_ignores_an_unrelated_card() {
        assert!(!link("a", "b").touches("c"));
    }

    #[test]
    fn touches_is_not_fooled_by_a_prefix() {
        // Ids are compared whole: `card-1` must not match `card-10`, or deleting
        // one card would prune another card's links.
        assert!(!link("card-10", "card-20").touches("card-1"));
    }

    #[test]
    fn touches_a_self_link() {
        // The backend rejects a self-link, but the predicate must not lean on
        // that: `a → a` is touched by `a`.
        assert!(link("a", "a").touches("a"));
    }

    #[test]
    fn card_scope_matches_the_cards_own_rows() {
        assert!(entry("card", "card-1", None).matches_history_card_scope("card-1"));
        assert!(!entry("card", "card-2", None).matches_history_card_scope("card-1"));
    }

    #[test]
    fn card_scope_matches_link_rows_at_either_end() {
        let link = json!({ "predecessor_id": "card-1", "successor_id": "card-2" });
        let row = entry("card_link", "link-1", Some(link));
        assert!(row.matches_history_card_scope("card-1"));
        assert!(row.matches_history_card_scope("card-2"));
        assert!(!row.matches_history_card_scope("card-3"));
    }

    #[test]
    fn card_scope_reads_a_delete_rows_before_snapshot() {
        let mut row = entry("card_link", "link-1", None);
        row.action = "delete".into();
        row.snapshot_before = Some(json!({ "predecessor_id": "card-1", "successor_id": "card-2" }));
        assert!(row.matches_history_card_scope("card-2"));
    }

    #[test]
    fn column_scope_ignores_link_rows() {
        let link = json!({ "predecessor_id": "card-1", "successor_id": "card-2" });
        assert!(!entry("card_link", "link-1", Some(link)).matches_history_column_scope("col-1"));
    }
}
