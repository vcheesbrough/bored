//! Predecessor/successor links between cards (iteration 42 — card #76).
//!
//! A link is one stored fact — `predecessor` comes before `successor` — that
//! is created from either card and addressed by its own id afterwards. The
//! rules a link must satisfy (trimmed, capped reason; no loops) live in
//! `shared::links` so the browser can apply the same ones before it asks.
//!
//! # Cross-board links (iteration 78 — card #544)
//!
//! The two ends may sit on different boards. A card can never move to another
//! board, so the pair of boards a link joins is fixed for its whole life, and
//! three things follow from that:
//!
//! * a board's link list is every link with **either** end on it;
//! * the loop check follows links across **every** board, because a loop can
//!   pass through any number of them;
//! * every history row and SSE event about a link is written **once per board
//!   it touches** ([`DbCardLink::boards`]), so both boards' history panels,
//!   card histories and live views see it. Link history rows are never
//!   restorable, so the second copy can never be replayed twice.
//!
//! # One transaction per link write (iteration 81 — card #547)
//!
//! A cross-board link has two history rows per change, one per board. Each
//! write — create, reason change, delete — therefore runs the link row *and
//! all of its history rows* in one database transaction, and sends its SSE
//! events only after that transaction commits. Either both boards see the
//! change (row, history, live event) or neither does; a failure half-way
//! cannot leave one board's history claiming something the other board never
//! heard about, or a link with no history at all.
//!
//! # The loop check (iteration 81 — card #547)
//!
//! Up to iteration 80 the loop check read every link in the database into
//! memory and searched it there. It now asks Postgres instead, with a
//! recursive query that walks forward from the new link's successor
//! ([`successor_reaches_predecessor`]), so only the cards actually reachable
//! from that one card are visited, however many links the rest of the
//! database holds.
//!
//! Responses here carry a short plain-text body on the client-error statuses,
//! unlike the other routes which return a bare status. Three different things
//! are 422 for a link (self-link, over-long reason, cycle) and the editor
//! needs to tell the user which one it hit.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};

use crate::audit;
use crate::auth::Claims;
use crate::db::{Db, DbOperation, DbQuery, Traced as _};
use crate::error::ApiError;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::{DbCard, DbCardLink, DbColumn};
use crate::routes::boards::{AppState, editor_sub, find_board_by_slug};

// The three distinct 422s and the 409 this module can return. Each carries its
// message because, unlike the other route modules, the link editor has to tell
// the user *which* rule they broke. `ApiError::Unprocessable(Some(..))` renders
// the same plain-text response the `(StatusCode, &str)` tuples used to.
const SELF_LINK: ApiError = ApiError::Unprocessable(Some("a card cannot be linked to itself"));
const REASON_TOO_LONG: ApiError =
    ApiError::Unprocessable(Some("reason is longer than 200 characters"));
const CYCLE: ApiError = ApiError::Unprocessable(Some("linking these cards would create a loop"));
/// The message is shared with `error::classify`, which produces this same
/// response when the unique index — rather than the check below — catches the
/// duplicate.
const ALREADY_LINKED: ApiError = ApiError::Conflict(Some(crate::error::ALREADY_LINKED_MESSAGE));

/// The select every link read starts from, followed by `$rest` (a string
/// literal: the WHERE / ORDER BY). The card numbers and each end's board (id
/// and slug) are not stored on the link row; they are joined in from the two
/// cards, their columns and their boards, so the API type can carry them
/// without a second query per link.
///
/// Inner joins are safe all the way up: a card's deletion cascades to its
/// links, and `cards.column_id` / `columns.board_id` are `ON DELETE RESTRICT`,
/// so every link has two cards, each card has a column, and each column has
/// a board.
///
/// A macro rather than a `const` + `format!` so the whole statement is still
/// one string literal (`concat!` runs at compile time), which is what sqlx
/// accepts as SQL without an injection audit.
macro_rules! link_select {
    ($rest:literal) => {
        concat!(
            "SELECT l.*, p.number AS predecessor_number, s.number AS successor_number, \
             pc.board_id AS predecessor_board_id, sc.board_id AS successor_board_id, \
             pb.name AS predecessor_board_slug, sb.name AS successor_board_slug \
             FROM card_links l \
             JOIN cards p ON p.id = l.predecessor_id \
             JOIN cards s ON s.id = l.successor_id \
             JOIN columns pc ON pc.id = p.column_id \
             JOIN columns sc ON sc.id = s.column_id \
             JOIN boards pb ON pb.id = pc.board_id \
             JOIN boards sb ON sb.id = sc.board_id ",
            $rest
        )
    };
}

