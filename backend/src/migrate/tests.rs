//! Tests for the migrate step and the server's check of it (card #640).
//!
//! The database tests go through the production entry points — `db::connect`
//! (what the server runs at startup) and `migrate::run` (what
//! `bored-backend migrate` runs) — against a database of their own from
//! `db_support::empty_test_database`. Expected counts come from the
//! migration files on disk, not from the migrator under test.

use super::*;
use crate::tests::db_support::empty_test_database;

/// How many up migrations ship in `backend/migrations/`, counted from the
/// directory itself — an oracle independent of `sqlx::migrate!`.
fn migration_files() -> usize {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let count = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8")
        })
        .filter(|name| name.ends_with(".sql") && !name.ends_with(".down.sql"))
        .count();
    // Non-degenerate: a test that expects 0 pending proves nothing.
    assert!(count >= 2, "only {count} migration files found");
    count
}

/// The first migration's version, from its file name (`<version>_<name>.sql`).
fn first_version() -> i64 {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut versions: Vec<i64> = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8")
        })
        .filter_map(|name| name.split('_').next()?.parse().ok())
        .collect();
    versions.sort_unstable();
    versions[0]
}

/// Every applied row, in a form two runs can be compared by: `installed_on`
/// changes if a migration is ever re-applied, so equal snapshots mean the
/// second run touched nothing.
async fn applied_snapshot(db: &Db) -> Vec<(i64, String, Vec<u8>, bool)> {
    sqlx::query_as(
        "SELECT version, installed_on::text, checksum, success \
         FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(db)
    .await
    .expect("read _sqlx_migrations")
}

/// Whether sqlx's bookkeeping table exists at all — i.e. whether anything has
/// ever migrated this database.
async fn migrations_table_exists(db: &Db) -> bool {
    sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(db)
        .await
        .expect("to_regclass")
}

// ── The comparison rules, without a database ─────────────────────────────────

/// The applied rows a fully migrated database has: every embedded migration,
/// with its own checksum, finished.
fn all_applied() -> Vec<Applied> {
    migrator()
        .iter()
        .map(|migration| Applied {
            version: migration.version,
            checksum: migration.checksum.to_vec(),
            success: true,
        })
        .collect()
}

#[test]
fn a_fully_migrated_database_is_current() {
    let status = compare(&migrator(), &all_applied());
    assert_eq!(status, SchemaStatus::default());
    assert_eq!(status.verdict(), Ok(()));
}

#[test]
fn nothing_applied_is_every_migration_pending() {
    let status = compare(&migrator(), &[]);
    assert_eq!(status.pending, migration_files());
    assert_eq!(
        status.verdict(),
        Err(SchemaError::Pending(migration_files()))
    );
}

#[test]
fn an_unknown_applied_migration_is_tolerated() {
    // What a newer release's `migrate` leaves behind, seen by this (older)
    // binary: a rollback, not an error.
    let mut applied = all_applied();
    applied.push(Applied {
        version: 99_991_231_000_000,
        checksum: vec![0],
        success: true,
    });
    assert_eq!(compare(&migrator(), &applied).verdict(), Ok(()));
}

#[test]
fn a_changed_checksum_is_refused() {
    let mut applied = all_applied();
    applied[0].checksum = vec![0];
    assert_eq!(
        compare(&migrator(), &applied).verdict(),
        Err(SchemaError::Changed(1))
    );
}

#[test]
fn a_failed_migration_is_refused_and_outranks_pending() {
    // One migration left failed, the rest never applied: re-running
    // `migrate` cannot fix that, so the verdict names the failure.
    let applied = vec![Applied {
        success: false,
        ..all_applied().remove(0)
    }];
    let status = compare(&migrator(), &applied);
    assert_eq!(status.dirty, 1);
    assert_eq!(status.pending, migration_files() - 1);
    assert_eq!(status.verdict(), Err(SchemaError::Dirty(1)));
}

#[test]
fn the_pending_refusal_names_migrate() {
    let message = SchemaError::Pending(2).to_string();
    assert!(message.contains("bored-backend migrate"), "{message}");
    assert_eq!(SchemaError::Pending(2).error_type(), "migration_pending");
}

// ── Against Postgres ─────────────────────────────────────────────────────────

/// The server refuses a database `migrate` has not run on, naming `migrate`,
/// and changes nothing while refusing. Then the same for a database that is
/// only part of the way there. The control at the end: once `migrate` has
/// run, the same connect succeeds.
#[tokio::test]
async fn the_server_refuses_a_database_with_pending_migrations() {
    let (pool, config) = empty_test_database().await.expect("test database");

    let refused = crate::db::connect(&config).await.err();
    assert_eq!(refused, Some(SchemaError::Pending(migration_files())));
    assert!(
        !migrations_table_exists(&pool).await,
        "the server's check must not create sqlx's table, let alone migrate"
    );

    // Part-way: only the first migration applied.
    migrator()
        .run_to(first_version(), &pool)
        .await
        .expect("apply the first migration");
    let refused = crate::db::connect(&config).await.err();
    assert_eq!(refused, Some(SchemaError::Pending(migration_files() - 1)));

    // Control: the check passes once `migrate` has done its job.
    run(&config).await.expect("migrate");
    crate::db::connect(&config)
        .await
        .expect("the server starts on a migrated database");
}

/// `migrate` applies everything the first time, and nothing the second: the
/// applied rows — `installed_on` included — are identical after the second
/// run. Every redeploy of the same image is that second run.
#[tokio::test]
async fn migrate_run_twice_is_a_no_op() {
    let (pool, config) = empty_test_database().await.expect("test database");

    run(&config).await.expect("first migrate");
    let first = applied_snapshot(&pool).await;
    // Control: the first run really did apply every migration, so the
    // equality below is not two empty lists.
    assert_eq!(first.len(), migration_files());
    assert!(first.iter().all(|(_, _, _, success)| *success));

    run(&config).await.expect("second migrate");
    assert_eq!(applied_snapshot(&pool).await, first);
}

/// A rollback: an older image's `migrate` — and then its server — run
/// against a database a newer release migrated. Both accept it. The control
/// is sqlx's default migrator, which refuses the very same state, so the
/// test is really exercising `set_ignore_missing`.
#[tokio::test]
async fn an_older_release_tolerates_newer_state() {
    let (pool, config) = empty_test_database().await.expect("test database");
    run(&config).await.expect("migrate");
    // What a newer release's `migrate` leaves behind.
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, success, checksum, execution_time) \
         VALUES (99991231000000, 'from a newer release', TRUE, '\\x00', 0)",
    )
    .execute(&pool)
    .await
    .expect("record a newer migration");

    run(&config)
        .await
        .expect("an older migrate tolerates a newer applied migration");
    crate::db::connect(&config)
        .await
        .expect("an older server tolerates a newer applied migration");

    // Control: without `set_ignore_missing`, sqlx refuses this database.
    let strict = sqlx::migrate!("./migrations");
    assert!(matches!(
        strict.run(&pool).await,
        Err(MigrateError::VersionMissing(99_991_231_000_000))
    ));
}

