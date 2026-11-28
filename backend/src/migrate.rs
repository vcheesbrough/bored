//! `bored-backend migrate`: the one-shot step every deploy runs before the
//! server is replaced (card #640, the machine-global `migrations` skill).
//!
//! bored changes one kind of state outside its image today: the **schema**,
//! the SQL migrations in `backend/migrations/` that `sqlx::migrate!` compiles
//! into the binary. So `migrate` has exactly one step, `schema`. A later
//! release that adds a backfill or external setup adds it here, after
//! `schema`, as its own step with its own root span (skill §2: schema →
//! stored data → external systems).
//!
//! The split of duties:
//!
//! * **`migrate` applies.** It runs as its own container from the same image
//!   (`deploy/docker-compose.yml`, service `migrate`), exits, and the server
//!   is only replaced once it has exited 0.
//! * **The server verifies.** At startup [`verify`] reads which migrations
//!   the database has and refuses to serve if any this binary embeds is
//!   missing, changed or left failed — naming `migrate` as the fix. It never
//!   changes the schema itself, so a deploy that skipped `migrate` fails
//!   loudly instead of looking healthy.
//!
//! Every migration must also work with the release *before* it, because
//! compose runs `migrate` while that release still serves (skill §2
//! "Compatibility"): expand in one release, contract in a later one.
//!
//! **Rollback** is deploying an older image. Its `migrate` then meets
//! migrations it does not know, applied by a newer release; [`migrator`]
//! tolerates them (`set_ignore_missing(true)`) and so does [`verify`].
//! Nothing is ever un-applied.

use sqlx::migrate::{MigrateError, Migrator};

use crate::config::DatabaseConfig;
use crate::db::{self, Db, DbOperation, DbQuery, Traced as _};
use crate::error::ErrorType;

/// How many connections the migrate step's pool may open. sqlx runs every
/// migration on the one connection that holds its advisory lock, and the
/// status reads below run before and after, never alongside it; the second
/// connection is headroom, not concurrency.
const MIGRATE_CONNECTIONS: u32 = 2;

/// The table sqlx records applied migrations in (its default name).
const MIGRATIONS_TABLE: &str = "_sqlx_migrations";

/// The embedded migrations, from `backend/migrations/`, compiled into the
/// binary, configured the way every caller here needs them.
///
/// A function rather than a `static`: `set_ignore_missing` is not a `const
/// fn`, and the `migrate!` expansion only points at data already baked into
/// the binary, so building one is cheap.
///
/// `set_ignore_missing(true)` makes an applied migration this binary does not
/// know a non-event instead of `MigrateError::VersionMissing`. That situation
/// is a rollback — an older image run against a database a newer one
/// migrated — and the skill requires the older `migrate` to tolerate it.
pub(crate) fn migrator() -> Migrator {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator
}

// ─────────────────────────────────────────────────────────────────────────────
// Why the schema step, or the server's check of it, failed
// ─────────────────────────────────────────────────────────────────────────────

/// Why the schema is not usable, as a fixed set of reasons.
///
/// Each variant has an `error.type` label ([`SchemaError::error_type`]), and
/// the `Display` text is written here, never taken from a driver error: a
/// Postgres message can quote the values involved (card #366's redaction
/// rule), so only the error's *class* crosses into a log line or span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaError {
    /// The database could not be reached or answered with an error; carries
    /// the same classification a request's `ApiError` would use.
    Database(ErrorType),
    /// Migrations this binary embeds have not been applied. The server's
    /// answer to a deploy that did not run `migrate` (or ran an older one).
    Pending(usize),
    /// Migrations are recorded as applied but under a different checksum:
    /// someone edited an applied migration file, or the database was
    /// migrated by a different build of the same version.
    Changed(usize),
    /// Migrations are recorded as started but not finished (`success =
    /// false`): an earlier `migrate` failed part-way. sqlx refuses to go on
    /// from there, and so does the server.
    Dirty(usize),
    /// Any other migration bookkeeping failure sqlx reports (a migration
    /// source that does not resolve, a version it cannot order).
    Failed,
}

