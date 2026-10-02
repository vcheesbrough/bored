//! The public API's wire contract (card #472): what every REST response and
//! every SSE event looks like, independent of the database behind it.
//!
//! # Why this suite exists
//!
//! Card #471 moves storage from SurrealDB to Postgres. Its acceptance test is
//! "every response byte-identical before and after", and this file is how that
//! is checked: it pins the *shape* of every payload, so a storage change that
//! alters what clients see fails here first. It is deliberately broad: one
//! scenario drives every JSON route, and every `BoardEvent` variant, through
//! the real router.
//!
//! # What it asserts
//!
//! For every response body and every serialized SSE event:
//!
//! - **No storage syntax.** No `d'` (a SurrealQL datetime literal) and no
//!   `"<table>:` (a SurrealDB record id) anywhere in the text.
//! - **Timestamps** match `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}Z$` —
//!   RFC 3339, UTC, exactly six fractional digits (`crate::timestamp`).
//! - **IDs** are bare lowercase ULIDs.
//! - **The field set** of each payload type is exactly the hand-written list
//!   below, so adding, removing or renaming a field fails loudly. Audit rows'
//!   snapshots are checked against the shape of the entity they record.
//! - **Every event variant is exercised.** [`event_type`] is an exhaustive
//!   `match`, so a new `BoardEvent` variant does not compile until it is named
//!   here, and the scenario then fails until it actually emits it.
//!
//! The MCP (`mcp/`) has no routes of its own: every tool is a call to one of
//! the REST routes below, passed through as JSON, so covering the routes
//! covers what agents see.
//!
//! # Independent oracle
//!
//! Field lists, the timestamp regex and the ULID alphabet are written out by
//! hand from the card, never derived from `shared`'s types or from
//! `crate::timestamp` — deriving them would make the test agree with whatever
//! the code does.
//!
//! # Not covered here
//!
//! Historic audit rows written before #472 still hold `d'…'` timestamps
//! *inside* their snapshots until #471's data migration rewrites them; these
//! tests run on a fresh database, so they see only new rows. Restoring such a
//! legacy row is covered by [`restore_legacy_deletion`].

use super::*;

use std::collections::BTreeSet;
use std::sync::LazyLock;

use axum_test::TestResponse;
use regex::Regex;
use serde_json::Value;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::TryRecvError;

use crate::events::{BoardEvent, BroadcastEvent};

// ── The contract, written out by hand ───────────────────────────────────────

/// The card's timestamp grammar, verbatim. `LazyLock` compiles the regex once,
/// on first use, and shares it between tests.
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}Z$").expect("timestamp regex")
});

/// A bored id: a ULID in lowercase Crockford base32 (26 characters, no
/// `i`, `l`, `o` or `u`). Never the `table:id` record-id form.
static ULID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-hjkmnp-tv-z]{26}$").expect("ulid regex"));

/// Every SurrealDB table. A JSON string starting `"<table>:` would be a
/// record id leaking out.
const TABLES: &[&str] = &[
    "boards",
    "columns",
    "cards",
    "card_counter",
    "audit_log",
    "card_links",
];

/// One kind of JSON object the API returns.
///
/// Each variant knows its exact field set, which of those fields are
/// timestamps, and which are ids. `Copy` so it can be passed around freely,
/// `Debug` so a failure message can name it.
#[derive(Clone, Copy, Debug)]
enum Shape {
    Board,
    Column,
    Card,
    CardLink,
    AuditEntry,
    CardPosition,
    UserInfo,
    AppInfo,
}