/// One link by id, with the card numbers joined in.
///
/// Generic over the executor so it can read either through the pool (`&Db`)
/// or inside an open transaction (`&mut *tx`) — a write reads its own,
/// not-yet-committed row back that way. `E: sqlx::PgExecutor<'e>` means
/// "anything that can run one Postgres statement, borrowed for `'e`".
async fn load_link<'e, E>(db: E, link_id: &str) -> Result<Option<DbCardLink>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as(link_select!("WHERE l.id = $1"))
        .bind(link_id)
        .fetch_optional(db)
        .traced(DbQuery::new(
            "links.load_link",
            DbOperation::Select,
            "card_links",
        ))
        .await
}

/// Every link with at least one end on the board, oldest first. A
/// cross-board link is listed on both of its boards.
async fn board_links(db: &Db, board_ulid: &str) -> Result<Vec<DbCardLink>, sqlx::Error> {
    sqlx::query_as(link_select!(
        "WHERE pc.board_id = $1 OR sc.board_id = $1 ORDER BY l.created_at, l.id"
    ))
    .bind(board_ulid)
    .fetch_all(db)
    .traced(DbQuery::new(
        "links.board_links",
        DbOperation::Select,
        "card_links",
    ))
    .await
}

/// Whether the exact link `predecessor → successor` is already stored. One
/// probe of the `card_links_pair` unique index.
async fn link_exists(
    db: &Db,
    predecessor_id: &str,
    successor_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM card_links WHERE predecessor_id = $1 AND successor_id = $2)",
    )
    .bind(predecessor_id)
    .bind(successor_id)
    .fetch_one(db)
    .traced(DbQuery::new(
        "links.link_exists",
        DbOperation::Select,
        "card_links",
    ))
    .await
}

/// Whether adding `predecessor → successor` would close a loop: true exactly
/// when `successor` can already reach `predecessor` by following existing
/// links forward (or the two are the same card).
///
/// This is the same rule as `shared::links::would_create_cycle`, which the
/// browser runs over the links it has loaded; here Postgres walks the graph
/// itself, so the server never has to load every link (card #547).
///
/// How the recursive query reads:
///
/// * `reachable` starts with one row, the successor;
/// * each round adds the successor of every link whose predecessor is
///   already in `reachable` — one step further forward. The
///   `card_links_pair` unique index leads on `predecessor_id`, so each step
///   is an index lookup, not a table scan;
/// * `UNION` (not `UNION ALL`) drops cards already found, so a round that
///   finds nothing new adds no rows and the recursion stops. That also makes
///   it finish on a graph that somehow already holds a loop.
///
/// Only cards reachable from `successor` are ever visited; links elsewhere in
/// the database cost nothing. `pub(crate)` so the tests can check it against
/// hand-built graphs.
pub(crate) async fn successor_reaches_predecessor(
    db: &Db,
    predecessor_id: &str,
    successor_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "WITH RECURSIVE reachable (card_id) AS ( \
             SELECT $2::text \
             UNION \
             SELECT l.successor_id FROM card_links l \
             JOIN reachable r ON l.predecessor_id = r.card_id \
         ) \
         SELECT EXISTS (SELECT 1 FROM reachable WHERE card_id = $1)",
    )
    .bind(predecessor_id)
    .bind(successor_id)
    .fetch_one(db)
    .traced(DbQuery::new(
        "links.successor_reaches_predecessor",
        DbOperation::Select,
        "card_links",
    ))
    .await
}

