//! `bored-backend migrate-from-surreal`: the one-off copy of a SurrealDB
//! (SurrealKV) store into Postgres (card #471).
//!
//! **Temporary.** It exists to move dev and prod across once, and is deleted —
//! with the `surrealdb` dependency — by the follow-up card once both
//! environments are migrated. Nothing on the serving path calls it.
//!
//! # What it does
//!
//! 1. Refuses to touch a Postgres database that already holds bored data
//!    (`db::has_data`). The deploy runs it with `--check-empty` first to
//!    decide whether to run it at all (`scripts/migrate-from-surreal.sh`).
//! 2. Copies the store directory to a scratch directory and opens the *copy*,
//!    so the original volume is never written — SurrealKV may write to a store
//!    it opens.
//! 3. Reads every record of the five tables, plus the card counter. Only
//!    records: the ~824k internal `!ts` keys card #470 found are not records,
//!    so they are left behind.
//! 4. **Normalises** the audit snapshots: JSON written before card #472 holds
//!    timestamps as SurrealQL literals (`"d'2026-…Z'"`); each is rewritten to
//!    the API's RFC 3339 microsecond form, and the rewrites are counted.
//! 5. Writes everything in **one transaction**, then reads it back inside that
//!    transaction and compares per-table counts and a SHA-256 over every row
//!    with what it meant to write. Only a match commits; anything else rolls
//!    back and leaves Postgres empty.
//!
//! Rows SurrealDB let dangle — a card whose column is gone, a link to a
//! deleted card — cannot satisfy Postgres's foreign keys. They are left out,
//! and every one is reported by id in the summary, never silently.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use surrealdb::Surreal;
use surrealdb::engine::local::Db as SurrealDb;
use surrealdb::sql::{Datetime, Thing};

use crate::db::Db;
use crate::models::{DbAuditLog, DbBoard, DbCard, DbCardLink, DbColumn};

/// The exit status `--check-empty` uses for "Postgres already has data, do
/// not migrate". Distinct from 1 (any failure) so the deploy script can tell
/// "skip" from "broken".
pub const EXIT_NOT_EMPTY: u8 = 3;

/// Why the migration stopped. `Display` is the one line an operator sees; it
/// never quotes record content (the redaction rule, card #366).
#[derive(Debug)]
pub enum MigrationError {
    /// The target database already holds bored data.
    NotEmpty,
    Usage(&'static str),
    Io(std::io::Error),
    Surreal(surrealdb::Error),
    Postgres(sqlx::Error),
    /// A value the source holds cannot be represented in the target (an id
    /// that is not a string, a position outside `i32`).
    Unrepresentable(String),
    /// Read-back did not match what was written; nothing was committed.
    Verification(String),
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotEmpty => write!(
                f,
                "the target database already holds bored data; refusing to write into it"
            ),
            Self::Usage(message) => write!(f, "usage: {message}"),
            Self::Io(error) => write!(f, "file copy failed: {:?}", error.kind()),
            // Variant names only: a driver message can quote values.
            Self::Surreal(error) => {
                write!(
                    f,
                    "surrealdb: {}",
                    crate::redact::variant_path(&format!("{error:?}"))
                )
            }
            Self::Postgres(error) => write!(
                f,
                "{} ({})",
                crate::error::database_error_description(error),
                crate::error::database_error_type(error).as_str()
            ),
            Self::Unrepresentable(what) | Self::Verification(what) => f.write_str(what),
        }
    }
}

impl From<std::io::Error> for MigrationError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<surrealdb::Error> for MigrationError {
    fn from(error: surrealdb::Error) -> Self {
        Self::Surreal(error)
    }
}

impl From<sqlx::Error> for MigrationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Postgres(error)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Command line
// ─────────────────────────────────────────────────────────────────────────────

/// The parsed arguments after `migrate-from-surreal`.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `--check-empty`: exit 0 if the target is empty (migrate), or
    /// [`EXIT_NOT_EMPTY`] if not (skip).
    CheckEmpty,
    /// `--surreal <dir>`: migrate the store at `<dir>`.
    Migrate { surreal: PathBuf },
}

impl Command {
    pub fn parse(args: &[String]) -> Result<Self, MigrationError> {
        match args {
            [flag] if flag == "--check-empty" => Ok(Self::CheckEmpty),
            [flag, path] if flag == "--surreal" => Ok(Self::Migrate {
                surreal: PathBuf::from(path),
            }),
            _ => Err(MigrationError::Usage(
                "migrate-from-surreal (--check-empty | --surreal <store directory>)",
            )),
        }
    }
}

/// What a successful run did, for the summary log line (and the card).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Rows written per table, by table name.
    pub written: BTreeMap<&'static str, usize>,
    /// Ids left out because a parent they reference no longer exists.
    pub skipped: BTreeMap<&'static str, Vec<String>>,
    /// `d'…'` literals rewritten inside audit snapshots.
    pub snapshot_rewrites: usize,
    /// The value `card_number_seq` was set to: the next card gets this + 1.
    pub card_counter: i64,
    /// SHA-256 over every written row (hex), equal on both sides of the copy.
    pub checksum: String,
    /// Whether there was a store to migrate at all.
    pub source_found: bool,
    /// Cards that shared a number with an earlier card and were given a new
    /// one: `(id, old number, new number)`.
    pub renumbered: Vec<(String, i32, i32)>,
}