impl Shape {
    /// The complete set of keys, sorted (they are compared as a set, so order
    /// here is only for reading).
    fn fields(self) -> &'static [&'static str] {
        match self {
            Shape::Board => &["created_at", "id", "last_edited_by", "name", "updated_at"],
            Shape::Column => &[
                "board_id",
                "created_at",
                "id",
                "last_edited_by",
                "name",
                "position",
                "updated_at",
            ],
            Shape::Card => &[
                "body",
                "column_id",
                "created_at",
                "id",
                "last_edited_by",
                "number",
                "position",
                "tags",
                "updated_at",
            ],
            Shape::CardLink => &[
                "created_at",
                "id",
                "last_edited_by",
                "predecessor_id",
                "predecessor_number",
                "reason",
                "successor_id",
                "successor_number",
                "updated_at",
            ],
            Shape::AuditEntry => &[
                "action",
                "actor_display_name",
                "actor_sub",
                "audit_edit_session",
                "batch_group",
                "board_id",
                "created_at",
                "entity_id",
                "entity_type",
                "id",
                "restored_from",
                "snapshot_after",
                "snapshot_before",
            ],
            Shape::CardPosition => &["id", "position"],
            Shape::UserInfo => &["email", "name", "picture"],
            Shape::AppInfo => &["branch", "env", "telemetry", "version"],
        }
    }

    /// Fields that must match [`TIMESTAMP`].
    fn timestamps(self) -> &'static [&'static str] {
        match self {
            Shape::Board | Shape::Column | Shape::Card | Shape::CardLink => {
                &["created_at", "updated_at"]
            }
            Shape::AuditEntry => &["created_at"],
            Shape::CardPosition | Shape::UserInfo | Shape::AppInfo => &[],
        }
    }

    /// Fields that must be a bare ULID when present (`null` is allowed, for
    /// the optional ones such as `restored_from`).
    fn ids(self) -> &'static [&'static str] {
        match self {
            Shape::Board => &["id"],
            Shape::Column => &["id", "board_id"],
            Shape::Card => &["id", "column_id"],
            Shape::CardLink => &["id", "predecessor_id", "successor_id"],
            Shape::AuditEntry => &[
                "id",
                "entity_id",
                "board_id",
                "restored_from",
                "batch_group",
            ],
            Shape::CardPosition => &["id"],
            Shape::UserInfo | Shape::AppInfo => &[],
        }
    }
}

// ── Checks ──────────────────────────────────────────────────────────────────

/// The text of a body or event must carry no SurrealDB syntax at all. Checked
/// on the raw text, not the parsed JSON, so nothing can hide in a field the
/// shape checks do not look at.
fn assert_no_storage_syntax(text: &str, context: &str) {
    assert!(
        !text.contains("d'"),
        "{context}: SurrealQL datetime literal in {text}"
    );
    for table in TABLES {
        assert!(
            !text.contains(&format!("\"{table}:")),
            "{context}: `{table}:` record id in {text}"
        );
    }
}

/// Check one object against its [`Shape`]: exact field set, timestamp format,
/// id format — and, for audit rows, the snapshots inside them.
fn assert_shape(value: &Value, shape: Shape, context: &str) {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{context}: expected a {shape:?} object, got {value}"));

    // `BTreeSet` compares as a set and prints sorted, so a mismatch shows
    // exactly which keys differ.
    let actual: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = shape.fields().iter().copied().collect();
    assert_eq!(actual, expected, "{context}: field set of {shape:?}");

    for field in shape.timestamps() {
        let raw = object[*field]
            .as_str()
            .unwrap_or_else(|| panic!("{context}: {shape:?}.{field} is not a string"));
        assert!(
            TIMESTAMP.is_match(raw),
            "{context}: {shape:?}.{field} = {raw:?} is not the API timestamp format"
        );
    }

    for field in shape.ids() {
        match &object[*field] {
            Value::Null => {}
            Value::String(raw) => assert!(
                ULID.is_match(raw),
                "{context}: {shape:?}.{field} = {raw:?} is not a bare ULID"
            ),
            other => panic!("{context}: {shape:?}.{field} is {other}, not an id"),
        }
    }

    if let Shape::AuditEntry = shape {
        // A snapshot is the `into_api()` form of the entity the row records,
        // so it is held to that entity's contract too.
        let snapshot_shape = match object["entity_type"].as_str() {
            Some("board") => Shape::Board,
            Some("column") => Shape::Column,
            Some("card") => Shape::Card,
            Some("card_link") => Shape::CardLink,
            other => panic!("{context}: unknown audit entity_type {other:?}"),
        };
        for field in ["snapshot_before", "snapshot_after"] {
            if !object[field].is_null() {
                assert_shape(
                    &object[field],
                    snapshot_shape,
                    &format!("{context} → {field}"),
                );
            }
        }
    }
}

/// Every element of a JSON array has `shape`. An empty list proves nothing, so
/// it is refused: each list route is called when it has something to list.
fn assert_list_of(value: &Value, shape: Shape, context: &str) {
    let items = value
        .as_array()
        .unwrap_or_else(|| panic!("{context}: expected a list, got {value}"));
    assert!(!items.is_empty(), "{context}: empty list proves nothing");
    for (index, item) in items.iter().enumerate() {
        assert_shape(item, shape, &format!("{context}[{index}]"));
    }
}

