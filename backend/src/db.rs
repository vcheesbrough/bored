// `#[cfg(test)]` means this `use` is only compiled when running `cargo test`.
// The `Mem` backend stores data in memory — perfect for tests because each
// test gets a fresh, isolated database with no disk I/O.
#[cfg(test)]
use surrealdb::engine::local::Mem;

use surrealdb::{
    engine::local::{Db, SurrealKv}, // `SurrealKv` is the persistent on-disk backend
    Surreal,
};

// Called at startup in production. `path` is a filesystem path like `/data/bored.db`.
// Returns a `Surreal<Db>` — the generic `Db` type erases the concrete backend so
// the rest of the app doesn't need to know whether storage is on-disk or in-memory.
pub async fn connect_persistent(path: &str) -> surrealdb::Result<Surreal<Db>> {
    // `Surreal::new::<SurrealKv>(path)` opens (or creates) the database file at `path`.
    // The `?` propagates any connection error up to the caller.
    let db = Surreal::new::<SurrealKv>(path).await?;
    init(&db).await?;
    Ok(db)
}

// Only compiled in test builds. Uses an in-memory backend so tests never touch disk
// and each `connect_mem()` call starts with a completely empty database.
#[cfg(test)]
pub async fn connect_mem() -> surrealdb::Result<Surreal<Db>> {
    // `()` is the unit type — `Mem` takes no path argument.
    let db = Surreal::new::<Mem>(()).await?;
    init(&db).await?;
    Ok(db)
}

// Shared initialisation: selects the namespace/database and applies the schema.
// Both production and test connections go through this.
async fn init(db: &Surreal<Db>) -> surrealdb::Result<()> {
    // SurrealDB uses a two-level namespace system: namespace → database.
    // We use "bored" for both. This must be called before any queries.
    db.use_ns("bored").use_db("bored").await?;

    // `include_str!` is a compile-time macro that reads a file from disk and
    // embeds it as a `&'static str` in the binary. The schema is applied every
    // startup — SurrealDB's `DEFINE ... IF NOT EXISTS` semantics make it idempotent
    // (safe to run multiple times without duplicating anything).
    // `.check()` turns any SurrealDB-level errors in the response into a Rust `Err`.
    db.query(include_str!("schema.surql")).await?.check()?;
    Ok(())
}
