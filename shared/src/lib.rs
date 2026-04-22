use serde::{Deserialize, Serialize};

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
    pub last_edited_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCardRequest {
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCardRequest {
    pub body: Option<String>,
    pub position: Option<i32>,
    pub column_id: Option<String>,
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