/// Check a response's status and storage hygiene, and return its body parsed.
fn body(response: TestResponse, status: StatusCode, context: &str) -> Value {
    assert_eq!(response.status_code(), status, "{context}: status");
    let text = response.text();
    assert_no_storage_syntax(&text, context);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{context}: body is not JSON ({e})"))
}

/// A `204 No Content`: right status, and no body to leak anything through.
fn assert_no_content(response: TestResponse, context: &str) {
    assert_eq!(response.status_code(), StatusCode::NO_CONTENT, "{context}");
    assert!(response.text().is_empty(), "{context}: 204 with a body");
}

/// An error response: right status, and no database text in whatever body it
/// carries (card #366's redaction, asserted here as part of the contract).
fn assert_error(response: TestResponse, status: StatusCode, context: &str) {
    assert_eq!(response.status_code(), status, "{context}: status");
    assert_no_storage_syntax(&response.text(), context);
}

// ── Events ──────────────────────────────────────────────────────────────────

/// Every SSE `type` value, by hand. [`event_type`] must agree with serde's
/// `rename_all = "snake_case"` for each, and the scenario must emit them all.
const ALL_EVENT_TYPES: &[&str] = &[
    "card_created",
    "card_updated",
    "card_deleted",
    "card_moved",
    "cards_renumbered",
    "card_link_created",
    "card_link_updated",
    "card_link_deleted",
    "column_created",
    "column_updated",
    "column_deleted",
    "columns_reordered",
    "board_created",
    "board_updated",
    "board_deleted",
    "audit_appended",
];

/// The wire `type` of an event, by exhaustive `match`.
///
/// There is deliberately no `_ =>` arm: adding a `BoardEvent` variant makes
/// this function fail to compile until the new variant is named here (and in
/// [`ALL_EVENT_TYPES`] and [`assert_event`]).
fn event_type(event: &BoardEvent) -> &'static str {
    match event {
        BoardEvent::CardCreated { .. } => "card_created",
        BoardEvent::CardUpdated { .. } => "card_updated",
        BoardEvent::CardDeleted { .. } => "card_deleted",
        BoardEvent::CardMoved { .. } => "card_moved",
        BoardEvent::CardsRenumbered { .. } => "cards_renumbered",
        BoardEvent::CardLinkCreated { .. } => "card_link_created",
        BoardEvent::CardLinkUpdated { .. } => "card_link_updated",
        BoardEvent::CardLinkDeleted { .. } => "card_link_deleted",
        BoardEvent::ColumnCreated { .. } => "column_created",
        BoardEvent::ColumnUpdated { .. } => "column_updated",
        BoardEvent::ColumnDeleted { .. } => "column_deleted",
        BoardEvent::ColumnsReordered { .. } => "columns_reordered",
        BoardEvent::BoardCreated { .. } => "board_created",
        BoardEvent::BoardUpdated { .. } => "board_updated",
        BoardEvent::BoardDeleted { .. } => "board_deleted",
        BoardEvent::AuditAppended { .. } => "audit_appended",
    }
}