impl SchemaError {
    /// The `error.type` value for the log line and the step's span. Every
    /// label is a literal; the `Database` arm reuses `ErrorType`'s labels so
    /// an unreachable database reads the same here as on a request.
    pub(crate) fn error_type(self) -> &'static str {
        match self {
            SchemaError::Database(error_type) => error_type.as_str(),
            SchemaError::Pending(_) => "migration_pending",
            SchemaError::Changed(_) => "migration_changed",
            SchemaError::Dirty(_) => "migration_dirty",
            SchemaError::Failed => "migration_failed",
        }
    }
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::Database(error_type) => {
                write!(f, "database unavailable ({})", error_type.as_str())
            }
            // The server's refusal: say what is wrong *and* what fixes it, so
            // the operator reading `docker logs` knows the next step.
            SchemaError::Pending(count) => write!(
                f,
                "the database schema is not current ({count} migration(s) pending); \
                 run `bored-backend migrate` (the deploy's `migrate` service) \
                 before starting the server"
            ),
            SchemaError::Changed(count) => write!(
                f,
                "{count} applied migration(s) differ from the ones this release \
                 carries; the database was migrated by another build"
            ),
            SchemaError::Dirty(count) => write!(
                f,
                "{count} migration(s) were left failed by an earlier `migrate`; \
                 fix forward and re-run the deploy"
            ),
            SchemaError::Failed => write!(f, "the schema migration failed"),
        }
    }
}

/// `?` on a sqlx call inside this module classifies it by its kind alone.
impl From<sqlx::Error> for SchemaError {
    fn from(error: sqlx::Error) -> Self {
        SchemaError::Database(crate::error::database_error_type(&error))
    }
}