/// Run one subcommand against `target`. `Ok(code)` is the process exit
/// status for a run that did what it was asked.
pub async fn run(command: Command, target: &Db) -> Result<u8, MigrationError> {
    match command {
        Command::CheckEmpty => {
            if crate::db::has_data(target).await? {
                tracing::info!("postgres already holds bored data; no migration needed");
                Ok(EXIT_NOT_EMPTY)
            } else {
                tracing::info!("postgres is empty; a migration is needed");
                Ok(0)
            }
        }
        Command::Migrate { surreal } => {
            let report = migrate_store(&surreal, target).await?;
            log_report(&report);
            Ok(0)
        }
    }
}

/// The summary line, with everything the card's migration record needs.
fn log_report(report: &Report) {
    let written = serde_json::to_string(&report.written).unwrap_or_default();
    let skipped = serde_json::to_string(&report.skipped).unwrap_or_default();
    tracing::info!(
        bored.migration.source_found = report.source_found,
        bored.migration.written = %written,
        bored.migration.skipped = %skipped,
        bored.migration.renumbered = ?report.renumbered,
        bored.migration.snapshot_rewrites = report.snapshot_rewrites,
        bored.migration.card_counter = report.card_counter,
        bored.migration.checksum = %report.checksum,
        "surreal migration committed"
    );
}