/// Check one event exactly as the SSE handler sends it:
/// `serde_json::to_string(&event)` (see `events::sse_handler`).
fn assert_event(event: &BoardEvent) -> &'static str {
    let name = event_type(event);
    let context = format!("SSE {name}");
    let text = serde_json::to_string(event).expect("events serialize");
    assert_no_storage_syntax(&text, &context);
    let value: Value = serde_json::from_str(&text).expect("event is JSON");

    assert_eq!(value["type"], name, "{context}: `type` discriminator");

    // The envelope's keys besides `type`, and what each one holds.
    enum Payload {
        Object(Shape),
        List(Shape),
        Id,
    }
    let payload: &[(&str, Payload)] = match name {
        "card_created" | "card_updated" => &[("card", Payload::Object(Shape::Card))],
        "card_moved" => &[
            ("card", Payload::Object(Shape::Card)),
            ("from_column_id", Payload::Id),
        ],
        "card_deleted" => &[("card_id", Payload::Id)],
        "cards_renumbered" => &[
            ("column_id", Payload::Id),
            ("positions", Payload::List(Shape::CardPosition)),
        ],
        "card_link_created" | "card_link_updated" => &[("link", Payload::Object(Shape::CardLink))],
        "card_link_deleted" => &[("link_id", Payload::Id)],
        "column_created" | "column_updated" => &[("column", Payload::Object(Shape::Column))],
        "column_deleted" => &[("column_id", Payload::Id)],
        "columns_reordered" => &[("columns", Payload::List(Shape::Column))],
        "board_created" | "board_updated" => &[("board", Payload::Object(Shape::Board))],
        "board_deleted" => &[("board_id", Payload::Id)],
        "audit_appended" => &[("entry", Payload::Object(Shape::AuditEntry))],
        other => unreachable!("event_type returned unknown {other}"),
    };

    let actual: BTreeSet<&str> = value
        .as_object()
        .expect("event is an object")
        .keys()
        .map(String::as_str)
        .collect();
    let expected: BTreeSet<&str> = std::iter::once("type")
        .chain(payload.iter().map(|(key, _)| *key))
        .collect();
    assert_eq!(actual, expected, "{context}: envelope fields");

    for (key, kind) in payload {
        let field_context = format!("{context}.{key}");
        match kind {
            Payload::Object(shape) => assert_shape(&value[*key], *shape, &field_context),
            Payload::List(shape) => assert_list_of(&value[*key], *shape, &field_context),
            Payload::Id => {
                let raw = value[*key].as_str().expect("id is a string");
                assert!(
                    ULID.is_match(raw),
                    "{field_context} = {raw:?} is not a bare ULID"
                );
            }
        }
    }
    name
}

/// Check everything queued on `rx`, recording which event types were seen.
///
/// Called after every request: each request's events are sent before its
/// response returns, and draining often keeps the receiver well inside the
/// channel's capacity. A `Lagged` receiver lost events unseen, so it fails.
fn drain_events(rx: &mut Receiver<BroadcastEvent>, seen: &mut BTreeSet<&'static str>) {
    loop {
        match rx.try_recv() {
            Ok(message) => {
                seen.insert(assert_event(&message.event));
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Lagged(n)) => panic!("receiver lagged; {n} events unchecked"),
            Err(TryRecvError::Closed) => panic!("broadcast channel closed"),
        }
    }
}

/// A server on a fresh in-memory database, plus a receiver subscribed before
/// any request and a handle on the database for tests that plant rows.
async fn contract_server() -> (
    TestServer,
    Receiver<BroadcastEvent>,
    surrealdb::Surreal<surrealdb::engine::local::Db>,
) {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db.clone());
    let rx = state.events.subscribe();
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    (server, rx, db)
}

/// Pull a string field out of a checked body.
fn text_of(value: &Value, key: &str) -> String {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} missing from {value}"))
        .to_string()
}

// ── The scenario ────────────────────────────────────────────────────────────

