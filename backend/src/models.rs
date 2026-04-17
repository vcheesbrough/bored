use serde::{Deserialize, Serialize};
use surrealdb::sql::Thing;

#[derive(Debug, Serialize, Deserialize)]
pub struct DbBoard {
    pub id: Thing,
    pub name: String,
    pub created_at: surrealdb::sql::Datetime,
    pub updated_at: surrealdb::sql::Datetime,
}

impl DbBoard {
    pub fn into_api(self) -> shared::Board {
        shared::Board {
            id: self.id.id.to_raw(),
            name: self.name,
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
            created_at: self.created_at.to_string(),
            updated_at: self.updated_at.to_string(),
        }
    }
}