/// Copy the store at `path` aside, open the copy, and migrate it.
async fn migrate_store(path: &Path, target: &Db) -> Result<Report, MigrationError> {
    if crate::db::has_data(target).await? {
        return Err(MigrationError::NotEmpty);
    }
    // A fresh environment's volume has no store in it: nothing to move.
    if !path.exists() {
        tracing::warn!("no SurrealDB store at the given path; nothing to migrate");
        return Ok(Report::default());
    }
    let scratch = std::env::temp_dir().join(format!(
        "bored-surreal-{}",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    copy_dir(path, &scratch)?;
    tracing::info!("copied the SurrealDB store aside; opening the copy");

    let result = async {
        let source = Surreal::new::<surrealdb::engine::local::SurrealKv>(
            scratch
                .to_str()
                .ok_or(MigrationError::Usage("store path is not UTF-8"))?,
        )
        .await?;
        source.use_ns("bored").use_db("bored").await?;
        migrate(&source, target).await
    }
    .await;
    // Best effort: the scratch copy lives in the container's own filesystem,
    // which goes when the one-shot container does anyway.
    let _ = std::fs::remove_dir_all(&scratch);
    let mut report = result?;
    report.source_found = true;
    Ok(report)
}

/// Recursively copy `from` (a directory) to `to`.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let destination = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Reading the source
// ─────────────────────────────────────────────────────────────────────────────

/// The source rows, shaped as the SurrealDB schema stored them
/// (`surreal_migration/legacy_schema.surql`). Positions and numbers are read
/// as `i64` — SurrealQL's `int` — and narrowed explicitly below.
#[derive(Debug, Deserialize)]
struct SourceBoard {
    id: Thing,
    name: String,
    last_edited_by: Option<String>,
    created_at: Datetime,
    updated_at: Datetime,
}

#[derive(Debug, Deserialize)]
struct SourceColumn {
    id: Thing,
    board: Thing,
    name: String,
    position: i64,
    last_edited_by: Option<String>,
    created_at: Datetime,
    updated_at: Datetime,
}

#[derive(Debug, Deserialize)]
struct SourceCard {
    id: Thing,
    column: Thing,
    body: String,
    position: i64,
    number: Option<i64>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    last_edited_by: Option<String>,
    created_at: Datetime,
    updated_at: Datetime,
}

#[derive(Debug, Deserialize)]
struct SourceLink {
    id: Thing,
    predecessor: Thing,
    successor: Thing,
    reason: Option<String>,
    last_edited_by: Option<String>,
    created_at: Datetime,
    updated_at: Datetime,
}

#[derive(Debug, Deserialize)]
struct SourceAudit {
    id: Thing,
    actor_sub: String,
    actor_display_name: String,
    entity_type: String,
    entity_id: String,
    board_id: String,
    action: String,
    snapshot_before: Option<Value>,
    snapshot_after: Option<Value>,
    restored_from: Option<String>,
    batch_group: Option<String>,
    #[serde(default)]
    audit_edit_session: Option<String>,
    created_at: Datetime,
}

/// A record id's key as the plain string the API has always used. bored only
/// ever created string keys (lower-case ULIDs); anything else cannot be a
/// `text` primary key and stops the migration.
fn key(thing: &Thing) -> Result<String, MigrationError> {
    match &thing.id {
        surrealdb::sql::Id::String(key) => Ok(key.clone()),
        _ => Err(MigrationError::Unrepresentable(format!(
            "a `{}` record has a non-string id",
            crate::redact::identifier(&thing.tb)
        ))),
    }
}

/// A SurrealDB instant, truncated to the microsecond Postgres stores. Done
/// here rather than left to the driver so the checksum compares exactly what
/// was written.
fn instant(datetime: &Datetime) -> DateTime<Utc> {
    truncate_to_micros(datetime.0)
}

fn truncate_to_micros(instant: DateTime<Utc>) -> DateTime<Utc> {
    // `timestamp_micros` floors to whole microseconds; converting back loses
    // only the sub-microsecond part. It cannot fail for an instant chrono
    // already holds, but fall back to the original rather than panic.
    DateTime::from_timestamp_micros(instant.timestamp_micros()).unwrap_or(instant)
}

fn narrow(value: i64, what: &str) -> Result<i32, MigrationError> {
    i32::try_from(value)
        .map_err(|_| MigrationError::Unrepresentable(format!("a {what} does not fit in i32")))
}

/// Everything read from the source, already converted to the target's row
/// types — what will be written, and what the read-back must equal.
#[derive(Debug, Default)]
struct Rows {
    boards: Vec<DbBoard>,
    columns: Vec<DbColumn>,
    cards: Vec<DbCard>,
    links: Vec<DbCardLink>,
    audit: Vec<DbAuditLog>,
}

/// Read every record of every table and convert it, dropping dangling rows.
async fn read_source(
    source: &Surreal<SurrealDb>,
    report: &mut Report,
) -> Result<Rows, MigrationError> {
    let boards: Vec<SourceBoard> = source.query("SELECT * FROM boards").await?.take(0)?;
    let columns: Vec<SourceColumn> = source.query("SELECT * FROM columns").await?.take(0)?;
    let cards: Vec<SourceCard> = source.query("SELECT * FROM cards").await?.take(0)?;
    let links: Vec<SourceLink> = source.query("SELECT * FROM card_links").await?.take(0)?;
    let audit: Vec<SourceAudit> = source.query("SELECT * FROM audit_log").await?.take(0)?;
    let counter: Vec<i64> = source
        .query("SELECT VALUE count FROM card_counter")
        .await?
        .take(0)?;

    let mut rows = Rows::default();
    let mut skipped: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();

    for board in boards {
        rows.boards.push(DbBoard {
            id: key(&board.id)?,
            name: board.name,
            last_edited_by: board.last_edited_by,
            created_at: instant(&board.created_at),
            updated_at: instant(&board.updated_at),
        });
    }
    let board_ids: HashSet<&str> = rows.boards.iter().map(|b| b.id.as_str()).collect();

    let mut kept_columns = Vec::new();
    for column in columns {
        let id = key(&column.id)?;
        let board_id = key(&column.board)?;
        if !board_ids.contains(board_id.as_str()) {
            skipped.entry("columns").or_default().push(id);
            continue;
        }
        kept_columns.push(DbColumn {
            id,
            board_id,
            name: column.name,
            position: narrow(column.position, "column position")?,
            last_edited_by: column.last_edited_by,
            created_at: instant(&column.created_at),
            updated_at: instant(&column.updated_at),
        });
    }
    rows.columns = kept_columns;
    let column_ids: HashSet<&str> = rows.columns.iter().map(|c| c.id.as_str()).collect();

    let mut kept_cards = Vec::new();
    for card in cards {
        let id = key(&card.id)?;
        let column_id = key(&card.column)?;
        if !column_ids.contains(column_id.as_str()) {
            skipped.entry("cards").or_default().push(id);
            continue;
        }
        // The SurrealDB schema numbered every card at startup, so a missing or
        // zero number means the store was never opened by a numbering
        // backend — not something to invent a number for silently.
        let number = card.number.filter(|n| *n > 0).ok_or_else(|| {
            MigrationError::Unrepresentable(format!(
                "card {} has no number",
                crate::redact::identifier(&id)
            ))
        })?;
        kept_cards.push(DbCard {
            id,
            column_id,
            body: card.body,
            position: narrow(card.position, "card position")?,
            number: narrow(number, "card number")?,
            tags: card.tags.unwrap_or_default(),
            last_edited_by: card.last_edited_by,
            created_at: instant(&card.created_at),
            updated_at: instant(&card.updated_at),
        });
    }
    rows.cards = kept_cards;

    // The counter's high-water mark, never below the highest number in use —
    // `card_number_seq` must not hand out a number a migrated card holds.
    let highest = rows
        .cards
        .iter()
        .map(|c| i64::from(c.number))
        .max()
        .unwrap_or(0);
    let mut next_number = counter.into_iter().max().unwrap_or(0).max(highest);

    // SurrealDB never enforced unique numbers, and an early race (card #456)
    // gave a few cards the same one. Postgres does enforce it
    // (`cards_number_unique`), so each duplicated number stays with the card
    // that was created first and every later holder gets a fresh number above
    // the counter — reported by id, never silent.
    let mut by_age: Vec<&mut DbCard> = rows.cards.iter_mut().collect();
    by_age.sort_by(|a, b| (a.number, a.created_at, &a.id).cmp(&(b.number, b.created_at, &b.id)));
    let mut previous: Option<i32> = None;
    for card in by_age {
        if previous == Some(card.number) {
            next_number += 1;
            let new = narrow(next_number, "card number")?;
            report.renumbered.push((card.id.clone(), card.number, new));
            card.number = new;
        } else {
            previous = Some(card.number);
        }
    }
    report.card_counter = next_number;

    let card_numbers: HashMap<&str, i32> = rows
        .cards
        .iter()
        .map(|c| (c.id.as_str(), c.number))
        .collect();

    let mut kept_links = Vec::new();
    for link in links {
        let id = key(&link.id)?;
        let predecessor_id = key(&link.predecessor)?;
        let successor_id = key(&link.successor)?;
        let (Some(&predecessor_number), Some(&successor_number)) = (
            card_numbers.get(predecessor_id.as_str()),
            card_numbers.get(successor_id.as_str()),
        ) else {
            skipped.entry("card_links").or_default().push(id);
            continue;
        };
        kept_links.push(DbCardLink {
            id,
            predecessor_id,
            successor_id,
            predecessor_number,
            successor_number,
            reason: link.reason,
            last_edited_by: link.last_edited_by,
            created_at: instant(&link.created_at),
            updated_at: instant(&link.updated_at),
        });
    }
    rows.links = kept_links;

    for row in audit {
        let snapshot_before = row
            .snapshot_before
            .map(|snapshot| normalise_snapshot(snapshot, &mut report.snapshot_rewrites));
        let snapshot_after = row
            .snapshot_after
            .map(|snapshot| normalise_snapshot(snapshot, &mut report.snapshot_rewrites));
        rows.audit.push(DbAuditLog {
            id: key(&row.id)?,
            actor_sub: row.actor_sub,
            actor_display_name: row.actor_display_name,
            entity_type: row.entity_type,
            entity_id: row.entity_id,
            board_id: row.board_id,
            action: row.action,
            snapshot_before: snapshot_before.map(Json),
            snapshot_after: snapshot_after.map(Json),
            restored_from: row.restored_from,
            batch_group: row.batch_group,
            audit_edit_session: row.audit_edit_session,
            created_at: instant(&row.created_at),
        });
    }

    report.skipped = skipped;

    // Stable order on both sides of the checksum.
    rows.boards.sort_by(|a, b| a.id.cmp(&b.id));
    rows.columns.sort_by(|a, b| a.id.cmp(&b.id));
    rows.cards.sort_by(|a, b| a.id.cmp(&b.id));
    rows.links.sort_by(|a, b| a.id.cmp(&b.id));
    rows.audit.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(rows)
}

// ─────────────────────────────────────────────────────────────────────────────
// Snapshot normalisation
// ─────────────────────────────────────────────────────────────────────────────

/// Rewrite every SurrealQL datetime literal inside `value` (recursively) to
/// the API's RFC 3339 microsecond form, adding one to `rewrites` per literal.
/// Every other value passes through unchanged.
pub(crate) fn normalise_snapshot(value: Value, rewrites: &mut usize) -> Value {
    match value {
        Value::String(text) => match normalise_literal(&text) {
            Some(normalised) => {
                *rewrites += 1;
                Value::String(normalised)
            }
            None => Value::String(text),
        },
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| normalise_snapshot(item, rewrites))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, field)| (name, normalise_snapshot(field, rewrites)))
                .collect(),
        ),
        other => other,
    }
}

