// `#[cfg(test)]` means this `use` is only compiled when running `cargo test`.
// The `Mem` backend stores data in memory — perfect for tests because each
// test gets a fresh, isolated database with no disk I/O.
#[cfg(test)]
use surrealdb::engine::local::Mem;

use std::time::Duration;
use surrealdb::opt::Config;
use surrealdb::{
    Surreal,
    engine::local::{Db, SurrealKv}, // `SurrealKv` is the persistent on-disk backend
};

// SurrealDB's local engine spawns several always-on background tasks (node-membership
// heartbeat/refresh, expiry-check, cleanup, and changefeed GC) that fire on fixed
// timers regardless of whether any client is connected. Their defaults are tuned for
// a clustered multi-node deployment — the refresh task defaults to every *3 seconds*
// and on every tick writes a node-heartbeat row (a KV transaction + fsync) and also
// refreshes system-usage metrics. For a single embedded node that work is largely
// wasted and shows up as continuous idle CPU.
//
// We lengthen these intervals, but the refresh interval CANNOT be stretched freely:
// SurrealDB's expiry-check archives any node whose heartbeat is older than a
// hard-coded 30s (`kvs/node.rs`: `n.hb < now - Duration::from_secs(30)`). If refresh
// ran only every 5 minutes, our own node would look expired for most of each window,
// and the expiry-check would archive it — flapping archive → cleanup → re-register
// every cycle (the next refresh revives it via `update_node`). That churn defeats the
// purpose and is fragile, so we keep refresh comfortably under 30s.
//
// - HEARTBEAT_REFRESH_INTERVAL (20s): keeps our node's heartbeat fresh (~10s of margin
//   below the 30s expiry window even if a tick is delayed), while still cutting the
//   heartbeat write rate ~7x versus the 3s default.
// - MAINTENANCE_INTERVAL (300s): the expiry-check, archived-node cleanup, and
//   changefeed GC are not time-sensitive once the heartbeat stays fresh (a healthy
//   node is never archived, so the check/cleanup are no-ops), so we run them rarely.
//
// The ~5s index-compaction tick is NOT exposed by the embedded `Config` API (only via
// the standalone server CLI), so it stays as an unavoidable — but here no-op, since we
// define no SEARCH/full-text indexes — floor. See card #255 for the full investigation.
const HEARTBEAT_REFRESH_INTERVAL: Duration = Duration::from_secs(20);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(300);

// Called at startup in production. `path` is a filesystem path like `/data/bored.db`.
// Returns a `Surreal<Db>` — the generic `Db` type erases the concrete backend so
// the rest of the app doesn't need to know whether storage is on-disk or in-memory.
pub async fn connect_persistent(path: &str) -> surrealdb::Result<Surreal<Db>> {
    // Build a config that lengthens the embedded engine's background-maintenance
    // intervals (see the constants above). Each setter internally does
    // `.filter(|x| !x.is_zero())`, so a non-zero Duration is required to override
    // the default — these intervals can be *lengthened* but never fully disabled.
    // Refresh stays under the 30s expiry window; the rest run every 5 minutes.
    let config = Config::new()
        .node_membership_refresh_interval(HEARTBEAT_REFRESH_INTERVAL)
        .node_membership_check_interval(MAINTENANCE_INTERVAL)
        .node_membership_cleanup_interval(MAINTENANCE_INTERVAL)
        .changefeed_gc_interval(MAINTENANCE_INTERVAL);
    // `Surreal::new::<SurrealKv>((path, config))` opens (or creates) the database
    // file at `path` with the tuned config. The `?` propagates any connection
    // error up to the caller.
    let db = Surreal::new::<SurrealKv>((path, config)).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    // Exercises the real `connect_persistent` path (on-disk SurrealKv + tuned
    // `Config`), rather than the in-memory `connect_mem` used elsewhere. The goal
    // is to prove the tuned-`Config` connection still opens, runs `init()` (schema
    // + migrations), and serves a basic round-trip query — i.e. lengthening the
    // maintenance intervals didn't break connection setup. We can't assert on the
    // interval values themselves (the `Config` fields are private and the engine
    // exposes no getter), so we assert on observable behaviour instead.
    #[tokio::test]
    async fn connect_persistent_opens_and_serves_queries() {
        // A throwaway directory that is deleted when `dir` drops at end of test.
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // SurrealKv stores its files under this path; a subdir keeps it tidy.
        let path = dir.path().join("bored-db");
        let path_str = path.to_str().expect("temp path is valid UTF-8");

        // Opening must succeed with the tuned Config and apply the schema via init().
        let db = connect_persistent(path_str)
            .await
            .expect("connect_persistent should open the database");

        // A trivial write+read round-trip proves the connection is usable and the
        // `boards` table from schema.surql exists. We create one board and read it
        // back; `.check()` turns any SurrealDB-level error into a test failure.
        db.query("CREATE boards SET name = 'probe', created_at = time::now()")
            .await
            .expect("insert query should execute")
            .check()
            .expect("insert should not surface a SurrealDB error");

        #[derive(serde::Deserialize)]
        struct NameOnly {
            name: String,
        }
        let rows: Vec<NameOnly> = db
            .query("SELECT name FROM boards")
            .await
            .expect("select query should execute")
            .take(0)
            .expect("select should deserialize");

        assert_eq!(rows.len(), 1, "expected exactly the one board we inserted");
        assert_eq!(rows[0].name, "probe");
    }
}
