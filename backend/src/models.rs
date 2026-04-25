use serde::{Deserialize, Serialize};
use surrealdb::sql::Thing;

#[derive(Debug, Serialize, Deserialize)]
pub struct DbBoard {
    pub id: Thing,
    pub name: String,
    pub last_edited_by: Option<String>,
    pub created_at: surrealdb::sql::Datetime,
    pub updated_at: surrealdb::sql::Datetime,
}

impl DbBoard {
    pub fn into_api(self) -> shared::Board {
        shared::Board {
            id: self.id.id.to_raw(),
            name: self.name,
            last_edited_by: self.last_edited_by,
            created_at: self.created_at.to_string(),
            updated_at: self.updated_at.to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DbColumn {
    pub id: Thing,
    pub board: Thing,
    pub name: String,
    pub position: i32,
    pub last_edited_by: Option<String>,
    pub created_at: surrealdb::sql::Datetime,
    pub updated_at: surrealdb::sql::Datetime,
}

impl DbColumn {
    pub fn into_api(self) -> shared::Column {
        shared::Column {
            id: self.id.id.to_raw(),
            board_id: self.board.id.to_raw(),
            name: self.name,
            position: self.position,
            last_edited_by: self.last_edited_by,
            created_at: self.created_at.to_string(),
            updated_at: self.updated_at.to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DbCard {
    pub id: Thing,
    pub column: Thing,
    pub body: String,
    pub position: i32,
    pub number: Option<i32>,
    pub last_edited_by: Option<String>,
    pub created_at: surrealdb::sql::Datetime,
    pub updated_at: surrealdb::sql::Datetime,
}

impl DbCard {
    pub fn into_api(self) -> shared::Card {
        shared::Card {
            id: self.id.id.to_raw(),
            column_id: self.column.id.to_raw(),
            body: self.body,
            position: self.position,
            number: self.number.unwrap_or(0) as u32,
            last_edited_by: self.last_edited_by,
            created_at: self.created_at.to_string(),
            updated_at: self.updated_at.to_string(),
        }
    }
}

/// Minimal projection used only when incrementing the card counter.
#[derive(Debug, Serialize, Deserialize)]
pub struct DbCardCounter {
    pub count: i32,
}