/// An applied migration whose recorded checksum differs from the file's:
/// both the server and `migrate` refuse it, with the class that says so.
#[tokio::test]
async fn a_changed_migration_stops_both_migrate_and_the_server() {
    let (pool, config) = empty_test_database().await.expect("test database");
    run(&config).await.expect("migrate");
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = $1")
        .bind(first_version())
        .execute(&pool)
        .await
        .expect("corrupt a checksum");

    assert_eq!(
        crate::db::connect(&config).await.err(),
        Some(SchemaError::Changed(1))
    );
    // Under a capturing subscriber, as every test that reaches the step's
    // failure line must be: with exactly one scoped subscriber alive,
    // tracing-core caches a callsite's interest from whichever thread
    // registers it first, so a run here with no subscriber could leave that
    // line switched off for `migrate_logs_why_it_failed` running alongside.
    let (outcome, logs) = crate::tests::capture_logs_async(run(&config)).await;
    assert_eq!(outcome, Err(SchemaError::Changed(1)));
    assert!(logs.contains("error.type=\"migration_changed\""), "{logs}");
}

/// A migration left failed (`success = false`) by an earlier `migrate`.
#[tokio::test]
async fn a_dirty_migration_stops_the_server() {
    let (pool, config) = empty_test_database().await.expect("test database");
    run(&config).await.expect("migrate");
    sqlx::query("UPDATE _sqlx_migrations SET success = FALSE WHERE version = $1")
        .bind(first_version())
        .execute(&pool)
        .await
        .expect("mark a migration failed");

    assert_eq!(
        crate::db::connect(&config).await.err(),
        Some(SchemaError::Dirty(1))
    );
}

/// An unreachable database fails `migrate` with the database's class, and
/// the failure is logged with its `error.type` — the line an operator reads
/// in `docker logs` of the exited container.
#[tokio::test]
async fn migrate_logs_why_it_failed() {
    let (_pool, mut config) = empty_test_database().await.expect("test database");
    // Nothing listens on port 1.
    config.host = "127.0.0.1".to_string();
    config.port = 1;

    let (outcome, logs) = crate::tests::capture_logs_async(run(&config)).await;
    assert_eq!(
        outcome,
        Err(SchemaError::Database(ErrorType::DbUnavailable))
    );
    assert!(logs.contains("migrate step failed"), "{logs}");
    assert!(logs.contains("error.type=\"db_unavailable\""), "{logs}");
    assert!(!logs.contains("migrations complete"), "{logs}");
}
