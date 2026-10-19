//! The Postgres connection pool, its migrations, and the span every database
//! call runs inside (card #471, which replaced the embedded SurrealDB).
//!
//! The rest of the backend holds a [`Db`] — a `sqlx::PgPool`, which is an
//! `Arc`-backed handle, so cloning it is cheap and every clone shares the same
//! connections — and issues queries with `sqlx::query*` functions, each
//! followed by `.traced(DbQuery::new(..))` before its `.await`.

use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::config::DatabaseConfig;

/// The handle every handler reaches the database through.
///
/// A type alias rather than a newtype: it *is* a `PgPool`, and every sqlx
/// executor function accepts `&PgPool` directly.
pub type Db = sqlx::PgPool;

/// How many connections the pool may open. bored's traffic is a handful of
/// people and agents; most requests make a few short queries one after
/// another. A column or board delete holds one connection in a transaction
/// while it borrows a second for audit rows (`routes::cascade`), so ten leave
/// room for several of those at once. The pool gauge (`db.client.connection.count`) shows when this
/// stops being true.
const MAX_CONNECTIONS: u32 = 10;

/// How long a request waits for a free connection before failing with
/// `PoolTimedOut` (a `db_unavailable` 500). Long enough to ride out a brief
/// Postgres restart, short enough that a stuck pool shows up as errors
/// rather than as requests that never finish.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// The embedded migrations, from `backend/migrations/`, compiled into the
/// binary. A `static` so the startup path and the test harness run the
/// identical set.
pub(crate) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

// ─────────────────────────────────────────────────────────────────────────────
// Database spans (card #415)
// ─────────────────────────────────────────────────────────────────────────────

/// What a database call does, as semconv's `db.operation.name` — the SQL
/// keyword of the statement.
///
/// A small enum rather than a string at each call site so the value set is
/// fixed in code. Add a verb here (and its label below) the day a query needs
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbOperation {
    Select,
    Insert,
    Update,
    Delete,
}

impl DbOperation {
    /// Every variant, for the every-variant label test.
    #[cfg(test)]
    pub(crate) const ALL: [DbOperation; 4] = [
        DbOperation::Select,
        DbOperation::Insert,
        DbOperation::Update,
        DbOperation::Delete,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            DbOperation::Select => "SELECT",
            DbOperation::Insert => "INSERT",
            DbOperation::Update => "UPDATE",
            DbOperation::Delete => "DELETE",
        }
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

/// `.traced(DbQuery::new(..))` on any sqlx call, before its `.await`:
///
/// ```ignore
/// let card: Option<DbCard> = sqlx::query_as("SELECT * FROM cards WHERE id = $1")
///     .bind(id)
///     .fetch_optional(db)
///     .traced(DbQuery::new("cards.get_card", DbOperation::Select, "cards"))
///     .await?;
/// ```
///
/// Opens a `client` span per call — the skill asks for a child span per call
/// that leaves the process. The span is named, as semconv asks, by its summary
/// (`SELECT cards`) and carries the call site as `bored.db.query.name`; never
/// SQL text or a bound value (skill §6: telemetry is published). A failure
/// sets `error.type` from the same classification the request's `ApiError`
/// will use, marks the span as an error, and counts `bored.db.errors`.
///
/// A trait with a blanket impl, rather than a free function, so the call site
/// reads as one more step in the builder chain.
pub(crate) trait Traced<T>: Sized {
    fn traced(
        self,
        query: DbQuery,
    ) -> impl std::future::Future<Output = Result<T, sqlx::Error>> + Send;
}

impl<F, T> Traced<T> for F
where
    // `IntoFuture` covers both a plain future (what `fetch_one` and friends
    // return) and anything that only becomes one when awaited. `Send` because
    // axum handler futures must be.
    F: std::future::IntoFuture<Output = Result<T, sqlx::Error>> + Send,
    F::IntoFuture: Send,
    T: Send,
{
    fn traced(
        self,
        query: DbQuery,
    ) -> impl std::future::Future<Output = Result<T, sqlx::Error>> + Send {
        use tracing::Instrument as _;
        // semconv's low-cardinality summary: `{operation} {table}`.
        let summary = format!("{} {}", query.operation.label(), query.table);
        // Literal keys, as the `tracing` macros require; each is a semconv
        // name or `bored.`-prefixed (`observability::tests` checks what the
        // exporter received). `postgresql` is semconv's listed value.
        let span = tracing::info_span!(
            "db",
            otel.name = %summary,
            otel.kind = "client",
            otel.status_code = tracing::field::Empty,
            db.system.name = "postgresql",
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
            let result = self.await;
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

// ─────────────────────────────────────────────────────────────────────────────
// Connecting
// ─────────────────────────────────────────────────────────────────────────────

/// The pool options production and tests share, so a test exercises the
/// configuration the backend really runs with.
pub(crate) fn pool_options(max_connections: u32) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(ACQUIRE_TIMEOUT)
}

/// Connection options built field by field from config, as v-note does,
/// rather than from a URL: a password with `@` or `/` in it then needs no
/// escaping, and no URL holding the password is ever formatted.
fn connect_options(config: &DatabaseConfig) -> PgConnectOptions {
    quiet(
        PgConnectOptions::new()
            .host(&config.host)
            .port(config.port)
            .username(&config.user)
            .password(&config.password)
            .database(&config.name)
            // Shows up in `pg_stat_activity`, which tells the backend's
            // connections apart from the SQL console's and a `psql` session's.
            .application_name("bored"),
    )
}

/// Turn off sqlx's own statement logging. By default it logs every statement
/// at DEBUG and any slower than a second at WARN — with the SQL text as a log
/// field, which the redaction rule (card #366) forbids. The `db` span
/// `.traced` opens already records each call and its duration, by name.
pub(crate) fn quiet(options: PgConnectOptions) -> PgConnectOptions {
    use sqlx::ConnectOptions as _;
    options
        .log_statements(tracing::log::LevelFilter::Off)
        .log_slow_statements(tracing::log::LevelFilter::Off, Duration::ZERO)
}

/// Open the pool and bring the schema up to date. Called once at startup.
///
/// `connect_lazy_with` opens no connection here; the migration run below is
/// the first use, so an unreachable database fails startup on that line,
/// loudly, before the listener binds.
pub async fn connect(config: &DatabaseConfig) -> Result<Db, sqlx::Error> {
    let pool = pool_options(MAX_CONNECTIONS).connect_lazy_with(connect_options(config));
    MIGRATOR.run(&pool).await?;
    // Report the pool's in-use / idle counts as `db.client.connection.count`.
    crate::observability::metrics::watch_pool(pool.clone());
    Ok(pool)
}

/// Whether the database holds any bored data yet — any board, column, card,
/// link or audit row. The SurrealDB import (`surreal_migration.rs`) refuses to
/// write into a database for which this is true.
pub async fn has_data(db: &Db) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM boards) OR EXISTS (SELECT 1 FROM columns) \
             OR EXISTS (SELECT 1 FROM cards) OR EXISTS (SELECT 1 FROM card_links) \
             OR EXISTS (SELECT 1 FROM audit_log)",
    )
    .fetch_one(db)
    .traced(DbQuery::new("db.has_data", DbOperation::Select, "boards"))
    .await
}

