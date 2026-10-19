//! The read-only role the SQL console connects as (card #473,
//! `backend/migrations/20261020000000_pgweb_readonly_role.sql`).
//!
//! A role is cluster-global, and these tests share one Postgres server, so
//! none of them gives the role a login or a password — that would change it
//! for every test at once. Instead each test acts *as* the role with
//! `SET ROLE` inside its own session, which applies exactly the role's
//! privileges. Its per-database settings (read-only by default, timeouts),
//! which only apply at login, are read back from the catalog.

use super::*;

/// The SQLSTATE of a failed statement.
fn sqlstate(error: &sqlx::Error) -> Option<String> {
    match error {
        sqlx::Error::Database(database) => database.code().map(|code| code.into_owned()),
        _ => None,
    }
}

/// One connection acting as `bored_pgweb` for the rest of its life. The
/// returned connection is detached from the pool, so the role switch can
/// never leak into a connection another query borrows.
async fn as_console(db: &crate::db::Db) -> sqlx::PgConnection {
    let mut connection = db.acquire().await.expect("connection").detach();
    sqlx::query("SET ROLE bored_pgweb")
        .execute(&mut connection)
        .await
        .expect("SET ROLE");
    connection
}

#[tokio::test]
async fn the_console_role_exists_and_cannot_log_in_until_provisioned() {
    let db = db::connect_test().await.expect("test db");
    let (can_login, superuser, has_password): (bool, bool, bool) = sqlx::query_as(
        "SELECT rolcanlogin, rolsuper, rolpassword IS NOT NULL FROM pg_authid \
         WHERE rolname = 'bored_pgweb'",
    )
    .fetch_one(&db)
    .await
    .expect("the role exists");
    assert!(!can_login, "NOLOGIN until the deploy provisions it");
    assert!(!superuser);
    assert!(!has_password, "no password in git or in the migration");
}

#[tokio::test]
async fn the_console_role_can_read_but_not_write() {
    let db = db::connect_test().await.expect("test db");
    sqlx::query("INSERT INTO boards (id, name) VALUES ('b1', 'readable')")
        .execute(&db)
        .await
        .unwrap();
    let mut console = as_console(&db).await;

    let name: String = sqlx::query_scalar("SELECT name FROM boards WHERE id = 'b1'")
        .fetch_one(&mut console)
        .await
        .expect("SELECT is granted");
    assert_eq!(name, "readable");

    // 42501 is `insufficient_privilege`.
    for statement in [
        "INSERT INTO boards (id, name) VALUES ('b2', 'nope')",
        "UPDATE boards SET name = 'changed'",
        "DELETE FROM boards",
        "TRUNCATE audit_log",
        "CREATE TABLE sneaky (id int)",
    ] {
        let error = sqlx::query(statement)
            .execute(&mut console)
            .await
            .expect_err(statement);
        assert_eq!(sqlstate(&error).as_deref(), Some("42501"), "{statement}");
    }
    // Control: nothing changed underneath.
    let still: String = sqlx::query_scalar("SELECT name FROM boards WHERE id = 'b1'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(still, "readable");
}

/// The reason the role exists at all: pgweb's `--readonly` is a client-side
/// filter, and a superuser behind it can still read host files with a
/// `SELECT`. The role must not.
#[tokio::test]
async fn the_console_role_cannot_read_server_files() {
    let db = db::connect_test().await.expect("test db");
    // Control: the application's own (superuser) login can — which is what
    // makes pointing the console at it dangerous.
    sqlx::query("SELECT pg_read_file('PG_VERSION', 0, 10)")
        .execute(&db)
        .await
        .expect("the superuser can read server files");

    let mut console = as_console(&db).await;
    let error = sqlx::query("SELECT pg_read_file('PG_VERSION', 0, 10)")
        .execute(&mut console)
        .await
        .expect_err("the console role must not");
    assert_eq!(sqlstate(&error).as_deref(), Some("42501"));
}

/// Default privileges cover tables a later migration creates, so no future
/// migration has to remember to grant the console anything.
#[tokio::test]
async fn tables_created_after_the_migration_are_readable() {
    let db = db::connect_test().await.expect("test db");
    sqlx::query("CREATE TABLE added_later (id int)")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO added_later VALUES (7)")
        .execute(&db)
        .await
        .unwrap();
    let mut console = as_console(&db).await;
    let id: i32 = sqlx::query_scalar("SELECT id FROM added_later")
        .fetch_one(&mut console)
        .await
        .expect("default privileges grant SELECT");
    assert_eq!(id, 7);
}

/// The settings a console login gets in this database: read-only
/// transactions (a server-side guard that survives pgweb being misconfigured),
/// the statement and idle timeouts, and statement logging as the audit trail.
#[tokio::test]
async fn the_console_role_has_its_login_settings_in_this_database() {
    let db = db::connect_test().await.expect("test db");
    let settings: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(s.setconfig) FROM pg_db_role_setting s \
         JOIN pg_roles r ON r.oid = s.setrole \
         JOIN pg_database d ON d.oid = s.setdatabase \
         WHERE r.rolname = 'bored_pgweb' AND d.datname = current_database()",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    for expected in [
        "default_transaction_read_only=on",
        "statement_timeout=30s",
        "idle_in_transaction_session_timeout=60s",
        "log_statement=all",
    ] {
        assert!(
            settings.iter().any(|s| s == expected),
            "missing {expected} in {settings:?}"
        );
    }
}

/// The role is cluster-global but migrations are per database, so databases
/// migrating at the same moment race to create it. The migration's
/// `CREATE ROLE` block catches the duplicate rather than failing.
///
/// `bored_pgweb` itself already exists in a shared test cluster (and cannot be
/// dropped: other test databases' grants depend on it), so migrating more
/// databases would never reach the create. This runs the migration's *own*
/// block — read from the file, not retyped — against a fresh role name: eight
/// sessions at once, then once more after the role exists.
#[tokio::test]
async fn the_role_creation_survives_concurrent_and_repeated_runs() {
    let db = db::connect_test().await.expect("test db");
    let migration = include_str!("../../migrations/20261020000000_pgweb_readonly_role.sql");
    let start = migration.find("DO $$").expect("the CREATE ROLE block");
    let end = start + migration[start..].find("END $$;").expect("its end") + "END $$;".len();
    let block = &migration[start..end];
    assert!(block.contains("CREATE ROLE bored_pgweb NOLOGIN"), "{block}");

    let probe = format!(
        "race_probe_{}",
        ulid::Ulid::new().to_string().to_lowercase()
    );
    let statement = block.replace("bored_pgweb", &probe);
    let run = || async {
        let mut connection = db.acquire().await.expect("connection").detach();
        sqlx::query(sqlx::AssertSqlSafe(statement.clone()))
            .execute(&mut connection)
            .await
    };
    let results = futures_util::future::join_all((0..8).map(|_| run())).await;
    for result in results {
        result.expect("every concurrent run succeeds");
    }
    run()
        .await
        .expect("a run after the role exists succeeds too");

    // Control: a plain CREATE ROLE of the same, now existing, name fails —
    // which is what the block's exception handler absorbs.
    let plain = sqlx::query(sqlx::AssertSqlSafe(format!("CREATE ROLE {probe} NOLOGIN")))
        .execute(&db)
        .await;
    assert!(plain.is_err(), "the duplicate is real");

    sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE {probe}")))
        .execute(&db)
        .await
        .expect("clean up the probe role");
}
