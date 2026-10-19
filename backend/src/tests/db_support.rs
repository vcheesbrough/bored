//! The per-test Postgres database (card #471). Test-only, and in the test
//! tree so the "every database call is traced" scan (which covers product
//! code) does not read it.

use crate::db::{Db, MIGRATOR, pool_options, quiet, with_session_limits};

/// A fresh, fully migrated database of its own for one test.
///
/// Tests run in parallel, so each gets a new database
/// (`bored_test_<ulid>`) created on the server `DATABASE_URL` names, rather
/// than sharing one and cleaning up between tests. The databases are left
/// behind: CI's Postgres is a throwaway container (`scripts/with-test-postgres.sh`),
/// and a local one can be dropped wholesale.
///
/// A missing `DATABASE_URL` is a panic with instructions rather than a skip:
/// nearly every backend test needs the database, so silently skipping them
/// would turn the suite green by testing nothing.
pub(crate) async fn connect_test() -> Result<Db, sqlx::Error> {
    use sqlx::{ConnectOptions as _, Connection as _};

    let url = std::env::var("DATABASE_URL").expect(
        "backend tests need Postgres: run them through `scripts/with-test-postgres.sh`, \
         or set DATABASE_URL to a server this user may CREATE DATABASE on",
    );
    let admin = with_session_limits(quiet(url.parse()?));
    let name = format!(
        "bored_test_{}",
        ulid::Ulid::new().to_string().to_lowercase()
    );

    let mut connection = admin.connect().await?;
    // Identifiers cannot be bound as parameters; `name` is built above from a
    // ULID (`[0-9a-z]` only), so formatting it in is safe.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut connection)
        .await?;
    connection.close().await?;

    // Three connections per test: enough for the concurrency tests, and small
    // enough that a parallel suite stays well under the server's limit.
    let pool = pool_options(3).connect_lazy_with(admin.database(&name));
    MIGRATOR.run(&pool).await?;
    // As `connect` does, so the pool gauge has something to report in tests.
    crate::observability::metrics::watch_pool(pool.clone());
    Ok(pool)
}
