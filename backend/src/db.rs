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
// - MAINTENANCE_INTERVAL (300s): the expiry-check and archived-node cleanup are not
//   time-sensitive once the heartbeat stays fresh (a healthy node is never archived,
//   so the check/cleanup are no-ops), so we run them rarely.
// - CHANGEFEED_GC_INTERVAL (24h): the changefeed "GC" task is not GC-only. Every tick
//   (`Datastore::changefeed_process`) first *writes* one `!ts` key per database
//   (`kvs/cf.rs::changefeed_versionstamp` → `kvs/tr.rs::set_timestamp_for_versionstamp`),
//   mapping "now" to the current versionstamp, and nothing ever deletes those keys —
//   the GC half (`cf/gc.rs`) only removes changefeed *entries*. bored defines no
//   changefeeds (the `schema_defines_no_changefeed` test guards that), so the task
//   does nothing useful for us and its only effect is one leaked key per tick.
//   SurrealKV keeps every live key in memory, and by card #470 ~824k of them
//   (10s default, then 300s since Iteration 36) held most of the backend's ~450 MB
//   idle RSS. 24h cuts growth from 288 keys/day to ~1/day. Like the others it cannot
//   be zero (see `connect_persistent`), so "rarely" is the best the SDK allows.
//   Stopgap until the Postgres migration replaces SurrealDB; the keys already leaked
//   stay until then. If a `CHANGEFEED` is ever defined, revisit this: its entries
//   would then only be garbage-collected once a day.
//
// The ~5s index-compaction tick is NOT exposed by the embedded `Config` API (only via
// the standalone server CLI), so it stays as an unavoidable — but here no-op, since we
// define no SEARCH/full-text indexes — floor. See card #255 for the full investigation.
const HEARTBEAT_REFRESH_INTERVAL: Duration = Duration::from_secs(20);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(300);
const CHANGEFEED_GC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The embedded engine's background-task intervals, gathered in one value so
/// tests can open the *same* connection path as production with one interval
/// changed (the `!ts` leak test shortens the changefeed tick to make it
/// observable in seconds).
///
/// `#[derive(Clone, Copy)]` lets the struct be passed by value freely: it is
/// four `Duration`s, which are themselves plain `Copy` numbers.
#[derive(Debug, Clone, Copy)]
struct EngineIntervals {
    heartbeat_refresh: Duration,
    membership_check: Duration,
    membership_cleanup: Duration,
    changefeed_gc: Duration,
}

/// What production runs with. A `const` (evaluated at compile time) so the
/// values above are the single source of truth.
const PRODUCTION_INTERVALS: EngineIntervals = EngineIntervals {
    heartbeat_refresh: HEARTBEAT_REFRESH_INTERVAL,
    membership_check: MAINTENANCE_INTERVAL,
    membership_cleanup: MAINTENANCE_INTERVAL,
    changefeed_gc: CHANGEFEED_GC_INTERVAL,
};

// ─────────────────────────────────────────────────────────────────────────────
// Database spans (card #415)
// ─────────────────────────────────────────────────────────────────────────────

/// What a database call does, as semconv's `db.operation.name`.
///
/// A small enum rather than a string at each call site so the value set is
/// fixed in code. `Define` is the schema and index statements run at startup.
/// Add a verb here (and its label below) the day a query needs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbOperation {
    Select,
    Create,
    Update,
    Delete,
    Define,
}

impl DbOperation {
    /// Every variant, for the every-variant label test.
    #[cfg(test)]
    pub(crate) const ALL: [DbOperation; 5] = [
        DbOperation::Select,
        DbOperation::Create,
        DbOperation::Update,
        DbOperation::Delete,
        DbOperation::Define,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            DbOperation::Select => "SELECT",
            DbOperation::Create => "CREATE",
            DbOperation::Update => "UPDATE",
            DbOperation::Delete => "DELETE",
            DbOperation::Define => "DEFINE",
        }
    }
}

