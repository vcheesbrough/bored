//! Card tags (iteration 41 — card #292): persistence, normalisation, audit rows
//! and restore.

use super::*;

#[tokio::test]
async fn create_card_persists_tags() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let create_resp = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Tagged".to_string(),
            tags: vec!["bug".to_string(), "urgent".to_string()],
        })
        .await;
    create_resp.assert_status(StatusCode::CREATED);
    let card: shared::Card = create_resp.json();
    assert_eq!(card.tags, vec!["bug".to_string(), "urgent".to_string()]);

    // Round-trips through a fresh read, not just the create response.
    let fetched: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
    assert_eq!(fetched.tags, vec!["bug".to_string(), "urgent".to_string()]);
}

#[tokio::test]
async fn create_card_without_tags_defaults_to_empty_list() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Plain".to_string(),
            ..Default::default()
        })
        .await
        .json();
    assert!(card.tags.is_empty());
}

#[tokio::test]
async fn update_card_replaces_the_whole_tag_list() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Tagged".to_string(),
            tags: vec!["bug".to_string(), "stale".to_string()],
        })
        .await
        .json();

    let updated: shared::Card = server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            tags: Some(vec!["fresh".to_string()]),
            ..Default::default()
        })
        .await
        .json();
    // Full replace, not a merge: both original tags are gone.
    assert_eq!(updated.tags, vec!["fresh".to_string()]);
    // A tags-only update leaves the body alone.
    assert_eq!(updated.body, "# Tagged");
}

#[tokio::test]
async fn update_card_omitting_tags_leaves_them_untouched() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Tagged".to_string(),
            tags: vec!["keep".to_string()],
        })
        .await
        .json();

    let updated: shared::Card = server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("# Retitled".to_string()),
            ..Default::default()
        })
        .await
        .json();
    assert_eq!(updated.tags, vec!["keep".to_string()]);
}

#[tokio::test]
async fn tags_are_normalized_on_the_way_in() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Messy tags".to_string(),
            tags: vec![
                "  #bug ".to_string(),
                "BUG".to_string(),
                "two words".to_string(),
                "   ".to_string(),
            ],
        })
        .await
        .json();
    // `#` stripped, whitespace split, case-insensitive dedup, empties dropped.
    assert_eq!(
        card.tags,
        vec!["bug".to_string(), "two".to_string(), "words".to_string()]
    );
}

#[tokio::test]
async fn over_long_tag_is_rejected_with_422() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let too_long = "x".repeat(shared::tags::MAX_TAG_CHARS + 1);
    server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Nope".to_string(),
            tags: vec![too_long.clone()],
        })
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Fine".to_string(),
            ..Default::default()
        })
        .await
        .json();
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            tags: Some(vec![too_long]),
            ..Default::default()
        })
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn tag_change_records_a_discrete_update_audit_row() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Tag me".to_string(),
            ..Default::default()
        })
        .await
        .json();

    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            tags: Some(vec!["bug".to_string()]),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();
    let updates: Vec<_> = hist.iter().filter(|e| e.action == "update").collect();
    assert_eq!(updates.len(), 1);
    let row = updates[0];
    assert_eq!(
        row.snapshot_before
            .as_ref()
            .and_then(|v| v.get("tags"))
            .and_then(|v| v.as_array())
            .map(Vec::len),
        Some(0)
    );
    assert_eq!(
        row.snapshot_after
            .as_ref()
            .and_then(|v| v.get("tags"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str()),
        Some("bug")
    );
    // A tag change is content, not layout — never filed under "move".
    assert!(!hist.iter().any(|e| e.action == "move"));
}

#[tokio::test]
async fn tag_change_does_not_merge_into_a_body_edit_session() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "alpha".to_string(),
            ..Default::default()
        })
        .await
        .json();

    let sess = "tag-session-test";
    // A body edit inside an edit session…
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("beta".to_string()),
            audit_edit_session: Some(sess.to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();
    // …then a tag change carrying the *same* session token. It must still
    // land as its own row rather than being folded into the body edit.
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            tags: Some(vec!["bug".to_string()]),
            audit_edit_session: Some(sess.to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();
    let updates: Vec<_> = hist.iter().filter(|e| e.action == "update").collect();
    assert_eq!(
        updates.len(),
        2,
        "tag change must not merge into the session"
    );
    // And a later body save can no longer merge into the tag row either,
    // because that row carries no session token.
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("gamma".to_string()),
            audit_edit_session: Some(sess.to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();
    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();
    assert_eq!(hist.iter().filter(|e| e.action == "update").count(), 3);
}

#[tokio::test]
async fn re_sending_identical_tags_is_not_an_edit() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Steady".to_string(),
            tags: vec!["bug".to_string()],
        })
        .await
        .json();

    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            tags: Some(vec!["bug".to_string()]),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();
    assert_eq!(hist.iter().filter(|e| e.action == "update").count(), 0);
}

