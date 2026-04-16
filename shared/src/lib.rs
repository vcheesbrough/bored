use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub id: String,
    pub board_id: String,
    pub name: String,
    pub position: i32,
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
    pub title: String,
    pub description: Option<String>,
    pub position: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCardRequest {
    pub title: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateCardRequest {
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub position: Option<i32>,
    pub column_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveCardRequest {
    pub column_id: String,
    pub position: i32,
}