/// What a traced call's result must do before the span can say whether it
/// failed.
///
/// A `db.query(..)` resolves to a [`surrealdb::Response`] *even when a
/// statement inside it failed* — the error waits inside the response until
/// `.check()` or `.take(n)`. So for a `Response` the span calls `check()`
/// itself, which turns a statement error into the `Err` it would have become
/// one step later at the call site anyway; the caller's own `.check()` /
/// `.take(n)` then runs on a response already known to be clean. Every other
/// result (`select`, `create`, `update`, `delete` builders) has already
/// failed or succeeded by the time it resolves.
pub(crate) trait Settle: Sized {
    fn settle(self) -> surrealdb::Result<Self>;
}

impl Settle for surrealdb::Response {
    fn settle(self) -> surrealdb::Result<Self> {
        self.check()
    }
}

impl<T> Settle for Option<T> {
    fn settle(self) -> surrealdb::Result<Self> {
        Ok(self)
    }
}

impl<T> Settle for Vec<T> {
    fn settle(self) -> surrealdb::Result<Self> {
        Ok(self)
    }
}

impl Settle for () {
    fn settle(self) -> surrealdb::Result<Self> {
        Ok(self)
    }
}

/// The identity of one database call site, for its span.
///
/// Every field is a `&'static str` or an enum, so nothing computed at run time
/// — and in particular no SQL text and no bound value — can reach a span
/// attribute through it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DbQuery {
    /// The call site: `<module>.<function>`, e.g. `cards.create_card`.
    name: &'static str,
    operation: DbOperation,
    /// The table the call is about (`cards`, `audit_log`, …).
    table: &'static str,
}

impl DbQuery {
    pub(crate) const fn new(
        name: &'static str,
        operation: DbOperation,
        table: &'static str,
    ) -> Self {
        Self {
            name,
            operation,
            table,
        }
    }
}

/// `.traced(DbQuery::new(..))` on any SurrealDB call, before its `.await`:
///
/// ```ignore
/// let card: Option<DbCard> = state
///     .db
///     .select(("cards", id))
///     .traced(DbQuery::new("cards.get_card", DbOperation::Select, "cards"))
///     .await?;
/// ```
///
/// Opens a `client` span per call — the database is embedded, but a call into
/// it is still where a request's time goes, and the skill asks for a child
/// span per call that leaves the handler's own code. The span is named, as
/// semconv asks, by its summary (`SELECT cards`) and carries the call site as
/// `bored.db.query.name`; never SQL text or a bound value (skill §6:
/// telemetry is published). A failure sets `error.type` from the same
/// classification the request's `ApiError` will use, marks the span as an
/// error, and counts `bored.db.errors`.
///
/// A trait with a blanket impl, rather than a free function, so the call site
/// reads as one more step in the builder chain.
pub(crate) trait Traced<T>: Sized {
    fn traced(
        self,
        query: DbQuery,
    ) -> impl std::future::Future<Output = surrealdb::Result<T>> + Send;
}

impl<F, T> Traced<T> for F
where
    // `IntoFuture` is what SurrealDB's builders implement: they become a
    // future only when awaited. `Send` because axum handler futures must be.
    F: std::future::IntoFuture<Output = surrealdb::Result<T>> + Send,
    F::IntoFuture: Send,
    T: Settle + Send,
{
    fn traced(
        self,
        query: DbQuery,
    ) -> impl std::future::Future<Output = surrealdb::Result<T>> + Send {
        use tracing::Instrument as _;
        // semconv's low-cardinality summary: `{operation} {table}`.
        let summary = format!("{} {}", query.operation.label(), query.table);
        // Literal keys, as the `tracing` macros require; each is a semconv
        // name or `bored.`-prefixed (`observability::tests` checks what the
        // exporter received). `db.system.name` is not one of semconv's listed
        // values — SurrealDB has none — which the convention allows.
        let span = tracing::info_span!(
            "db",
            otel.name = %summary,
            otel.kind = "client",
            otel.status_code = tracing::field::Empty,
            db.system.name = "surrealdb",
            db.namespace = "bored",
            db.operation.name = query.operation.label(),
            db.collection.name = query.table,
            db.query.summary = %summary,
            bored.db.query.name = query.name,
            error.type = tracing::field::Empty,
        );
        // `clone` so the span can be both the one the future runs inside and
        // the one this function records the outcome on.
        let recorder = span.clone();
        async move {
            let result = self.await.and_then(Settle::settle);
            if let Err(error) = &result {
                let error_type = crate::error::database_error_type(error);
                recorder.record("error.type", error_type.as_str());
                recorder.record("otel.status_code", "ERROR");
                crate::observability::metrics::db_error(error_type);
            }
            result
        }
        .instrument(span)
    }
}