/// Every link with `card_id` at either end. Generic over the executor (see
/// [`load_link`]) so the card delete can read them on its own transaction.
async fn links_touching_card<'e, E>(db: E, card_id: &str) -> Result<Vec<DbCardLink>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as(link_select!(
        "WHERE l.predecessor_id = $1 OR l.successor_id = $1 \
         ORDER BY l.created_at, l.id"
    ))
    .bind(card_id)
    .fetch_all(db)
    .traced(DbQuery::new(
        "links.links_touching_card",
        DbOperation::Select,
        "card_links",
    ))
    .await
}

/// One card by id, or `None`; `name` is the call site, for the span.
async fn select_card(
    db: &Db,
    card_id: &str,
    name: &'static str,
) -> Result<Option<DbCard>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM cards WHERE id = $1")
        .bind(card_id)
        .fetch_optional(db)
        .traced(DbQuery::new(name, DbOperation::Select, "cards"))
        .await
}

/// The JSON a link's history row stores: the API shape **minus the two board
/// slugs**.
///
/// The slugs are left out on purpose. A history row is permanent and lives on
/// a board's history, so storing the far board's name there would copy it
/// into the other board's record for good — which becomes a leak to redact
/// once boards are private (card #12). The summaries only render card numbers
/// (`#P → #S`), and `shared::CardLink`'s `#[serde(default)]` slugs let these
/// snapshots still parse.
fn link_snapshot(link: &shared::CardLink) -> Result<serde_json::Value, ApiError> {
    let mut value = serde_json::to_value(link)?;
    // `as_object_mut` is `Some` for any struct serialized by serde_json; the
    // `if let` just avoids an unwrap.
    if let Some(object) = value.as_object_mut() {
        object.remove("predecessor_board_slug");
        object.remove("successor_board_slug");
    }
    Ok(value)
}

