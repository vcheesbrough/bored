#[cfg(test)]
use surrealdb::engine::local::Mem;
use surrealdb::{
    engine::local::{Db, SurrealKv},
    Surreal,
};

pub async fn connect_persistent(path: &str) -> surrealdb::Result<Surreal<Db>> {
    let db = Surreal::new::<SurrealKv>(path).await?;
    init(&db).await?;
    Ok(db)
}

#[cfg(test)]
pub async fn connect_mem() -> surrealdb::Result<Surreal<Db>> {
    let db = Surreal::new::<Mem>(()).await?;
    init(&db).await?;
    Ok(db)
}

async fn init(db: &Surreal<Db>) -> surrealdb::Result<()> {
    db.use_ns("bored").use_db("bored").await?;
    db.query(include_str!("schema.surql")).await?.check()?;
    Ok(())
}