/// Every JSON route and every SSE event type, in one realistic session.
///
/// One test rather than one per route because the routes build on each other
/// (a link needs two cards, a restore needs a deletion), and because the
/// "every event type was seen" assertion only means something over the
/// whole run.
#[tokio::test]
async fn every_route_and_event_honours_the_contract() {
    let (server, mut rx, _db) = contract_server().await;
    let mut seen = BTreeSet::new();

    // ── Boards ──────────────────────────────────────────────────────────
    let board = body(
        server
            .post("/api/boards")
            .json(&shared::CreateBoardRequest {
                name: "contract-board".to_string(),
            })
            .await,
        StatusCode::CREATED,
        "POST /api/boards",
    );
    assert_shape(&board, Shape::Board, "POST /api/boards");
    drain_events(&mut rx, &mut seen);

    let list = body(
        server.get("/api/boards").await,
        StatusCode::OK,
        "GET /api/boards",
    );
    assert_list_of(&list, Shape::Board, "GET /api/boards");

    let fetched = body(
        server.get("/api/boards/contract-board").await,
        StatusCode::OK,
        "GET /api/boards/:slug",
    );
    assert_shape(&fetched, Shape::Board, "GET /api/boards/:slug");

    let renamed = body(
        server
            .put("/api/boards/contract-board")
            .json(&shared::UpdateBoardRequest {
                name: "contract".to_string(),
            })
            .await,
        StatusCode::OK,
        "PUT /api/boards/:slug",
    );
    assert_shape(&renamed, Shape::Board, "PUT /api/boards/:slug");
    drain_events(&mut rx, &mut seen);
    let slug = text_of(&renamed, "name");

    // ── Columns ─────────────────────────────────────────────────────────
    let mut column_ids = Vec::new();
    for (position, name) in ["Todo", "Doing", "Done"].into_iter().enumerate() {
        let column = body(
            server
                .post(&format!("/api/boards/{slug}/columns"))
                .json(&shared::CreateColumnRequest {
                    name: name.to_string(),
                    position: position as i32,
                })
                .await,
            StatusCode::CREATED,
            "POST /api/boards/:slug/columns",
        );
        assert_shape(&column, Shape::Column, "POST /api/boards/:slug/columns");
        column_ids.push(text_of(&column, "id"));
        drain_events(&mut rx, &mut seen);
    }
    let (todo, doing, done) = (&column_ids[0], &column_ids[1], &column_ids[2]);

    let columns = body(
        server.get(&format!("/api/boards/{slug}/columns")).await,
        StatusCode::OK,
        "GET /api/boards/:slug/columns",
    );
    assert_list_of(&columns, Shape::Column, "GET /api/boards/:slug/columns");

    let column = body(
        server
            .put(&format!("/api/columns/{doing}"))
            .json(&shared::UpdateColumnRequest {
                name: Some("In progress".to_string()),
                position: None,
            })
            .await,
        StatusCode::OK,
        "PUT /api/columns/:id",
    );
    assert_shape(&column, Shape::Column, "PUT /api/columns/:id");
    drain_events(&mut rx, &mut seen);

    let reordered = body(
        server
            .put(&format!("/api/boards/{slug}/columns/reorder"))
            .json(&shared::ColumnsReorderRequest {
                order: vec![doing.clone(), todo.clone(), done.clone()],
            })
            .await,
        StatusCode::OK,
        "PUT /api/boards/:slug/columns/reorder",
    );
    assert_list_of(
        &reordered,
        Shape::Column,
        "PUT /api/boards/:slug/columns/reorder",
    );
    drain_events(&mut rx, &mut seen);

    // ── Cards (with tags) ───────────────────────────────────────────────
    let mut cards = Vec::new();
    for i in 0..3 {
        let card = body(
            server
                .post(&format!("/api/columns/{todo}/cards"))
                .json(&shared::CreateCardRequest {
                    body: format!("# Contract card {i}"),
                    tags: vec!["contract".to_string(), format!("tag-{i}")],
                })
                .await,
            StatusCode::CREATED,
            "POST /api/columns/:id/cards",
        );
        assert_shape(&card, Shape::Card, "POST /api/columns/:id/cards");
        cards.push(card);
        drain_events(&mut rx, &mut seen);
    }
    let first = text_of(&cards[0], "id");
    let second = text_of(&cards[1], "id");
    let third = text_of(&cards[2], "id");

    let listed = body(
        server.get(&format!("/api/columns/{todo}/cards")).await,
        StatusCode::OK,
        "GET /api/columns/:id/cards",
    );
    assert_list_of(&listed, Shape::Card, "GET /api/columns/:id/cards");

    let card = body(
        server.get(&format!("/api/cards/{first}")).await,
        StatusCode::OK,
        "GET /api/cards/:id",
    );
    assert_shape(&card, Shape::Card, "GET /api/cards/:id");

    let number = card["number"].as_u64().expect("card number");
    let by_number = body(
        server.get(&format!("/api/cards/by-number/{number}")).await,
        StatusCode::OK,
        "GET /api/cards/by-number/:n",
    );
    assert_shape(&by_number, Shape::Card, "GET /api/cards/by-number/:n");

    let updated = body(
        server
            .put(&format!("/api/cards/{first}"))
            .json(&shared::UpdateCardRequest {
                body: Some("# Contract card 0, edited".to_string()),
                tags: Some(vec!["edited".to_string()]),
                ..Default::default()
            })
            .await,
        StatusCode::OK,
        "PUT /api/cards/:id",
    );
    assert_shape(&updated, Shape::Card, "PUT /api/cards/:id");
    drain_events(&mut rx, &mut seen);

    let moved = body(
        server
            .post(&format!("/api/cards/{third}/move"))
            .json(&shared::MoveCardRequest {
                column_id: doing.clone(),
                position: 0,
            })
            .await,
        StatusCode::OK,
        "POST /api/cards/:id/move",
    );
    assert_shape(&moved, Shape::Card, "POST /api/cards/:id/move");
    drain_events(&mut rx, &mut seen);

    let reordered = body(
        server
            .put(&format!("/api/columns/{todo}/cards/reorder"))
            .json(&shared::CardsReorderRequest {
                order: vec![second.clone(), first.clone()],
            })
            .await,
        StatusCode::OK,
        "PUT /api/columns/:id/cards/reorder",
    );
    assert_list_of(
        &reordered,
        Shape::Card,
        "PUT /api/columns/:id/cards/reorder",
    );
    drain_events(&mut rx, &mut seen);

    // ── Links ───────────────────────────────────────────────────────────
    let link = body(
        server
            .post(&format!("/api/cards/{first}/links"))
            .json(&shared::CreateCardLinkRequest {
                direction: shared::LinkDirection::Successor,
                other_card_id: second.clone(),
                reason: Some("contract".to_string()),
            })
            .await,
        StatusCode::CREATED,
        "POST /api/cards/:id/links",
    );
    assert_shape(&link, Shape::CardLink, "POST /api/cards/:id/links");
    drain_events(&mut rx, &mut seen);
    let link_id = text_of(&link, "id");

    let links = body(
        server.get(&format!("/api/boards/{slug}/links")).await,
        StatusCode::OK,
        "GET /api/boards/:slug/links",
    );
    assert_list_of(&links, Shape::CardLink, "GET /api/boards/:slug/links");

    let link = body(
        server
            .put(&format!("/api/links/{link_id}"))
            .json(&shared::UpdateCardLinkRequest {
                reason: Some("contract, revised".to_string()),
            })
            .await,
        StatusCode::OK,
        "PUT /api/links/:id",
    );
    assert_shape(&link, Shape::CardLink, "PUT /api/links/:id");
    drain_events(&mut rx, &mut seen);

    // ── History and restore ─────────────────────────────────────────────
    let board_history = body(
        server.get(&format!("/api/boards/{slug}/history")).await,
        StatusCode::OK,
        "GET /api/boards/:slug/history",
    );
    assert_list_of(
        &board_history,
        Shape::AuditEntry,
        "GET /api/boards/:slug/history",
    );

    let column_history = body(
        server.get(&format!("/api/columns/{doing}/history")).await,
        StatusCode::OK,
        "GET /api/columns/:id/history",
    );
    assert_list_of(
        &column_history,
        Shape::AuditEntry,
        "GET /api/columns/:id/history",
    );

    let card_history = body(
        server.get(&format!("/api/cards/{first}/history")).await,
        StatusCode::OK,
        "GET /api/cards/:id/history",
    );
    assert_list_of(
        &card_history,
        Shape::AuditEntry,
        "GET /api/cards/:id/history",
    );

    // Restore the card's original version (its own `create` row — a card's
    // history also lists its links' rows): the card-content restore path.
    let create_row = card_history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|row| row["action"] == "create" && row["entity_type"] == "card")
        .expect("card has a create row");
    let restored = body(
        server
            .post(&format!("/api/audit/{}/restore", text_of(create_row, "id")))
            .await,
        StatusCode::OK,
        "POST /api/audit/:id/restore (version)",
    );
    assert_list_of(
        &restored,
        Shape::AuditEntry,
        "POST /api/audit/:id/restore (version)",
    );
    drain_events(&mut rx, &mut seen);

    // ── A column rebalance (`cards_renumbered`) ─────────────────────────
    // Moving the bottom card to the top halves the gap above the first card
    // each time; after about ten moves it is used up and the column is
    // renumbered (see `tests::sse::a_rebalance_announces_every_renumbered_card`).
    for _ in 0..24 {
        if seen.contains("cards_renumbered") {
            break;
        }
        let column: Vec<shared::Card> = server
            .get(&format!("/api/columns/{todo}/cards"))
            .await
            .json();
        let bottom = column.last().expect("column has cards");
        let moved = body(
            server
                .post(&format!("/api/cards/{}/move", bottom.id))
                .json(&shared::MoveCardRequest {
                    column_id: todo.clone(),
                    position: 0,
                })
                .await,
            StatusCode::OK,
            "POST /api/cards/:id/move (to top)",
        );
        assert_shape(&moved, Shape::Card, "POST /api/cards/:id/move (to top)");
        drain_events(&mut rx, &mut seen);
    }

    // ── Identity and deployment info ────────────────────────────────────
    let me = body(server.get("/api/me").await, StatusCode::OK, "GET /api/me");
    assert_shape(&me, Shape::UserInfo, "GET /api/me");
    let info = body(
        server.get("/api/info").await,
        StatusCode::OK,
        "GET /api/info",
    );
    assert_shape(&info, Shape::AppInfo, "GET /api/info");

    // ── Errors carry no database text ───────────────────────────────────
    assert_error(
        server.get("/api/cards/01aaaaaaaaaaaaaaaaaaaaaaaa").await,
        StatusCode::NOT_FOUND,
        "GET /api/cards/:missing",
    );
    assert_error(
        server.get("/api/boards/no-such-board").await,
        StatusCode::NOT_FOUND,
        "GET /api/boards/:missing",
    );
    assert_error(
        server
            .post(&format!("/api/cards/{first}/links"))
            .json(&shared::CreateCardLinkRequest {
                direction: shared::LinkDirection::Successor,
                other_card_id: second.clone(),
                reason: None,
            })
            .await,
        StatusCode::CONFLICT,
        "POST /api/cards/:id/links (duplicate)",
    );

    // ── Deletes, and restoring one ──────────────────────────────────────
    assert_no_content(
        server.delete(&format!("/api/links/{link_id}")).await,
        "DELETE /api/links/:id",
    );
    drain_events(&mut rx, &mut seen);

    assert_no_content(
        server.delete(&format!("/api/cards/{second}")).await,
        "DELETE /api/cards/:id",
    );
    drain_events(&mut rx, &mut seen);

    // Restoring a deletion recreates the card and emits `card_created` from
    // the restore path (`audit::restore_one_delete`), not the create route.
    // A deleted card has no history route of its own (its column lookup
    // 404s), so find the deletion on the board's history.
    let history = body(
        server.get(&format!("/api/boards/{slug}/history")).await,
        StatusCode::OK,
        "GET /api/boards/:slug/history (after delete)",
    );
    assert_list_of(
        &history,
        Shape::AuditEntry,
        "GET /api/boards/:slug/history (after delete)",
    );
    let delete_row = history
        .as_array()
        .expect("history is a list")
        .iter()
        .find(|row| {
            row["action"] == "delete" && row["entity_type"] == "card" && row["entity_id"] == second
        })
        .expect("card has a delete row");
    let restored = body(
        server
            .post(&format!("/api/audit/{}/restore", text_of(delete_row, "id")))
            .await,
        StatusCode::OK,
        "POST /api/audit/:id/restore (delete)",
    );
    assert_list_of(
        &restored,
        Shape::AuditEntry,
        "POST /api/audit/:id/restore (delete)",
    );
    drain_events(&mut rx, &mut seen);

    assert_no_content(
        server.delete(&format!("/api/columns/{done}")).await,
        "DELETE /api/columns/:id",
    );
    drain_events(&mut rx, &mut seen);

    assert_no_content(
        server.delete(&format!("/api/boards/{slug}")).await,
        "DELETE /api/boards/:slug",
    );
    drain_events(&mut rx, &mut seen);

    // ── Every event type was emitted and checked ────────────────────────
    let expected: BTreeSet<&str> = ALL_EVENT_TYPES.iter().copied().collect();
    assert_eq!(seen, expected, "event types the scenario emitted");
}

