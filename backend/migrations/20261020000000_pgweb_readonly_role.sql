-- Card #473: the read-only role the SQL console (pgweb, `/dbconsole`) connects
-- as. Ported from v-note #299, whose spike established everything below.
--
-- This runs as the application's own database user, which the postgres image
-- creates as a **superuser**. That is precisely why this role has to exist: the
-- v-note spike (#299) pointed pgweb at the app's credentials in `--readonly` mode and
-- got `select pg_read_file('/etc/passwd', 0, 60)` back. pgweb's read-only mode
-- is a keyword filter over the submitted text plus a read-only transaction —
-- neither has anything to say about a `SELECT`, so "read-only" against a
-- superuser is arbitrary host-file read. The privilege boundary has to come from
-- Postgres, not from the client.
--
-- Created NOLOGIN and with no password. The deploy sets both (idempotently, on
-- every deploy) so the password never enters git; until it does, the role exists
-- and can be granted to but cannot connect. Environments that never provision a
-- password — local compose, the e2e stack before its console starts — simply
-- carry an unusable role, which is the intended resting state.
--
-- ⚠️ CONCURRENCY. A role is a **cluster-global** object, but a migration is
-- per-database, so several databases migrating at the same time all run this
-- file against one shared catalog. sqlx's own migration lock does not help:
-- it serialises migrations *within* a database, and this race is *across*
-- databases. The test harness does exactly that — one throwaway database per
-- test, migrated in parallel — and without the two measures below it fails with
-- `duplicate key value violates unique constraint "pg_authid_rolname_index"`
-- and `tuple concurrently updated`. Both are handled rather than retried,
-- because the operations are idempotent: whoever wins, the end state is
-- identical.

DO $$
BEGIN
  -- `IF NOT EXISTS` on its own is not enough: another database's migration can
  -- create the role between the check and the CREATE. Catching the duplicate is
  -- the only race-free form.
  BEGIN
    CREATE ROLE bored_pgweb NOLOGIN;
  EXCEPTION
    WHEN duplicate_object OR unique_violation THEN
      NULL;
  END;
END $$;

DO $$
DECLARE
  db text := quote_ident(current_database());
BEGIN
  -- Every environment names the database `bored` today, but that name is
  -- compose input rather than a constant, so it is read back rather than
  -- spelled out here.
  EXECUTE format('GRANT CONNECT ON DATABASE %s TO bored_pgweb', db);
END $$;

GRANT USAGE ON SCHEMA public TO bored_pgweb;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO bored_pgweb;

-- Covers tables that *future* migrations create, so this is a one-off rather
-- than a chore every later migration has to remember. Per-database
-- (pg_default_acl), so no cross-database race.
--
-- `CURRENT_USER` rather than a literal `bored`, for the same reason the
-- database name above is read back: the owning role is compose input
-- (`POSTGRES_USER`), not a constant. It also fails more gracefully — a wrong
-- database name would silently grant nothing, whereas a wrong role name raises
-- `role "…" does not exist`, failing the migration and so app startup in every
-- environment, including a local Postgres running under someone's own login.
-- Default privileges attach to the creating role, and every migration runs as
-- exactly this one, so `CURRENT_USER` *is* the property the grant depends on.
ALTER DEFAULT PRIVILEGES FOR ROLE CURRENT_USER IN SCHEMA public
  GRANT SELECT ON TABLES TO bored_pgweb;

-- The per-role settings, applied `IN DATABASE` rather than cluster-wide.
--
-- That is the second concurrency measure, and it is also the more correct
-- scoping. A bare `ALTER ROLE … SET` writes one shared `pg_db_role_setting` row
-- (setdatabase = 0) that every database's migration would fight over; `IN
-- DATABASE` writes a row per database, so parallel migrations touch different
-- rows and never collide. The console only ever connects to this database, so
-- nothing is lost by scoping it here.
DO $$
DECLARE
  db text := quote_ident(current_database());
BEGIN
  -- Defaults for a console session. Not a security boundary on their own: a
  -- session may `SET default_transaction_read_only = off` (and raise its own
  -- statement_timeout) for later transactions. The boundary is the grants
  -- above — SELECT only, so a read-write transaction still cannot write — and
  -- the hard query cap is pgweb's --query-timeout. Read-only by default still
  -- earns its place: a stray write fails on the transaction mode before it
  -- ever reaches a permission check.
  EXECUTE format(
    'ALTER ROLE bored_pgweb IN DATABASE %s SET default_transaction_read_only = on', db);

  -- A console invites the query that accidentally seq-scans the whole audit
  -- log; this default stops it, and pgweb's --query-timeout caps it regardless.
  EXECUTE format(
    'ALTER ROLE bored_pgweb IN DATABASE %s SET statement_timeout = ''30s''', db);
  EXECUTE format(
    'ALTER ROLE bored_pgweb IN DATABASE %s SET idle_in_transaction_session_timeout = ''60s''', db);

  -- The audit trail. pgweb's own request log records method/path/status but not
  -- the query text, so this is the only record of what was actually run. Scoped
  -- to this role so the application's own (very chatty) sessions are untouched.
  EXECUTE format(
    'ALTER ROLE bored_pgweb IN DATABASE %s SET log_statement = ''all''', db);
END $$;