#[tokio::test]
async fn restoring_a_version_restores_body_and_tags_together() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Original".to_string(),
            tags: vec!["first".to_string()],
        })
        .await
        .json();

    // Move both halves away from the create snapshot.
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("# Rewritten".to_string()),
            tags: Some(vec!["second".to_string()]),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();
    let create_row = hist
        .iter()
        .find(|e| e.action == "create")
        .expect("create row");

    server
        .post(&format!("/api/audit/{}/restore", create_row.id))
        .await
        .assert_status_ok();

    let restored: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
    assert_eq!(restored.body, "# Original");
    assert_eq!(restored.tags, vec!["first".to_string()]);
}

#[tokio::test]
async fn restoring_a_tags_only_version_is_not_a_conflict() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Same body".to_string(),
            tags: vec!["keep".to_string()],
        })
        .await
        .json();

    // Only the tags change, so the create row's body already matches the
    // card. Restoring it must still put the old tags back rather than
    // reporting "already current".
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            tags: Some(vec!["dropped".to_string()]),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();
    let create_row = hist
        .iter()
        .find(|e| e.action == "create")
        .expect("create row");

    server
        .post(&format!("/api/audit/{}/restore", create_row.id))
        .await
        .assert_status_ok();

    let restored: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
    assert_eq!(restored.tags, vec!["keep".to_string()]);
}

#[tokio::test]
async fn restoring_a_deleted_card_brings_its_tags_back() {
    let server = test_app().await;
    let (board, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Doomed".to_string(),
            tags: vec!["bug".to_string()],
        })
        .await
        .json();

    server
        .delete(&format!("/api/cards/{}", card.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // The card-scoped endpoint 404s once the card is gone, so read the
    // delete row from the board's history instead.
    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    let delete_row = hist
        .iter()
        .find(|e| e.action == "delete" && e.entity_id == card.id)
        .expect("delete row");

    server
        .post(&format!("/api/audit/{}/restore", delete_row.id))
        .await
        .assert_status_ok();

    let restored: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
    assert_eq!(restored.tags, vec!["bug".to_string()]);
}

#[tokio::test]
async fn legacy_cards_without_a_tags_field_stay_readable_and_editable() {
    // Rows written before `tags` was defined have no value for it at all.
    // Reproduce that by applying the schema with the tags lines stripped,
    // writing a card, then applying the real schema on top — exactly what a
    // deploy of this iteration does to an existing database.
    let schema = include_str!("../schema.surql");
    let legacy_schema: String = schema
        .lines()
        .filter(|line| !line.contains("tags"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !legacy_schema.contains("tags"),
        "legacy schema must not define tags"
    );

    let db = surrealdb::Surreal::new::<surrealdb::engine::local::Mem>(())
        .await
        .expect("mem db");
    db.use_ns("bored").use_db("bored").await.unwrap();
    db.query(legacy_schema).await.unwrap().check().unwrap();

    let bid = ulid::Ulid::new().to_string().to_lowercase();
    let cid = ulid::Ulid::new().to_string().to_lowercase();
    let kid = ulid::Ulid::new().to_string().to_lowercase();
    db.query("CREATE type::thing('boards', $bid) SET name = $name")
        .bind(("bid", bid.clone()))
        .bind(("name", format!("legacy-{bid}")))
        .await
        .unwrap()
        .check()
        .unwrap();
    db.query(
        "CREATE type::thing('columns', $cid) SET board = type::thing('boards', $bid), \
         name = 'Col', position = 0",
    )
    .bind(("cid", cid.clone()))
    .bind(("bid", bid.clone()))
    .await
    .unwrap()
    .check()
    .unwrap();
    db.query(
        "CREATE type::thing('cards', $kid) SET column = type::thing('columns', $cid), \
         body = 'legacy', position = 0, number = 1",
    )
    .bind(("kid", kid.clone()))
    .bind(("cid", cid.clone()))
    .await
    .unwrap()
    .check()
    .unwrap();

    // Now upgrade: the real schema defines `tags` and backfills it.
    db.query(schema).await.unwrap().check().unwrap();

    let card: Option<crate::models::DbCard> = db.select(("cards", &kid)).await.unwrap();
    let card = card.expect("legacy card still readable");
    assert!(card.tags.is_empty());

    // The backfill is what makes this work: on a SCHEMAFULL table an UPDATE
    // of a row whose `tags` is still missing fails the type check.
    db.query("UPDATE type::thing('cards', $kid) SET body = 'edited'")
        .bind(("kid", kid))
        .await
        .unwrap()
        .check()
        .unwrap();
}