/// A fresh, migrated database per test; lives in the test tree
/// (`tests/db_support.rs`), re-exported here so tests reach it as
/// `db::connect_test`.
#[cfg(test)]
pub(crate) use crate::tests::db_support::connect_test;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sqlx call in product code goes through `.traced(..)`, so a call
    /// added later cannot quietly miss its span (card #415).
    ///
    /// A source scan, like the telemetry module's SDK allowlist test: for each
    /// `sqlx::query*(` outside test code, the text up to the next `.await` must
    /// contain `.traced(`. Only code before a file's `#[cfg(test)] mod tests`
    /// is scanned, and comment lines are skipped.
    #[test]
    fn every_database_call_is_traced() {
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
            // The one-off SurrealDB import is an operator command, not request
            // handling: it runs in its own process with telemetry off.
            if path.ends_with("surreal_migration.rs") {
                continue;
            }
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
            let mut index = 0;
            while let Some(found) = code[index..].find("sqlx::query") {
                let start = index + found;
                index = start + "sqlx::query".len();
                // `query(`, `query_as(`, `query_scalar(`, with or without a
                // turbofish — anything up to the opening parenthesis.
                let rest = &code[index..];
                let Some(open) = rest.find('(') else { continue };
                let head = &rest[..open];
                if !head
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_:<>, ".contains(c))
                {
                    continue;
                }
                calls += 1;
                let until_await = rest.find(".await").map_or(rest, |end| &rest[..end]);
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

    /// Running the migrations again on a migrated database changes nothing
    /// and does not fail: sqlx records what it applied, which is what makes
    /// every restart safe.
    #[tokio::test]
    async fn migrations_are_idempotent() {
        let db = connect_test().await.expect("test database");
        let applied = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE success")
                .fetch_one(&db)
                .await
                .expect("count migrations")
        };
        let before = applied().await;
        assert_eq!(
            before as usize,
            MIGRATOR.iter().count(),
            "every embedded migration is applied once"
        );
        MIGRATOR.run(&db).await.expect("second run succeeds");
        MIGRATOR.run(&db).await.expect("third run succeeds");
        assert_eq!(applied().await, before);
    }

    /// sqlx's own query logging is off: a statement slower than its default
    /// one-second threshold would otherwise log its SQL text at WARN.
    #[tokio::test]
    async fn slow_statements_do_not_log_their_sql() {
        let db = connect_test().await.expect("test database");
        let (_, logs) = crate::tests::capture_logs_async(async {
            sqlx::query("SELECT pg_sleep(1.2), 'marker-7d1c' AS probe")
                .execute(&db)
                .await
                .expect("slow query runs")
        })
        .await;
        assert!(!logs.contains("marker-7d1c"), "SQL text was logged: {logs}");
        assert!(!logs.contains("pg_sleep"), "SQL text was logged: {logs}");

        // Control: the same statement on a connection with sqlx's default
        // logging *does* log its SQL, so the capture above could have seen it.
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let noisy: PgConnectOptions = url.parse().expect("url");
        let noisy = pool_options(1).connect_lazy_with(noisy);
        let (_, control) = crate::tests::capture_logs_async(async {
            sqlx::query("SELECT pg_sleep(1.2), 'marker-7d1c' AS probe")
                .execute(&noisy)
                .await
                .expect("slow query runs")
        })
        .await;
        assert!(
            control.contains("marker-7d1c"),
            "control: default sqlx logging should log the slow SQL: {control}"
        );
    }

    /// The schema is really there after `connect_test` (a control for the
    /// test above, which would also pass against an empty migration set).
    #[tokio::test]
    async fn migrations_create_the_tables() {
        let db = connect_test().await.expect("test database");
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = 'public' ORDER BY table_name",
        )
        .fetch_all(&db)
        .await
        .expect("list tables");
        for expected in ["audit_log", "boards", "card_links", "cards", "columns"] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}: {tables:?}"
            );
        }
        assert!(!has_data(&db).await.expect("has_data"));
    }
}