/// Which kind of entity a legacy-restore test deletes and restores. Each one
/// takes a different arm of `audit::restore_one_delete`; a board or column
/// deletion is a cascade, so its restore replays the whole batch.
#[derive(Clone, Copy, Debug)]
enum Deleted {
    Board,
    Column,
    Card,
}

/// Restoring a deletion recorded before #472 must not put its legacy
/// `d'…'` timestamps back on the wire.
///
/// The restore used to broadcast, and snapshot, the *audit snapshot* it was
/// recreating from. Old snapshots still hold SurrealQL literals until #471's
/// data migration rewrites them, so this plants them — rewriting every real
/// delete row's snapshot timestamps to the pre-#472 form — then restores
/// `target`'s deletion and checks the response, every event it caused, and
/// the recreated entity read back. The recreated entity must also carry a
/// fresh `created_at` (the schema default), not the deleted row's.
async fn restore_legacy_deletion(target: Deleted) {
    let (server, mut rx, db) = contract_server().await;
    let (board, column) = setup_board_and_column(&server).await;
    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Legacy".to_string(),
            ..Default::default()
        })
        .await
        .json();

    // Delete the target. Deleting a board or column cascades to what it
    // holds, and records one delete row per entity under a shared batch.
    let (delete_path, entity_id, original_created_at, created_event) = match target {
        Deleted::Board => (
            format!("/api/boards/{}", board.name),
            board.id.clone(),
            board.created_at.clone(),
            "board_created",
        ),
        Deleted::Column => (
            format!("/api/columns/{}", column.id),
            column.id.clone(),
            column.created_at.clone(),
            "column_created",
        ),
        Deleted::Card => (
            format!("/api/cards/{}", card.id),
            card.id.clone(),
            card.created_at.clone(),
            "card_created",
        ),
    };
    server
        .delete(&delete_path)
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // Exactly what `Datetime::to_string()` wrote before #472, planted on
    // every delete row so each entity a batch restore recreates has a legacy
    // snapshot behind it.
    let legacy = "d'2026-05-07T01:27:04.823026281Z'";
    db.query(
        "UPDATE audit_log SET \
         snapshot_before.created_at = $legacy, snapshot_before.updated_at = $legacy \
         WHERE action = 'delete'",
    )
    .bind(("legacy", legacy))
    .await
    .expect("plant legacy snapshots")
    .check()
    .expect("plant legacy snapshots");

    // Found through the database: a deleted board's history route 404s.
    let mut found = db
        .query("SELECT * FROM audit_log WHERE action = 'delete' AND entity_id = $id")
        .bind(("id", entity_id.clone()))
        .await
        .expect("find delete row");
    let rows: Vec<crate::models::DbAuditLog> = found.take(0).expect("delete rows");
    let [delete_row] = rows.as_slice() else {
        panic!("{target:?}: expected one delete row, got {}", rows.len());
    };
    // The plant took, which is what makes the restore below a real test.
    assert_eq!(
        delete_row.snapshot_before.as_ref().expect("snapshot")["created_at"],
        legacy,
        "{target:?}: legacy snapshot planted"
    );

    // Only the restore's own events matter from here.
    while rx.try_recv().is_ok() {}

    let context = format!("POST /api/audit/:id/restore (legacy {target:?})");
    let restored = body(
        server
            .post(&format!("/api/audit/{}/restore", delete_row.id.id.to_raw()))
            .await,
        StatusCode::OK,
        &context,
    );
    assert_list_of(&restored, Shape::AuditEntry, &context);

    let mut seen = BTreeSet::new();
    drain_events(&mut rx, &mut seen);
    assert!(
        seen.contains(created_event),
        "{target:?}: restore announced {created_event}: {seen:?}"
    );

    // The recreated entity reads back clean, with a fresh `created_at`.
    let (fetched, shape) = match target {
        Deleted::Board => (
            body(
                server.get(&format!("/api/boards/{}", board.name)).await,
                StatusCode::OK,
                "GET /api/boards/:slug (restored)",
            ),
            Shape::Board,
        ),
        Deleted::Column => {
            let columns = body(
                server
                    .get(&format!("/api/boards/{}/columns", board.name))
                    .await,
                StatusCode::OK,
                "GET /api/boards/:slug/columns (restored)",
            );
            let restored = columns
                .as_array()
                .expect("columns list")
                .iter()
                .find(|c| c["id"] == column.id.as_str())
                .expect("restored column listed")
                .clone();
            (restored, Shape::Column)
        }
        Deleted::Card => (
            body(
                server.get(&format!("/api/cards/{}", card.id)).await,
                StatusCode::OK,
                "GET /api/cards/:id (restored)",
            ),
            Shape::Card,
        ),
    };
    assert_shape(&fetched, shape, &format!("{target:?} (restored)"));
    assert_ne!(
        fetched["created_at"], original_created_at,
        "{target:?}: the recreated row has its own created_at"
    );
}

#[tokio::test]
async fn restoring_a_legacy_board_deletion_emits_clean_payloads() {
    restore_legacy_deletion(Deleted::Board).await;
}

#[tokio::test]
async fn restoring_a_legacy_column_deletion_emits_clean_payloads() {
    restore_legacy_deletion(Deleted::Column).await;
}

#[tokio::test]
async fn restoring_a_legacy_card_deletion_emits_clean_payloads() {
    restore_legacy_deletion(Deleted::Card).await;
}
