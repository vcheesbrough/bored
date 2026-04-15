use surrealdb::{
    engine::local::{Db, Mem, SurrealKv},
    Surreal,
};

pub async fn connect_persistent(path: &str) -> surrealdb::Result<Surreal<Db>> {
    let db = Surreal::new::<SurrealKv>(path).await?;
    init(&db).await?;
    Ok(db)
}

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
