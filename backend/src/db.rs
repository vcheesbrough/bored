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
    // Sanitize existing board names into slug format (lowercase, hyphens only)
    // and deduplicate before enforcing the unique index below.
    migrate_board_names(db).await?;
    crate::audit::migrate_audit_baselines(db).await?;
    // Now safe to add the uniqueness constraint — all names are already clean.
    db.query("DEFINE INDEX IF NOT EXISTS board_name_unique ON TABLE boards FIELDS name UNIQUE")
        .await?
        .check()?;
    Ok(())
}

/// Convert an arbitrary string into a URL slug: ASCII-lowercase, any character
/// that is not `[a-z0-9]` becomes a hyphen, consecutive hyphens are collapsed,
/// leading/trailing hyphens are stripped.  Falls back to `"board"` for empty results.
pub(crate) fn slugify_name(name: &str) -> String {
    let lowered = name.to_ascii_lowercase();
    let mut slug = String::with_capacity(lowered.len());
    // Treat the virtual character before the string as a hyphen so leading
    // separators are dropped without a separate trim step.
    let mut last_was_sep = true;

    for ch in lowered.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('-');
            last_was_sep = true;
        }
    }

    // Strip trailing hyphen left when the input ends with a separator.
    if slug.ends_with('-') {
        slug.pop();
    }

    if slug.is_empty() {
        "board".to_string()
    } else {
        slug
    }
}

/// On startup, ensure every board's name is a valid slug and no two boards
/// share a name.  Boards are processed in creation order so the earliest board
/// keeps the "clean" slug; later duplicates get a numeric suffix (-2, -3, …).
/// This is idempotent: boards whose names are already valid slugs are untouched.
async fn migrate_board_names(db: &Surreal<Db>) -> surrealdb::Result<()> {
    #[derive(serde::Deserialize)]
    struct RawBoard {
        id: surrealdb::sql::Thing,
        name: String,
    }

    // SELECT * so SurrealDB can resolve the ORDER BY created_at field;
    // take() deserializes only the fields declared in RawBoard.
    let boards: Vec<RawBoard> = db
        .query("SELECT * FROM boards ORDER BY created_at ASC")
        .await?
        .take(0)?;

    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();

    for board in boards {
        let base = slugify_name(&board.name);

        let final_name = if !used.contains(&base) {
            base.clone()
        } else {
            let mut n = 2u32;
            loop {
                let candidate = format!("{base}-{n}");
                if !used.contains(&candidate) {
                    break candidate;
                }
                n += 1;
            }
        };

        used.insert(final_name.clone());

        if final_name != board.name {
            db.query("UPDATE $id SET name = $name")
                .bind(("id", board.id))
                .bind(("name", final_name))
                .await?
                .check()?;
        }
    }

    Ok(())
}