/// What sqlx's migrator can fail with, by variant. Never formatted: an
/// `Execute` error is a database error whose message may quote values, and
/// the other variants carry nothing the label does not already say.
impl From<MigrateError> for SchemaError {
    fn from(error: MigrateError) -> Self {
        match error {
            // A statement in a migration, or sqlx's own bookkeeping query,
            // failed in the database.
            MigrateError::Execute(inner) | MigrateError::ExecuteMigration(inner, _) => inner.into(),
            MigrateError::VersionMismatch(_) => SchemaError::Changed(1),
            MigrateError::Dirty(_) => SchemaError::Dirty(1),
            _ => SchemaError::Failed,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Comparing what the database has applied with what this binary carries
// ─────────────────────────────────────────────────────────────────────────────

/// One row of `_sqlx_migrations`, as far as the comparison needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) version: i64,
    pub(crate) checksum: Vec<u8>,
    /// `false` while a migration is running, and for good if it failed
    /// part-way.
    pub(crate) success: bool,
}

/// How the database's applied migrations compare with this binary's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SchemaStatus {
    /// Embedded here, not applied there.
    pub(crate) pending: usize,
    /// Applied under a different checksum.
    pub(crate) changed: usize,
    /// Recorded but not successful.
    pub(crate) dirty: usize,
}

impl SchemaStatus {
    /// The server's verdict: `Ok` only when every embedded migration is
    /// applied, unchanged and finished. A changed or dirty record outranks a
    /// pending one — running `migrate` would not fix either.
    pub(crate) fn verdict(self) -> Result<(), SchemaError> {
        if self.dirty > 0 {
            Err(SchemaError::Dirty(self.dirty))
        } else if self.changed > 0 {
            Err(SchemaError::Changed(self.changed))
        } else if self.pending > 0 {
            Err(SchemaError::Pending(self.pending))
        } else {
            Ok(())
        }
    }
}

/// Compare the migrations `migrator` embeds with the `applied` rows.
///
/// A pure function, so the rules are unit-tested without a database:
///
/// * down migrations are skipped — bored has none, and sqlx never applies
///   them on the way up;
/// * an applied row with no embedded migration is **ignored**: it is a newer
///   release's migration, and this binary is the rollback;
/// * a failed row counts only when this binary embeds that version. A newer
///   release's half-applied migration is that release's problem; by the
///   compatibility rule this release still works on the shape before it.
pub(crate) fn compare(migrator: &Migrator, applied: &[Applied]) -> SchemaStatus {
    let mut status = SchemaStatus::default();
    for migration in migrator
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
    {
        match applied.iter().find(|row| row.version == migration.version) {
            None => status.pending += 1,
            Some(row) if !row.success => status.dirty += 1,
            Some(row) if row.checksum.as_slice() != &*migration.checksum => status.changed += 1,
            Some(_) => {}
        }
    }
    status
}

/// Read `_sqlx_migrations`, or nothing if sqlx has never run here.
///
/// Read-only, and both statements are plain catalogue or table reads, so the
/// server can run this with whatever login it holds.
async fn applied_migrations(db: &Db) -> Result<Vec<Applied>, sqlx::Error> {
    // `to_regclass` resolves the name on the search path exactly as sqlx's
    // own unqualified `CREATE TABLE IF NOT EXISTS _sqlx_migrations` did, and
    // answers NULL instead of failing when there is no such table — a
    // database nothing has migrated yet.
    let tracked: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(MIGRATIONS_TABLE)
        .fetch_one(db)
        .traced(DbQuery::new(
            "migrate.migrations_table_exists",
            DbOperation::Select,
            MIGRATIONS_TABLE,
        ))
        .await?;
    if !tracked {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64, Vec<u8>, bool)> =
        sqlx::query_as("SELECT version, checksum, success FROM _sqlx_migrations")
            .fetch_all(db)
            .traced(DbQuery::new(
                "migrate.applied_migrations",
                DbOperation::Select,
                MIGRATIONS_TABLE,
            ))
            .await?;
    Ok(rows
        .into_iter()
        .map(|(version, checksum, success)| Applied {
            version,
            checksum,
            success,
        })
        .collect())
}

/// How this binary's migrations stand against `db`.
pub(crate) async fn status(db: &Db) -> Result<SchemaStatus, SchemaError> {
    let applied = applied_migrations(db).await?;
    Ok(compare(&migrator(), &applied))
}

/// The server's half of the schema step: every migration this binary embeds
/// is applied, unchanged and finished. Changes nothing.
///
/// # Errors
///
/// The database cannot be read ([`SchemaError::Database`]), or the schema is
/// not this release's ([`SchemaStatus::verdict`]).
pub(crate) async fn verify(db: &Db) -> Result<(), SchemaError> {
    status(db).await?.verdict()
}

// ─────────────────────────────────────────────────────────────────────────────
// The migrate command
// ─────────────────────────────────────────────────────────────────────────────

/// Run every migrate step, in order, against the database `config` names.
/// Called by `main` for `bored-backend migrate`; the process exits non-zero
/// on `Err`, which is what fails the deploy.
///
/// # Errors
///
/// A step failed; the steps after it (none yet) do not run.
pub(crate) async fn run(config: &DatabaseConfig) -> Result<(), SchemaError> {
    // `connect_lazy_with` opens nothing here; the first status read is the
    // first connection, so an unreachable database fails inside the step's
    // span, where it is recorded.
    let db = db::open(config, MIGRATE_CONNECTIONS);
    let outcome = schema(&db).await;
    // Hand the connections back to Postgres before the process exits, rather
    // than leaving the server to notice the sockets closed.
    db.close().await;
    if outcome.is_ok() {
        tracing::info!("migrations complete");
    }
    outcome
}

/// The `schema` step: apply every embedded migration that is not applied
/// yet, inside its own root span.
///
/// Idempotent: sqlx records each migration it applies in `_sqlx_migrations`
/// and skips recorded ones, so a second run (every redeploy of the same
/// image) applies nothing. It takes a Postgres advisory lock for the run, so
/// two `migrate`s started at once apply each migration once.
///
/// The connection keeps the session limits every bored connection carries
/// (`db::with_session_limits`): in particular `lock_timeout`, so a migration
/// that needs a lock the serving release holds fails within seconds instead
/// of queueing every request behind it. A migration that genuinely needs
/// longer than `statement_timeout` sets its own with `SET LOCAL`.
async fn schema(db: &Db) -> Result<(), SchemaError> {
    use tracing::Instrument as _;

    // A root span (`parent: None`) per step, as the skill asks, so a trace
    // backend shows "migrate schema" as an operation of its own. The keys are
    // semconv names or `bored.`-prefixed, as every span's are.
    let span = tracing::info_span!(
        parent: None,
        "migrate schema",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        bored.migrate.step = "schema",
        bored.migrate.applied = tracing::field::Empty,
        error.type = tracing::field::Empty,
    );
    let recorder = span.clone();
    async move {
        let outcome = async {
            // Counted before rather than reported by sqlx, which does not say
            // what it applied.
            let pending = status(db).await?.pending;
            migrator().run(db).await?;
            // Read back rather than trusted: what the server will check at
            // its startup is what `migrate` checks before calling it done.
            verify(db).await?;
            Ok::<_, SchemaError>(pending)
        }
        .await;
        match outcome {
            Ok(applied) => {
                // `as i64`: the span bridge would export a `usize` field as a
                // string (see `numeric_span_fields_are_exported_as_integers`).
                recorder.record("bored.migrate.applied", applied as i64);
                tracing::info!(
                    bored.migrate.step = "schema",
                    bored.migrate.applied = applied as i64,
                    "migrate step complete: schema is current"
                );
                Ok(())
            }
            Err(error) => {
                recorder.record("error.type", error.error_type());
                recorder.record("otel.status_code", "ERROR");
                tracing::error!(
                    bored.migrate.step = "schema",
                    error.type = error.error_type(),
                    "migrate step failed: {error}"
                );
                Err(error)
            }
        }
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests;