/// `Some(rfc3339 µs)` when `text` is exactly a SurrealQL datetime literal —
/// `d'` + an RFC 3339 instant + `'` — and `None` for anything else, including
/// prose that merely starts with `d'` (a card body is never touched: the
/// inside must parse as an instant).
fn normalise_literal(text: &str) -> Option<String> {
    let inner = text.strip_prefix("d'")?.strip_suffix('\'')?;
    let parsed = DateTime::parse_from_rfc3339(inner)
        .ok()?
        .with_timezone(&Utc);
    Some(crate::timestamp::api_timestamp(&parsed))
}

// ─────────────────────────────────────────────────────────────────────────────
// Writing and verifying
// ─────────────────────────────────────────────────────────────────────────────

/// One table's rows, rendered for the checksum: each row as its API JSON on
/// its own line, in id order. The API form covers every stored column (the
/// audit snapshots included) in the exact format the API serves.
fn table_digest(hasher: &mut Sha256, table: &str, rows: impl Iterator<Item = Value>) {
    hasher.update(table.as_bytes());
    hasher.update(b"\n");
    for row in rows {
        hasher.update(canonical(row).to_string().as_bytes());
        hasher.update(b"\n");
    }
}

/// `value` with every object's keys in sorted order, recursively.
///
/// serde_json is built with `preserve_order` in this workspace, so a map
/// renders its keys in insertion order — and `jsonb` stores an object's keys
/// in its own order, not the order they were written in. The same snapshot
/// read back from Postgres would otherwise hash differently from the one
/// written. Sorting makes the rendering a function of the content alone.
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut sorted: Vec<(String, Value)> = fields.into_iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(name, field)| (name, canonical(field)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

/// The checksum of a full set of rows (see [`table_digest`]).
fn checksum(rows: &Rows) -> Result<String, MigrationError> {
    let to_json = |value: Result<Value, serde_json::Error>| {
        value.map_err(|_| MigrationError::Unrepresentable("a row did not serialise".to_string()))
    };
    let mut hasher = Sha256::new();
    let boards = rows
        .boards
        .iter()
        .map(|r| to_json(serde_json::to_value(r.clone().into_api())))
        .collect::<Result<Vec<_>, _>>()?;
    table_digest(&mut hasher, "boards", boards.into_iter());
    let columns = rows
        .columns
        .iter()
        .map(|r| to_json(serde_json::to_value(r.clone().into_api())))
        .collect::<Result<Vec<_>, _>>()?;
    table_digest(&mut hasher, "columns", columns.into_iter());
    let cards = rows
        .cards
        .iter()
        .map(|r| to_json(serde_json::to_value(r.clone().into_api())))
        .collect::<Result<Vec<_>, _>>()?;
    table_digest(&mut hasher, "cards", cards.into_iter());
    let links = rows
        .links
        .iter()
        .map(|r| to_json(serde_json::to_value(r.clone().into_api())))
        .collect::<Result<Vec<_>, _>>()?;
    table_digest(&mut hasher, "card_links", links.into_iter());
    let audit = rows
        .audit
        .iter()
        .map(|r| to_json(serde_json::to_value(r.clone().into_api())))
        .collect::<Result<Vec<_>, _>>()?;
    table_digest(&mut hasher, "audit_log", audit.into_iter());
    // `{:x}` on the digest bytes renders lowercase hex.
    Ok(format!("{:x}", hasher.finalize()))
}

/// Copy `source` into `target` in one transaction, verified before commit.
pub(crate) async fn migrate(
    source: &Surreal<SurrealDb>,
    target: &Db,
) -> Result<Report, MigrationError> {
    let mut report = Report::default();
    let rows = read_source(source, &mut report).await?;
    let expected = checksum(&rows)?;

    let mut tx = target.begin().await?;
    // Checked again inside the transaction, so a backend that started writing
    // since the first check cannot be overwritten.
    let non_empty: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM boards) OR EXISTS (SELECT 1 FROM audit_log)",
    )
    .fetch_one(&mut *tx)
    .await?;
    if non_empty {
        return Err(MigrationError::NotEmpty);
    }

    for board in &rows.boards {
        sqlx::query(
            "INSERT INTO boards (id, name, last_edited_by, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&board.id)
        .bind(&board.name)
        .bind(&board.last_edited_by)
        .bind(board.created_at)
        .bind(board.updated_at)
        .execute(&mut *tx)
        .await?;
    }
    for column in &rows.columns {
        sqlx::query(
            "INSERT INTO columns (id, board_id, name, position, last_edited_by, created_at, \
             updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&column.id)
        .bind(&column.board_id)
        .bind(&column.name)
        .bind(column.position)
        .bind(&column.last_edited_by)
        .bind(column.created_at)
        .bind(column.updated_at)
        .execute(&mut *tx)
        .await?;
    }
    for card in &rows.cards {
        sqlx::query(
            "INSERT INTO cards (id, column_id, body, position, number, tags, last_edited_by, \
             created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&card.id)
        .bind(&card.column_id)
        .bind(&card.body)
        .bind(card.position)
        .bind(card.number)
        .bind(&card.tags)
        .bind(&card.last_edited_by)
        .bind(card.created_at)
        .bind(card.updated_at)
        .execute(&mut *tx)
        .await?;
    }
    for link in &rows.links {
        sqlx::query(
            "INSERT INTO card_links (id, predecessor_id, successor_id, reason, last_edited_by, \
             created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&link.id)
        .bind(&link.predecessor_id)
        .bind(&link.successor_id)
        .bind(&link.reason)
        .bind(&link.last_edited_by)
        .bind(link.created_at)
        .bind(link.updated_at)
        .execute(&mut *tx)
        .await?;
    }
    for row in &rows.audit {
        sqlx::query(
            "INSERT INTO audit_log (id, actor_sub, actor_display_name, entity_type, entity_id, \
             board_id, action, snapshot_before, snapshot_after, restored_from, batch_group, \
             audit_edit_session, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(&row.id)
        .bind(&row.actor_sub)
        .bind(&row.actor_display_name)
        .bind(&row.entity_type)
        .bind(&row.entity_id)
        .bind(&row.board_id)
        .bind(&row.action)
        .bind(&row.snapshot_before)
        .bind(&row.snapshot_after)
        .bind(&row.restored_from)
        .bind(&row.batch_group)
        .bind(&row.audit_edit_session)
        .bind(row.created_at)
        .execute(&mut *tx)
        .await?;
    }
    // `setval(seq, n)` makes the next `nextval` return n + 1 — exactly where
    // SurrealDB's counter would have gone next. A sequence cannot be set to 0,
    // so an empty store leaves it at its start (1).
    if report.card_counter > 0 {
        sqlx::query("SELECT setval('card_number_seq', $1)")
            .bind(report.card_counter)
            .execute(&mut *tx)
            .await?;
    }

    // Read everything back through the same row types the backend uses.
    let stored = Rows {
        boards: sqlx::query_as("SELECT * FROM boards ORDER BY id")
            .fetch_all(&mut *tx)
            .await?,
        columns: sqlx::query_as("SELECT * FROM columns ORDER BY id")
            .fetch_all(&mut *tx)
            .await?,
        cards: sqlx::query_as("SELECT * FROM cards ORDER BY id")
            .fetch_all(&mut *tx)
            .await?,
        links: sqlx::query_as(
            "SELECT l.*, p.number AS predecessor_number, s.number AS successor_number \
             FROM card_links l JOIN cards p ON p.id = l.predecessor_id \
             JOIN cards s ON s.id = l.successor_id ORDER BY l.id",
        )
        .fetch_all(&mut *tx)
        .await?,
        audit: sqlx::query_as("SELECT * FROM audit_log ORDER BY id")
            .fetch_all(&mut *tx)
            .await?,
    };
    let counts = [
        ("boards", rows.boards.len(), stored.boards.len()),
        ("columns", rows.columns.len(), stored.columns.len()),
        ("cards", rows.cards.len(), stored.cards.len()),
        ("card_links", rows.links.len(), stored.links.len()),
        ("audit_log", rows.audit.len(), stored.audit.len()),
    ];
    for (table, wanted, found) in counts {
        if wanted != found {
            // Dropping `tx` here rolls everything back.
            return Err(MigrationError::Verification(format!(
                "{table}: wrote {wanted} rows but read back {found}"
            )));
        }
        report.written.insert(table, found);
    }
    let actual = checksum(&stored)?;
    if actual != expected {
        return Err(MigrationError::Verification(format!(
            "checksum mismatch: source {expected}, postgres {actual}"
        )));
    }
    report.checksum = actual;

    tx.commit().await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A source store with the real legacy schema applied, in memory.
    async fn legacy_store() -> Surreal<SurrealDb> {
        let source = Surreal::new::<surrealdb::engine::local::Mem>(())
            .await
            .expect("mem store");
        source.use_ns("bored").use_db("bored").await.expect("ns/db");
        source
            .query(include_str!("surreal_migration/legacy_schema.surql"))
            .await
            .expect("schema runs")
            .check()
            .expect("schema applies");
        source
    }

    /// Run one SurrealQL statement, failing the test on any error.
    async fn exec(source: &Surreal<SurrealDb>, statement: &str) {
        source
            .query(statement)
            .await
            .expect("statement runs")
            .check()
            .expect("statement succeeds");
    }

    /// A store shaped like production's: two boards, a column, cards with
    /// tags and numbers, a link, audit rows with pre-#472 `d'…'` snapshots,
    /// and — the dangling rows SurrealDB allowed — a card whose column is gone
    /// and a link to a deleted card.
    async fn seeded_store() -> Surreal<SurrealDb> {
        let source = legacy_store().await;
        exec(
            &source,
            "CREATE boards:b1 SET name = 'main', last_edited_by = 'alice', \
                 created_at = d'2026-04-18T23:35:44.644413966Z';
             UPDATE boards:b1 SET name = 'main';
             CREATE boards:b2 SET name = 'other';
             CREATE columns:c1 SET board = boards:b1, name = 'Todo', position = 0;
             CREATE cards:k1 SET column = columns:c1, body = \"first, it's d'oh\", \
                 position = 1024, number = 7, tags = ['bug', 'ux'];
             CREATE cards:k2 SET column = columns:c1, body = 'second', position = 2048, number = 9;
             CREATE cards:orphan SET column = columns:gone, body = 'lost', position = 1, number = 3;
             CREATE card_links:l1 SET predecessor = cards:k1, successor = cards:k2, reason = 'needs';
             CREATE card_links:dangling SET predecessor = cards:k1, successor = cards:deleted;
             UPDATE card_counter:global SET count = 12;
             CREATE audit_log:a1 SET actor_sub = 'alice', actor_display_name = 'Alice', \
                 entity_type = 'card', entity_id = 'k1', board_id = 'b1', action = 'update', \
                 snapshot_before = { id: 'k1', body: 'old', \
                     created_at: \"d'2026-05-07T01:27:04.823026281Z'\", \
                     updated_at: \"d'2026-05-07T01:27:05Z'\" }, \
                 snapshot_after = { id: 'k1', body: \"d'not a date'\", \
                     created_at: '2026-05-07T01:27:04.823026Z', \
                     nested: [\"d'2026-01-01T00:00:00.5Z'\"] }, \
                 created_at = d'2026-05-07T01:27:06.123456789Z';",
        )
        .await;
        source
    }

    #[tokio::test]
    async fn a_store_round_trips_with_normalised_snapshots() {
        let source = seeded_store().await;
        let target = crate::db::connect_test().await.expect("target db");

        let report = migrate(&source, &target).await.expect("migration succeeds");

        // Counts: dangling rows are left out, and reported by id.
        assert_eq!(report.written["boards"], 2);
        assert_eq!(report.written["columns"], 1);
        assert_eq!(report.written["cards"], 2);
        assert_eq!(report.written["card_links"], 1);
        assert_eq!(report.written["audit_log"], 1);
        assert_eq!(report.skipped["cards"], vec!["orphan".to_string()]);
        assert_eq!(report.skipped["card_links"], vec!["dangling".to_string()]);
        // Three literals: two in `snapshot_before`, one nested in an array.
        // `d'not a date'` is prose that happens to look like one and stays.
        assert_eq!(report.snapshot_rewrites, 3);
        assert_eq!(report.card_counter, 12);
        assert_eq!(report.checksum.len(), 64);

        // The values themselves, written out by hand (an independent oracle).
        let board: DbBoard = sqlx::query_as("SELECT * FROM boards WHERE id = 'b1'")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(board.name, "main");
        assert_eq!(board.last_edited_by.as_deref(), Some("alice"));
        // Nanoseconds truncated to the microsecond Postgres keeps.
        assert_eq!(
            crate::timestamp::api_timestamp(&board.created_at),
            "2026-04-18T23:35:44.644413Z"
        );

        let card: DbCard = sqlx::query_as("SELECT * FROM cards WHERE id = 'k1'")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(card.number, 7);
        assert_eq!(card.position, 1024);
        assert_eq!(card.column_id, "c1");
        assert_eq!(card.tags, vec!["bug".to_string(), "ux".to_string()]);
        assert_eq!(card.body, "first, it's d'oh");
        let untagged: DbCard = sqlx::query_as("SELECT * FROM cards WHERE id = 'k2'")
            .fetch_one(&target)
            .await
            .unwrap();
        assert!(untagged.tags.is_empty());

        let link: (String, String, Option<String>) = sqlx::query_as(
            "SELECT predecessor_id, successor_id, reason FROM card_links WHERE id = 'l1'",
        )
        .fetch_one(&target)
        .await
        .unwrap();
        assert_eq!(link, ("k1".into(), "k2".into(), Some("needs".into())));

        let audit: DbAuditLog = sqlx::query_as("SELECT * FROM audit_log WHERE id = 'a1'")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(
            audit.snapshot_before.unwrap().0,
            json!({
                "id": "k1",
                "body": "old",
                "created_at": "2026-05-07T01:27:04.823026Z",
                "updated_at": "2026-05-07T01:27:05.000000Z",
            })
        );
        assert_eq!(
            audit.snapshot_after.unwrap().0,
            json!({
                "id": "k1",
                "body": "d'not a date'",
                "created_at": "2026-05-07T01:27:04.823026Z",
                "nested": ["2026-01-01T00:00:00.500000Z"],
            })
        );
        assert_eq!(
            crate::timestamp::api_timestamp(&audit.created_at),
            "2026-05-07T01:27:06.123456Z"
        );

        // The counter carries on from SurrealDB's high-water mark: the next
        // card created through the API is number 13.
        let next: i64 = sqlx::query_scalar("SELECT nextval('card_number_seq')")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(next, 13);
    }

    #[tokio::test]
    async fn the_migration_refuses_a_database_that_already_has_data() {
        let source = seeded_store().await;
        let target = crate::db::connect_test().await.expect("target db");
        sqlx::query("INSERT INTO boards (id, name) VALUES ('existing', 'existing')")
            .execute(&target)
            .await
            .unwrap();

        let error = migrate(&source, &target).await.expect_err("must refuse");
        assert!(matches!(error, MigrationError::NotEmpty), "{error}");
        // Nothing was added to what was already there.
        let boards: i64 = sqlx::query_scalar("SELECT count(*) FROM boards")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(boards, 1);
    }

    #[tokio::test]
    async fn a_failed_migration_writes_nothing() {
        // A row that violates a constraint part way through the copy (two
        // boards sharing a name: the legacy schema file never defined the
        // unique index, which SurrealDB's startup code added) must leave
        // nothing behind — the whole copy is one transaction.
        let source = legacy_store().await;
        exec(
            &source,
            "CREATE boards:b1 SET name = 'main';
             CREATE columns:c1 SET board = boards:b1, name = 'Todo', position = 0;
             CREATE boards:b2 SET name = 'main';",
        )
        .await;
        let target = crate::db::connect_test().await.expect("target db");

        migrate(&source, &target)
            .await
            .expect_err("duplicate names fail");
        assert!(!crate::db::has_data(&target).await.unwrap());
    }

    /// What Postgres stores differing from what was meant to be written —
    /// here a trigger quietly altering board names — fails verification, and
    /// the transaction leaves nothing behind. This is the import's central
    /// guarantee, so it is driven for real rather than assumed.
    #[tokio::test]
    async fn a_checksum_mismatch_rolls_everything_back() {
        let source = seeded_store().await;
        let target = crate::db::connect_test().await.expect("target db");
        for statement in [
            "CREATE FUNCTION tamper() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN NEW.name := NEW.name || '-tampered'; RETURN NEW; END $$",
            "CREATE TRIGGER tamper BEFORE INSERT ON boards \
             FOR EACH ROW EXECUTE FUNCTION tamper()",
        ] {
            sqlx::query(statement)
                .execute(&target)
                .await
                .expect("trigger");
        }

        let error = migrate(&source, &target)
            .await
            .expect_err("a tampered write must not verify");
        assert!(
            matches!(&error, MigrationError::Verification(message) if message.contains("checksum")),
            "{error}"
        );
        assert!(!crate::db::has_data(&target).await.unwrap());
    }

    /// Duplicate card numbers keep the number on the earliest card and give
    /// the later ones fresh numbers above the counter.
    #[tokio::test]
    async fn duplicate_card_numbers_are_renumbered_after_the_counter() {
        let source = legacy_store().await;
        exec(
            &source,
            "CREATE boards:b1 SET name = 'main';
             CREATE columns:c1 SET board = boards:b1, name = 'Todo', position = 0;
             CREATE cards:older SET column = columns:c1, body = 'a', position = 1, number = 5, \
                 created_at = d'2026-04-19T18:57:40.388174Z';
             CREATE cards:newer SET column = columns:c1, body = 'b', position = 2, number = 5, \
                 created_at = d'2026-04-19T18:57:40.388219Z';
             CREATE cards:other SET column = columns:c1, body = 'c', position = 3, number = 6;
             CREATE card_links:l1 SET predecessor = cards:older, successor = cards:newer;
             UPDATE card_counter:global SET count = 8;",
        )
        .await;
        let target = crate::db::connect_test().await.expect("target db");

        let report = migrate(&source, &target).await.expect("migration succeeds");

        assert_eq!(report.renumbered, vec![("newer".to_string(), 5, 9)]);
        assert_eq!(report.card_counter, 9);
        let numbers: Vec<(String, i32)> =
            sqlx::query_as("SELECT id, number FROM cards ORDER BY id")
                .fetch_all(&target)
                .await
                .unwrap();
        assert_eq!(
            numbers,
            vec![
                ("newer".to_string(), 9),
                ("older".to_string(), 5),
                ("other".to_string(), 6),
            ]
        );
        // The link reads the renumbered card's new number.
        let successor_number: i32 = sqlx::query_scalar(
            "SELECT s.number FROM card_links l JOIN cards s ON s.id = l.successor_id",
        )
        .fetch_one(&target)
        .await
        .unwrap();
        assert_eq!(successor_number, 9);
        let next: i64 = sqlx::query_scalar("SELECT nextval('card_number_seq')")
            .fetch_one(&target)
            .await
            .unwrap();
        assert_eq!(next, 10);
    }

    #[tokio::test]
    async fn check_empty_reports_through_the_exit_code() {
        let target = crate::db::connect_test().await.expect("target db");
        assert_eq!(run(Command::CheckEmpty, &target).await.unwrap(), 0);

        sqlx::query("INSERT INTO boards (id, name) VALUES ('b', 'b')")
            .execute(&target)
            .await
            .unwrap();
        assert_eq!(
            run(Command::CheckEmpty, &target).await.unwrap(),
            EXIT_NOT_EMPTY
        );
    }

    #[tokio::test]
    async fn a_missing_store_migrates_nothing() {
        let target = crate::db::connect_test().await.expect("target db");
        let missing = std::env::temp_dir().join("bored-no-such-store");
        let code = run(Command::Migrate { surreal: missing }, &target)
            .await
            .expect("a missing store is not an error");
        assert_eq!(code, 0);
        assert!(!crate::db::has_data(&target).await.unwrap());
    }

    #[test]
    fn arguments_parse() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            Command::parse(&args(&["--check-empty"])).unwrap(),
            Command::CheckEmpty
        );
        assert_eq!(
            Command::parse(&args(&["--surreal", "/surreal/bored.db"])).unwrap(),
            Command::Migrate {
                surreal: PathBuf::from("/surreal/bored.db")
            }
        );
        assert!(Command::parse(&args(&[])).is_err());
        assert!(Command::parse(&args(&["--surreal"])).is_err());
    }

    /// Two renderings of one object with its keys in different orders hash
    /// the same — what lets a `jsonb` snapshot verify against its source.
    #[test]
    fn the_checksum_ignores_object_key_order() {
        let digest = |value: Value| {
            let mut hasher = Sha256::new();
            table_digest(&mut hasher, "t", std::iter::once(value));
            format!("{:x}", hasher.finalize())
        };
        let mut forwards = serde_json::Map::new();
        forwards.insert("a".into(), json!(1));
        forwards.insert("b".into(), json!({ "x": 1, "y": 2 }));
        let mut backwards = serde_json::Map::new();
        backwards.insert("b".into(), json!({ "y": 2, "x": 1 }));
        backwards.insert("a".into(), json!(1));
        assert_eq!(
            digest(Value::Object(forwards.clone())),
            digest(Value::Object(backwards))
        );
        // Control: a different value does hash differently.
        forwards.insert("a".into(), json!(2));
        assert_ne!(
            digest(Value::Object(forwards)),
            digest(json!({ "a": 1, "b": { "x": 1, "y": 2 } }))
        );
    }

    #[test]
    fn only_whole_datetime_literals_are_rewritten() {
        let mut rewrites = 0;
        let out = normalise_snapshot(
            json!({
                "a": "d'2026-10-02T16:17:12.643091493Z'",
                "b": "d'2026-10-02T16:17:12Z' trailing",
                "c": "plain",
                "d": 5,
            }),
            &mut rewrites,
        );
        assert_eq!(rewrites, 1);
        assert_eq!(
            out,
            json!({
                "a": "2026-10-02T16:17:12.643091Z",
                "b": "d'2026-10-02T16:17:12Z' trailing",
                "c": "plain",
                "d": 5,
            })
        );
    }
}