// Called at startup in production. `path` is a filesystem path like `/data/bored.db`.
// Returns a `Surreal<Db>` — the generic `Db` type erases the concrete backend so
// the rest of the app doesn't need to know whether storage is on-disk or in-memory.
pub async fn connect_persistent(path: &str) -> surrealdb::Result<Surreal<Db>> {
    connect_persistent_with(path, PRODUCTION_INTERVALS).await
}

// The body of `connect_persistent`, with the intervals as a parameter so tests
// can exercise the identical open path with a different tick. Private (no `pub`):
// production code only ever reaches it through `connect_persistent`.
async fn connect_persistent_with(
    path: &str,
    intervals: EngineIntervals,
) -> surrealdb::Result<Surreal<Db>> {
    // Build a config that lengthens the embedded engine's background-maintenance
    // intervals (see the constants above). Each setter internally does
    // `.filter(|x| !x.is_zero())`, so a non-zero Duration is required to override
    // the default — these intervals can be *lengthened* but never fully disabled.
    // Refresh stays under the 30s expiry window; check/cleanup run every 5
    // minutes, and the leaking changefeed tick once a day (card #470).
    let config = Config::new()
        .node_membership_refresh_interval(intervals.heartbeat_refresh)
        .node_membership_check_interval(intervals.membership_check)
        .node_membership_cleanup_interval(intervals.membership_cleanup)
        .changefeed_gc_interval(intervals.changefeed_gc);
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
    db.query(include_str!("schema.surql"))
        .traced(DbQuery::new("db.init", DbOperation::Define, "schema"))
        .await?
        .check()?;
    // Sanitize existing board names into slug format (lowercase, hyphens only)
    // and deduplicate before enforcing the unique index below.
    migrate_board_names(db).await?;
    crate::audit::migrate_audit_baselines(db).await?;
    // Now safe to add the uniqueness constraint — all names are already clean.
    db.query("DEFINE INDEX IF NOT EXISTS board_name_unique ON TABLE boards FIELDS name UNIQUE")
        .traced(DbQuery::new("db.init", DbOperation::Define, "boards"))
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
        .traced(DbQuery::new(
            "db.migrate_board_names",
            DbOperation::Select,
            "boards",
        ))
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
                .traced(DbQuery::new(
                    "db.migrate_board_names",
                    DbOperation::Update,
                    "boards",
                ))
                .await?
                .check()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every SurrealDB call in product code goes through `.traced(..)`, so a
    /// call added later cannot quietly miss its span (card #415).
    ///
    /// A source scan, like the telemetry module's SDK allowlist test: for each
    /// `db.<query|select|create|update|delete|upsert|insert>(` outside test
    /// code, the text up to the next `.await` must contain `.traced(`. Only
    /// code before a file's `#[cfg(test)] mod tests` is scanned, and comment
    /// lines are skipped.
    #[test]
    fn every_database_call_is_traced() {
        const METHODS: [&str; 7] = [
            "query", "select", "create", "update", "delete", "upsert", "insert",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        let mut pending = vec![root];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src") {
                let path = entry.expect("entry").path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if path.is_dir() {
                    // Test-only trees.
                    if name != "tests" {
                        pending.push(path);
                    }
                } else if name.ends_with(".rs") && name != "tests.rs" {
                    files.push(path);
                }
            }
        }

        let mut calls = 0;
        let mut untraced = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read source");
            // Product code only: stop at the test module.
            let product = source
                .find("#[cfg(test)]\nmod tests")
                .map_or(source.as_str(), |cut| &source[..cut]);
            // Blank out comment lines so a doc example is not counted.
            let code: String = product
                .lines()
                .map(|line| {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") { "" } else { line }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let bytes = code.as_bytes();
            let mut index = 0;
            while let Some(found) = code[index..].find("db") {
                let start = index + found;
                index = start + 2;
                // `db` must be a whole word (not `self.dbx` or `Db`).
                if start > 0
                    && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_')
                {
                    continue;
                }
                let rest = code[index..].trim_start();
                let Some(after_dot) = rest.strip_prefix('.') else {
                    continue;
                };
                let after_dot = after_dot.trim_start();
                if !METHODS
                    .iter()
                    .any(|method| after_dot.starts_with(&format!("{method}(")))
                {
                    continue;
                }
                calls += 1;
                let chain = &code[index..];
                let until_await = chain.find(".await").map_or(chain, |end| &chain[..end]);
                if !until_await.contains(".traced(") {
                    let line = code[..start].lines().count() + 1;
                    untraced.push(format!("{}:{line}", path.display()));
                }
            }
        }
        // Non-degenerate: the scan must be finding the product's calls.
        assert!(calls > 50, "only {calls} database calls found");
        assert!(
            untraced.is_empty(),
            "database calls without `.traced(..)`:\n{}",
            untraced.join("\n")
        );
    }

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

    // ─────────────────────────────────────────────────────────────────────
    // The `!ts` key leak (card #470)
    // ─────────────────────────────────────────────────────────────────────

    /// The first bytes of every `!ts` key SurrealDB writes for namespace
    /// `bored`, database `bored`, written out by hand from the key layout
    /// (`surrealdb-core` `key/database/ts.rs`: `/` `*` ns `*` db `!ts` then a
    /// big-endian `u64` timestamp, where each name is NUL-terminated by the
    /// key encoder) rather than built with SurrealDB's own key code, so the
    /// count below does not trust the code under test.
    ///
    /// `b"..."` is a byte-string literal: a `&'static [u8; N]`, not a `str`,
    /// and `\0` inside it is a single zero byte.
    const TS_KEY_PREFIX: &[u8] = b"/*bored\0*bored\0!ts";

    /// The `!ts` keys (and, for a sanity check, all keys) in the SurrealKV
    /// store at `dir`, read with the `surrealkv` crate directly.
    ///
    /// The SurrealDB connection that wrote `dir` must be fully closed first:
    /// SurrealKV keeps its index in memory and only one `Store` should own a
    /// directory at a time. `run_engine_for` guarantees that.
    fn count_keys(dir: &std::path::Path) -> (usize, usize) {
        // The same options SurrealDB opens the store with (`kvs/surrealkv`):
        // on disk, and `enable_versions` off for the plain `SurrealKv` engine.
        let mut opts = surrealkv::Options::new();
        opts.dir = dir.to_path_buf();
        opts.disk_persistence = true;
        opts.enable_versions = false;
        let store = surrealkv::Store::new(opts).expect("surrealkv should open the store");
        let txn = store
            .begin_with_mode(surrealkv::Mode::ReadOnly)
            .expect("read-only transaction");

        // The exclusive upper bound of "every key starting with the prefix":
        // the prefix with its last byte bumped (`!ts` → `!tt`). Keys sort
        // byte-wise, so `[prefix, bumped)` is exactly the prefixed keys.
        let mut upper = TS_KEY_PREFIX.to_vec();
        *upper.last_mut().expect("prefix is not empty") += 1;
        let ts_keys = txn.keys(TS_KEY_PREFIX..upper.as_slice(), None).count();
        // `..` (a `RangeFull`) is every key in the store.
        let all_keys = txn.keys(.., None).count();
        (ts_keys, all_keys)
    }

    /// Opens the database at `path` through the production open path with the
    /// given intervals, writes a board every 100 ms for `wall` (so the run is
    /// not idle — ordinary writes must not create `!ts` keys either), then
    /// closes it completely. Returns how long the connection was open.
    ///
    /// A runtime of its own, rather than `#[tokio::test]`, because SurrealDB
    /// closes its store on a spawned task after the last handle drops, and
    /// nothing hands that task back to await. Dropping the runtime drops every
    /// task it owns — and with them the last `Arc<Datastore>`, whose `Drop`
    /// closes SurrealKV (flushing the commit log) — before `count_keys` opens
    /// the same directory.
    fn run_engine_for(
        path: &std::path::Path,
        intervals: EngineIntervals,
        wall: Duration,
    ) -> Duration {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let open_for = runtime.block_on(async {
            let path_str = path.to_str().expect("temp path is valid UTF-8");
            let db = connect_persistent_with(path_str, intervals)
                .await
                .expect("connect_persistent_with should open the database");
            let opened = std::time::Instant::now();
            let mut written = 0u32;
            while opened.elapsed() < wall {
                db.query("CREATE boards SET name = $name")
                    .bind(("name", format!("probe-{written}")))
                    .await
                    .expect("insert query should execute")
                    .check()
                    .expect("insert should not surface a SurrealDB error");
                written += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let open_for = opened.elapsed();
            // Dropping the last handle ends SurrealDB's router loop, which
            // cancels the background tasks and shuts the datastore down. The
            // short sleep lets that orderly path run before the runtime is
            // dropped (which would otherwise just cut it off).
            drop(db);
            tokio::time::sleep(Duration::from_millis(500)).await;
            open_for
        });
        drop(runtime);
        open_for
    }

    /// The leak is stopped: the changefeed tick is what writes `!ts` keys,
    /// one per tick, and production's tick is now rare enough to add at most
    /// one key a day.
    ///
    /// Three parts, because a test that runs for seconds cannot tell a 300 s
    /// interval from a 24 h one by waiting:
    /// 1. **Mechanism.** With a 200 ms test-only tick, keys appear — about one
    ///    per tick. This proves the oracle can see the leak, and measures the
    ///    "one key per tick" rate part 3 relies on.
    /// 2. **Production config, same wall time.** No keys at all, despite
    ///    writes: nothing else in the open path or the write path leaks.
    /// 3. **Budget.** The production tick adds at most one key per day — the
    ///    card's requirement, stated in keys/day rather than as the constant.
    ///    The previous `MAINTENANCE_INTERVAL` (300 s) would be 288/day.
    #[test]
    fn changefeed_tick_no_longer_leaks_ts_keys() {
        const WALL: Duration = Duration::from_secs(3);
        let short_tick = Duration::from_millis(200);

        // ── 1. Mechanism ────────────────────────────────────────────────
        let leaky_dir = tempfile::tempdir().expect("temp dir");
        let leaky_path = leaky_dir.path().join("bored-db");
        // `..PRODUCTION_INTERVALS` is struct-update syntax: every field not
        // named here is copied from the production value, so only the
        // changefeed tick differs from what the backend runs.
        let leaky = EngineIntervals {
            changefeed_gc: short_tick,
            ..PRODUCTION_INTERVALS
        };
        let open_for = run_engine_for(&leaky_path, leaky, WALL);
        let (leaked, all_keys) = count_keys(&leaky_path);
        // Non-degenerate: the oracle is reading SurrealDB's store (the schema
        // and the boards written above are in there), not an empty directory.
        assert!(all_keys > 50, "only {all_keys} keys read from the store");
        // The tick's first fire is one interval after open (the SDK consumes
        // tokio's immediate first tick), so about `open_for / short_tick`
        // fires happen (~15 here). The upper bound is the one part 3 leans
        // on — never more than one key per tick (+3 for the ticks that can
        // land during shutdown). The lower bound is loose on purpose: under a
        // loaded CI host the tick is delayed rather than doubled up
        // (`MissedTickBehavior::Delay`), so fewer fires is jitter, not a bug.
        let ticks = (open_for.as_secs_f64() / short_tick.as_secs_f64()).floor() as usize;
        assert!(
            leaked >= (ticks / 4).max(1) && leaked <= ticks + 3,
            "with a {short_tick:?} tick over {open_for:?} (~{ticks} ticks), \
             expected about one `!ts` key per tick, found {leaked}"
        );

        // ── 2. Production config, same wall time ────────────────────────
        let prod_dir = tempfile::tempdir().expect("temp dir");
        let prod_path = prod_dir.path().join("bored-db");
        run_engine_for(&prod_path, PRODUCTION_INTERVALS, WALL);
        let (prod_leaked, prod_all_keys) = count_keys(&prod_path);
        assert!(
            prod_all_keys > 50,
            "only {prod_all_keys} keys read from the store"
        );
        assert_eq!(
            prod_leaked, 0,
            "the production config wrote `!ts` keys within {WALL:?}"
        );

        // ── 3. Budget ───────────────────────────────────────────────────
        // One key per tick (part 1), so keys/day = one day / the tick.
        const ONE_DAY: Duration = Duration::from_secs(24 * 60 * 60);
        let keys_per_day = ONE_DAY.as_secs_f64() / PRODUCTION_INTERVALS.changefeed_gc.as_secs_f64();
        assert!(
            keys_per_day <= 1.0,
            "the production changefeed tick ({:?}) leaks {keys_per_day} `!ts` keys/day; \
             card #470 caps it at 1",
            PRODUCTION_INTERVALS.changefeed_gc
        );
    }

    /// Every `DEFINE` the database holds, rendered as text: `INFO FOR NS`
    /// lists the databases (a database-level `CHANGEFEED` shows there) and
    /// `INFO FOR DB` the tables (a table-level one shows there).
    async fn definitions(db: &Surreal<Db>) -> String {
        let mut response = db
            .query("INFO FOR NS; INFO FOR DB;")
            .await
            .expect("INFO should execute");
        // `surrealdb::Value` is SurrealDB's dynamic value; `take(n)` pulls
        // statement n's result out of the response, and `Display` renders it
        // in SurrealQL — including each object's full `DEFINE …` text.
        let ns: surrealdb::Value = response.take(0).expect("INFO FOR NS result");
        let db_info: surrealdb::Value = response.take(1).expect("INFO FOR DB result");
        format!("{ns}\n{db_info}")
    }

    /// The 24 h changefeed tick is only harmless because bored defines no
    /// changefeed: with one, its entries would be garbage-collected once a
    /// day. Fail loudly if the schema (or anything `init()` runs) ever adds
    /// one, so whoever does it revisits `CHANGEFEED_GC_INTERVAL`.
    #[tokio::test]
    async fn schema_defines_no_changefeed() {
        let db = connect_mem().await.expect("connect_mem");
        let defined = definitions(&db).await;
        // Non-degenerate: the INFO text really carries the schema's tables.
        assert!(
            defined.contains("DEFINE TABLE boards") && defined.contains("DEFINE TABLE cards"),
            "INFO output does not list the schema's tables:\n{defined}"
        );
        assert!(
            !defined.to_uppercase().contains("CHANGEFEED"),
            "the schema now defines a CHANGEFEED — the changefeed tick is set to \
             once a day on the basis that there are none (CHANGEFEED_GC_INTERVAL, \
             card #470); revisit it:\n{defined}"
        );

        // Control: the same check does see a changefeed when there is one.
        db.query("DEFINE TABLE changefeed_probe CHANGEFEED 1d")
            .await
            .expect("define should execute")
            .check()
            .expect("define should succeed");
        assert!(
            definitions(&db).await.to_uppercase().contains("CHANGEFEED"),
            "the check cannot see a table-level CHANGEFEED"
        );
    }
}