/// Write one history row per board the link touches, inside the caller's
/// transaction, and return them for [`announce_link_change`] to broadcast
/// once that transaction has committed. `before` / `after` are the snapshots
/// (already slug-free, from [`link_snapshot`]).
///
/// `tx` is the open transaction's connection (`&mut *tx` at the call site):
/// `&mut sqlx::PgConnection` is what a `sqlx::Transaction` dereferences to.
/// Each insert reborrows it (`&mut *tx`) so the loop can use it again.
#[allow(clippy::too_many_arguments)]
async fn record_link_audit(
    tx: &mut sqlx::PgConnection,
    claims: &Claims,
    boards: &[String],
    link_id: &str,
    action: &'static str,
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
    batch_group: Option<&str>,
) -> Result<Vec<shared::AuditLogEntry>, ApiError> {
    let mut entries = Vec::with_capacity(boards.len());
    for board_id in boards {
        let entry = audit::insert(
            &mut *tx,
            audit::AuditRecord {
                claims,
                board_id: board_id.clone(),
                entity_type: "card_link",
                entity_id: link_id,
                action,
                snapshot_before: before.clone(),
                snapshot_after: after.clone(),
                restored_from: None,
                batch_group: batch_group.map(str::to_string),
                audit_edit_session: None,
            },
        )
        .await?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Announce a committed link change: first each history row
/// (`AuditAppended`, one per board), then `event` once per board.
///
/// Called only **after** the transaction holding the change and its history
/// rows has committed, so a live view never hears of a change a rollback
/// took back. Sending on the in-process channel cannot fail in a way that
/// matters (see below), so once the commit has succeeded both boards are
/// told.
///
/// `BroadcastEvent` carries one board id and each SSE subscriber filters on
/// its own board, so a cross-board link's event has to go out once per board.
fn announce_link_change(
    state: &AppState,
    entries: Vec<shared::AuditLogEntry>,
    boards: &[String],
    event: &BoardEvent,
) {
    for entry in entries {
        audit::broadcast_appended(&state.events, entry);
    }
    for board_id in boards {
        // `send` only fails when nobody is subscribed, which is fine.
        let _ = state.events.send(BroadcastEvent {
            board_id: board_id.clone(),
            event: event.clone(),
        });
    }
}

/// The board a card sits on, via its column. `None` when the column is gone,
/// which the `ON DELETE RESTRICT` foreign key should make impossible; the
/// caller treats it as a missing card.
async fn board_of_card(db: &Db, card: &DbCard) -> Result<Option<String>, sqlx::Error> {
    let column: Option<DbColumn> = sqlx::query_as("SELECT * FROM columns WHERE id = $1")
        .bind(&card.column_id)
        .fetch_optional(db)
        .traced(DbQuery::new(
            "links.board_of_card",
            DbOperation::Select,
            "columns",
        ))
        .await?;
    Ok(column.map(|c| c.board_id))
}

/// Map a reason the shared rules rejected onto the 422 the client sees.
fn normalize_reason(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    shared::links::normalize_reason(raw).map_err(|_| REASON_TOO_LONG)
}

/// `GET /api/boards/:slug/links` — every link on the board.
#[tracing::instrument(skip_all, fields(bored.board.slug = %slug))]
pub async fn list_board_links(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Vec<shared::CardLink>>, ApiError> {
    let board = match find_board_by_slug(&state.db, &slug).await? {
        Some(b) => b,
        None => return Err(ApiError::NotFound),
    };
    let links = board_links(&state.db, &board.id).await?;
    Ok(Json(links.into_iter().map(DbCardLink::into_api).collect()))
}

/// `POST /api/cards/:id/links` — link this card to another one.
///
/// `direction` says which role the *other* card takes, so the same row comes
/// out whether the user started from the predecessor or the successor.
///
/// The other card may be on any board (card #544).
///
/// 404 unknown card · 409 already linked · 422 self-link, reason too long, or
/// cycle (checked across every board).
#[tracing::instrument(skip_all, fields(bored.card.id = %card_id))]
pub async fn create_card_link(
    State(state): State<AppState>,
    Path(card_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::CreateCardLinkRequest>,
) -> Result<(StatusCode, Json<shared::CardLink>), ApiError> {
    let card = select_card(&state.db, &card_id, "links.create_card_link")
        .await?
        .ok_or(ApiError::NotFound)?;

    // Checked before the other card is even loaded: a self-link is not a
    // missing card, and 404 would send the client looking for one.
    if payload.other_card_id == card_id {
        return Err(SELF_LINK);
    }

    let other = select_card(&state.db, &payload.other_card_id, "links.create_card_link")
        .await?
        .ok_or(ApiError::NotFound)?;

    // Both ends must resolve to a board. They no longer have to be the same
    // board; the boards the link touches are read back from the created row
    // below, so nothing here needs to keep them.
    for end in [&card, &other] {
        board_of_card(&state.db, end)
            .await?
            .ok_or(ApiError::NotFound)?;
    }

    // Validate the reason before touching the graph so a bad reason costs
    // nothing more than the lookups above.
    let reason = normalize_reason(payload.reason.as_deref())?;

    let (predecessor_id, successor_id) = match payload.direction {
        shared::LinkDirection::Predecessor => (payload.other_card_id.clone(), card_id.clone()),
        shared::LinkDirection::Successor => (card_id.clone(), payload.other_card_id.clone()),
    };

    // From the checks to the COMMIT the link graph must not change under us —
    // see `AppState::link_lock`, which is one process-wide lock, so it
    // already covers every board. The guard drops at the end of the block,
    // which is *after* the commit: under Postgres's default READ COMMITTED
    // isolation another create's loop check cannot see this link until it
    // has committed, so releasing the lock any earlier would let two creates
    // each pass the check against the old graph and together close a loop.
    let (link, boards, entries) = {
        let _guard = state.link_lock.lock().await;

        // An exact duplicate is a conflict, not a cycle — checked first so the
        // client gets the more specific answer.
        if link_exists(&state.db, &predecessor_id, &successor_id).await? {
            return Err(ALREADY_LINKED);
        }
        // Walks the graph across every board: a loop may run through any of
        // them. `predecessor_id != successor_id` was checked above.
        if successor_reaches_predecessor(&state.db, &predecessor_id, &successor_id).await? {
            return Err(CYCLE);
        }

        // The card lookups above ran before this lock was taken, so a
        // concurrent delete could have removed either card in the meantime.
        // Re-check right before the write so a vanished card is the 404 it
        // is. The re-check narrows the window but cannot close it — a card
        // can still go between it and the INSERT — so the INSERT's own
        // foreign-key violation maps to the same 404 below. A card that is
        // *being* deleted is locked (`FOR UPDATE`) by its delete for the
        // whole of that delete; the INSERT's foreign-key check waits for that
        // lock and then finds the card gone — also the 404 — so a link can
        // never slip in behind a delete's link sweep (card #547).
        let predecessor_still_exists =
            select_card(&state.db, &predecessor_id, "links.create_card_link").await?;
        let successor_still_exists =
            select_card(&state.db, &successor_id, "links.create_card_link").await?;
        if predecessor_still_exists.is_none() || successor_still_exists.is_none() {
            return Err(ApiError::NotFound);
        }

        // The link row and its history rows commit together (card #547).
        // An early `return` / `?` below drops `tx` without committing, and
        // sqlx rolls a dropped transaction back.
        let mut tx = state.db.begin().await?;

        let id = ulid::Ulid::new().to_string().to_lowercase();
        sqlx::query(
            "INSERT INTO card_links (id, predecessor_id, successor_id, reason, last_edited_by) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&id)
        .bind(&predecessor_id)
        .bind(&successor_id)
        .bind(&reason)
        .bind(editor_sub(&claims))
        .execute(&mut *tx)
        .traced(DbQuery::new(
            "links.create_card_link",
            DbOperation::Insert,
            "card_links",
        ))
        // Under the lock the duplicate check above should make the unique
        // constraint unreachable; if it fires anyway, the right answer is
        // still "already linked", not a server fault — which is what
        // `error::classify` returns for a `card_links_pair` violation. A
        // foreign-key violation is a card deleted since the re-check: 404.
        .await
        .map_err(|error| match &error {
            sqlx::Error::Database(database) if database.code().as_deref() == Some("23503") => {
                ApiError::NotFound
            }
            _ => ApiError::from(error),
        })?;

        // Re-read rather than use the INSERT's return: the card numbers and
        // the boards are only joined in by `link_select!`. Read inside the
        // transaction — the row is not visible outside it until the commit.
        let row = load_link(&mut *tx, &id)
            .await?
            .ok_or_else(|| ApiError::internal("link vanished after create"))?;
        let boards = row.boards();
        let link = row.into_api();

        let entries = record_link_audit(
            &mut tx,
            &claims,
            &boards,
            &link.id,
            "create",
            None,
            Some(link_snapshot(&link)?),
            None,
        )
        .await?;

        tx.commit().await?;
        (link, boards, entries)
    };

    announce_link_change(
        &state,
        entries,
        &boards,
        &BoardEvent::CardLinkCreated { link: link.clone() },
    );
    Ok((StatusCode::CREATED, Json(link)))
}

/// `PUT /api/links/:id` — change a link's reason.
///
/// Sending the reason the link already has is a no-op: the link comes back
/// unchanged and no history row is written.
#[tracing::instrument(skip_all, fields(bored.link.id = %link_id))]
pub async fn update_card_link(
    State(state): State<AppState>,
    Path(link_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::UpdateCardLinkRequest>,
) -> Result<Json<shared::CardLink>, ApiError> {
    let existing = load_link(&state.db, &link_id)
        .await?
        .ok_or(ApiError::NotFound)?;

    let reason = normalize_reason(payload.reason.as_deref())?;
    if reason == existing.reason {
        return Ok(Json(existing.into_api()));
    }

    let boards = existing.boards();
    let snapshot_before = link_snapshot(&existing.into_api())?;

    // The new reason and its history rows commit together (card #547); see
    // the module docs. Dropping `tx` on an early return rolls it back.
    let mut tx = state.db.begin().await?;

    sqlx::query("UPDATE card_links SET reason = $2, last_edited_by = $3 WHERE id = $1")
        .bind(&link_id)
        .bind(&reason)
        .bind(editor_sub(&claims))
        .execute(&mut *tx)
        .traced(DbQuery::new(
            "links.update_card_link",
            DbOperation::Update,
            "card_links",
        ))
        .await?;

    // A link deleted since `load_link` above is not here to read back: the
    // UPDATE matched nothing, and the `?` returns 404 with `tx` dropped —
    // rolled back, so no history row is written.
    let link = load_link(&mut *tx, &link_id)
        .await?
        .ok_or(ApiError::NotFound)?
        .into_api();

    let entries = record_link_audit(
        &mut tx,
        &claims,
        &boards,
        &link.id,
        "update",
        Some(snapshot_before),
        Some(link_snapshot(&link)?),
        None,
    )
    .await?;

    tx.commit().await?;

    announce_link_change(
        &state,
        entries,
        &boards,
        &BoardEvent::CardLinkUpdated { link: link.clone() },
    );
    Ok(Json(link))
}

/// `DELETE /api/links/:id` — remove a link.
#[tracing::instrument(skip_all, fields(bored.link.id = %link_id))]
pub async fn delete_card_link(
    State(state): State<AppState>,
    Path(link_id): Path<String>,
    claims: Extension<Claims>,
) -> Result<StatusCode, ApiError> {
    let existing = load_link(&state.db, &link_id)
        .await?
        .ok_or(ApiError::NotFound)?;

    // `false`: a concurrent delete removed it (and wrote its history) after
    // `load_link` found it — for this request it is simply not there.
    if !delete_one_link(&state, &claims, existing, None).await? {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// One link removal that has been written (row deleted, history rows
/// inserted) on some transaction but not yet announced. The caller sends it
/// with [`announce_link_removal`] once that transaction has committed.
pub(crate) struct LinkRemoval {
    link_id: String,
    boards: Vec<String>,
    entries: Vec<shared::AuditLogEntry>,
}

/// Delete one link and write its history rows (one per board) on `tx`,
/// **without** committing or announcing anything. The building block both
/// [`delete_one_link`] (its own transaction) and [`sweep_card_links`] (the
/// card delete's transaction) use, so every removal — however it came
/// about — leaves the same history rows and the same SSE events, on every
/// board the link touches. Deleting board B therefore tells board A that its
/// cross-board links to B are gone.
///
/// Returns `None`, writing nothing, when the link was already gone: a
/// concurrent delete got there first and wrote the history itself, so a
/// second set of rows would record one removal twice.
async fn remove_link(
    tx: &mut sqlx::PgConnection,
    claims: &Claims,
    link: DbCardLink,
    batch_group: Option<&str>,
) -> Result<Option<LinkRemoval>, ApiError> {
    let link_id = link.id.clone();
    let boards = link.boards();
    let snapshot_before = link_snapshot(&link.into_api())?;

    // Delete first: the row lock this takes makes a concurrent delete of the
    // same link wait for our commit and then find nothing (0 rows), so
    // exactly one of them records the removal.
    let deleted = sqlx::query("DELETE FROM card_links WHERE id = $1")
        .bind(&link_id)
        .execute(&mut *tx)
        .traced(DbQuery::new(
            "links.delete_one_link",
            DbOperation::Delete,
            "card_links",
        ))
        .await?;
    if deleted.rows_affected() == 0 {
        return Ok(None);
    }

    let entries = record_link_audit(
        tx,
        claims,
        &boards,
        &link_id,
        "delete",
        Some(snapshot_before),
        None,
        batch_group,
    )
    .await?;
    Ok(Some(LinkRemoval {
        link_id,
        boards,
        entries,
    }))
}

/// Announce a committed [`LinkRemoval`]: its history rows, then
/// `CardLinkDeleted`, on every board the link touched.
pub(crate) fn announce_link_removal(state: &AppState, removal: LinkRemoval) {
    let LinkRemoval {
        link_id,
        boards,
        entries,
    } = removal;
    announce_link_change(
        state,
        entries,
        &boards,
        &BoardEvent::CardLinkDeleted { link_id },
    );
}

/// Delete, record, and broadcast the removal of one link in a transaction of
/// its own: the delete and its history rows commit together (card #547), and
/// the events go out after the commit. Returns `false`, writing nothing, when
/// the link was already gone (see [`remove_link`]).
async fn delete_one_link(
    state: &AppState,
    claims: &Claims,
    link: DbCardLink,
    batch_group: Option<&str>,
) -> Result<bool, ApiError> {
    let mut tx = state.db.begin().await?;
    // `&mut tx` derefs to the transaction's `&mut PgConnection`. On `None`
    // or an error, dropping `tx` just ends (rolls back) the transaction.
    let Some(removal) = remove_link(&mut tx, claims, link, batch_group).await? else {
        return Ok(false);
    };
    tx.commit().await?;
    announce_link_removal(state, removal);
    Ok(true)
}

/// The card-delete route's link sweep: remove every link touching `card_id`
/// **on the delete's own transaction** `tx`, and return the removals for the
/// caller to announce after it commits.
///
/// Running on the caller's transaction means the whole card delete uses one
/// pooled connection from start to finish. Holding the card's row lock on one
/// connection while taking more from the pool would let enough concurrent
/// deletes hold every connection and wait on each other for a free one
/// (PR #101 review). It also makes the link removals commit — or roll back —
/// together with the card.
///
/// The caller must already hold the card's row lock on `tx` (see
/// [`cascade_delete_card_links`] for why).
pub(crate) async fn sweep_card_links(
    tx: &mut sqlx::PgConnection,
    claims: &Claims,
    card_id: &str,
) -> Result<Vec<LinkRemoval>, ApiError> {
    let links = links_touching_card(&mut *tx, card_id).await?;
    let mut removals = Vec::with_capacity(links.len());
    for link in links {
        // `None`: already gone (a concurrent explicit delete recorded it).
        if let Some(removal) = remove_link(&mut *tx, claims, link, None).await? {
            removals.push(removal);
        }
    }
    Ok(removals)
}

/// Remove every link touching `card_id`, recording each removal. Called by
/// the card, column, and board delete routes before the card itself goes,
/// because a link cannot outlive either of its ends.
///
/// `batch_group` ties the link rows to the cascade that caused them. They are
/// never replayed by a batch restore — a deleted link may no longer be legal
/// to re-add — but grouping them keeps the history honest about why they went.
/// For a cross-board link the row on the *other* board carries the same
/// group, so its history can say which cascade removed the link.
///
/// **The caller must hold the card's row lock** (`SELECT … FOR UPDATE` in a
/// transaction that lasts until the card's own `DELETE`). Creating a link
/// takes a share lock on both cards through its foreign-key check, so with
/// the card locked no new link to it can commit between this sweep and the
/// card's delete. Without the lock one could, and the card's `DELETE` would
/// then remove it through `ON DELETE CASCADE` with no history row and no
/// event (card #547).
pub(crate) async fn cascade_delete_card_links(
    state: &AppState,
    claims: &Claims,
    card_id: &str,
    batch_group: Option<&str>,
) -> Result<(), ApiError> {
    let links = links_touching_card(&state.db, card_id).await?;
    for link in links {
        // `false` (already gone — a concurrent explicit delete recorded it)
        // needs nothing more from the cascade, so the flag is not inspected.
        delete_one_link(state, claims, link, batch_group).await?;
    }
    Ok(())
}
