//! Audit log behaviour: baseline backfill, edit-session merging, history and restore.

use super::*;

#[tokio::test]
async fn audit_baseline_backfill_inserts_once_per_entity() {
    let db = db::connect_mem().await.expect("mem db");
    let bid = ulid::Ulid::new().to_string().to_lowercase();
    let cid = ulid::Ulid::new().to_string().to_lowercase();
    let kid = ulid::Ulid::new().to_string().to_lowercase();

    db.query("CREATE type::thing('boards', $bid) SET name = $name, last_edited_by = $sub")
        .bind(("bid", bid.clone()))
        .bind(("name", format!("seed-{bid}")))
        .bind(("sub", "preaudit-board-editor"))
        .await
        .unwrap()
        .check()
        .unwrap();

    db.query(
        "CREATE type::thing('columns', $cid) SET board = type::thing('boards', $bid), \
         name = $cname, position = 0, last_edited_by = $sub",
    )
    .bind(("cid", cid.clone()))
    .bind(("bid", bid.clone()))
    .bind(("cname", "Col"))
    .bind(("sub", "preaudit-column-editor"))
    .await
    .unwrap()
    .check()
    .unwrap();

    db.query(
        "CREATE type::thing('cards', $kid) SET column = type::thing('columns', $cid), \
         body = $body, position = 0, number = 1, last_edited_by = $sub",
    )
    .bind(("kid", kid.clone()))
    .bind(("cid", cid.clone()))
    .bind(("body", "hello"))
    .bind(("sub", "preaudit-card-editor"))
    .await
    .unwrap()
    .check()
    .unwrap();

    audit::migrate_audit_baselines(&db).await.unwrap();

    let baselines: Vec<crate::models::DbAuditLog> = db
        .query("SELECT * FROM audit_log WHERE action = 'baseline' ORDER BY entity_type ASC")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(baselines.len(), 3);

    let board_row = baselines.iter().find(|r| r.entity_type == "board").unwrap();
    assert_eq!(board_row.entity_id, bid);
    assert_eq!(board_row.actor_sub, "preaudit-board-editor");

    audit::migrate_audit_baselines(&db).await.unwrap();
    let baselines_again: Vec<crate::models::DbAuditLog> = db
        .query("SELECT * FROM audit_log WHERE action = 'baseline'")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(baselines_again.len(), 3);
}

#[tokio::test]
async fn card_updates_same_audit_session_merge_into_one_row() {
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

    let sess = "merge-test-session";
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("beta".to_string()),
            audit_edit_session: Some(sess.to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();
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

    let updates: Vec<_> = hist.iter().filter(|e| e.action == "update").collect();
    assert_eq!(updates.len(), 1);
    let row = updates[0];
    assert_eq!(
        row.snapshot_before
            .as_ref()
            .and_then(|v| v.get("body"))
            .and_then(|v| v.as_str()),
        Some("alpha")
    );
    assert_eq!(
        row.snapshot_after
            .as_ref()
            .and_then(|v| v.get("body"))
            .and_then(|v| v.as_str()),
        Some("gamma")
    );
}

#[tokio::test]
async fn column_history_endpoint_returns_cards_for_that_column_only() {
    let server = test_app().await;
    let (board, col_a) = setup_board_and_column(&server).await;

    let col_b: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "B".to_string(),
            position: 1,
        })
        .await
        .json();

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", col_a.id))
        .json(&shared::CreateCardRequest {
            body: "only-a".to_string(),
            ..Default::default()
        })
        .await
        .json();

    let hist_a: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/columns/{}/history", col_a.id))
        .await
        .json();

    assert!(
        hist_a
            .iter()
            .any(|e| { e.entity_type == "card" && e.entity_id == card.id && e.action == "create" })
    );
    assert!(
        !hist_a
            .iter()
            .any(|e| e.entity_type == "column" && e.entity_id == col_b.id)
    );
}

#[tokio::test]
async fn card_updates_without_audit_session_stay_separate_rows() {
    let server = test_app().await;
    let (_, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "a".to_string(),
            ..Default::default()
        })
        .await
        .json();

    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("b".to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();
    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("c".to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();

    let updates: Vec<_> = hist.iter().filter(|e| e.action == "update").collect();
    assert_eq!(updates.len(), 2);
}

#[tokio::test]
async fn update_card_body_and_column_change_audit_action_is_update_not_move() {
    let server = test_app().await;
    let (board, col_a) = setup_board_and_column(&server).await;

    let col_b: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Col B".to_string(),
            position: 1,
        })
        .await
        .json();

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", col_a.id))
        .json(&shared::CreateCardRequest {
            body: "original".to_string(),
            ..Default::default()
        })
        .await
        .json();

    server
        .put(&format!("/api/cards/{}", card.id))
        .json(&shared::UpdateCardRequest {
            body: Some("edited after move".to_string()),
            column_id: Some(col_b.id.clone()),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let hist: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();

    let layout_mutations: Vec<_> = hist
        .iter()
        .filter(|e| e.action == "update" || e.action == "move")
        .collect();
    assert_eq!(layout_mutations.len(), 1);
    assert_eq!(layout_mutations[0].action, "update");
}

#[tokio::test]
async fn audit_delete_card_then_restore_via_audit_endpoint() {
    let server = test_app().await;
    let (board, column) = setup_board_and_column(&server).await;

    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Audit restore target".to_string(),
            ..Default::default()
        })
        .await
        .json();

    server
        .delete(&format!("/api/cards/{}", card.id))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let hist_resp = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await;
    hist_resp.assert_status_ok();
    let hist: Vec<shared::AuditLogEntry> = hist_resp.json();
    let delete_row = hist
        .iter()
        .find(|e| e.action == "delete" && e.entity_type == "card" && e.entity_id == card.id)
        .expect("delete audit row present");

    let restore_resp = server
        .post(&format!("/api/audit/{}/restore", delete_row.id))
        .await;
    restore_resp.assert_status_ok();

    server
        .get(&format!("/api/cards/{}", card.id))
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn audit_restore_prior_card_body_preserves_card_identity_and_layout() {
    let server = test_app().await;
    let (board, column) = setup_board_and_column(&server).await;

    let original: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "# Version A".to_string(),
            ..Default::default()
        })
        .await
        .json();
    server
        .put(&format!("/api/cards/{}", original.id))
        .json(&shared::UpdateCardRequest {
            body: Some("# Version B".to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();
    server
        .put(&format!("/api/cards/{}", original.id))
        .json(&shared::UpdateCardRequest {
            body: Some("# Version C".to_string()),
            ..Default::default()
        })
        .await
        .assert_status_ok();

    let history: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", original.id))
        .await
        .json();
    let version_b = history
        .iter()
        .find(|entry| {
            entry.action == "update"
                && entry
                    .snapshot_after
                    .as_ref()
                    .and_then(|value| value.get("body"))
                    .and_then(|value| value.as_str())
                    == Some("# Version B")
        })
        .expect("Version B update present");

    let restore_response = server
        .post(&format!("/api/audit/{}/restore", version_b.id))
        .await;
    restore_response.assert_status_ok();
    let restored_rows: Vec<shared::AuditLogEntry> = restore_response.json();
    assert_eq!(restored_rows.len(), 1);
    let restore = &restored_rows[0];
    assert_eq!(restore.action, "restore");
    assert_eq!(
        restore.restored_from.as_deref(),
        Some(version_b.id.as_str())
    );
    assert_eq!(
        restore
            .snapshot_before
            .as_ref()
            .and_then(|value| value.get("body"))
            .and_then(|value| value.as_str()),
        Some("# Version C")
    );

    let restored: shared::Card = server
        .get(&format!("/api/cards/{}", original.id))
        .await
        .json();
    assert_eq!(restored.body, "# Version B");
    assert_eq!(restored.id, original.id);
    assert_eq!(restored.number, original.number);
    assert_eq!(restored.column_id, original.column_id);
    assert_eq!(restored.position, original.position);

    let board_history: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    assert!(board_history.iter().any(|entry| entry.id == restore.id));
}

#[tokio::test]
async fn audit_body_restore_rejects_invalid_or_current_versions() {
    let db = db::connect_mem().await.expect("failed to connect mem db");
    let state = AppState::new(db.clone());
    let server =
        TestServer::new(app(state, "./dist", DeploymentInfo::new("dev", None)).await).unwrap();
    let (board, column) = setup_board_and_column(&server).await;
    let card: shared::Card = server
        .post(&format!("/api/columns/{}/cards", column.id))
        .json(&shared::CreateCardRequest {
            body: "current body".to_string(),
            ..Default::default()
        })
        .await
        .json();

    let card_history: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();
    let current = card_history
        .iter()
        .find(|entry| entry.action == "create")
        .expect("current create version present");
    server
        .post(&format!("/api/audit/{}/restore", current.id))
        .await
        .assert_status(StatusCode::CONFLICT);

    let malformed_id = ulid::Ulid::new().to_string().to_lowercase();
    db.query(
        "CREATE type::thing('audit_log', $id) SET \
         actor_sub = 'test', actor_display_name = 'Test', \
         entity_type = 'card', entity_id = $entity_id, board_id = $board_id, \
         action = 'update', snapshot_before = NONE, snapshot_after = $snapshot_after, \
         restored_from = NONE, batch_group = NONE, audit_edit_session = NONE",
    )
    .bind(("id", malformed_id.clone()))
    .bind(("entity_id", card.id.clone()))
    .bind(("board_id", board.id.clone()))
    .bind(("snapshot_after", serde_json::json!({ "id": card.id })))
    .await
    .expect("insert malformed audit row")
    .check()
    .expect("malformed audit row accepted by storage");
    server
        .post(&format!("/api/audit/{malformed_id}/restore"))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    let board_history: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/boards/{}/history", board.name))
        .await
        .json();
    let board_create = board_history
        .iter()
        .find(|entry| entry.entity_type == "board" && entry.action == "create")
        .expect("board create row present");
    server
        .post(&format!("/api/audit/{}/restore", board_create.id))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    let other_column: shared::Column = server
        .post(&format!("/api/boards/{}/columns", board.name))
        .json(&shared::CreateColumnRequest {
            name: "Other".to_string(),
            position: 1,
        })
        .await
        .json();
    server
        .post(&format!("/api/cards/{}/move", card.id))
        .json(&shared::MoveCardRequest {
            column_id: other_column.id,
            position: 0,
        })
        .await
        .assert_status_ok();
    let history_after_move: Vec<shared::AuditLogEntry> = server
        .get(&format!("/api/cards/{}/history", card.id))
        .await
        .json();
    let move_row = history_after_move
        .iter()
        .find(|entry| entry.action == "move")
        .expect("move row present");
    server
        .post(&format!("/api/audit/{}/restore", move_row.id))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    let unchanged: shared::Card = server.get(&format!("/api/cards/{}", card.id)).await.json();
    assert_eq!(unchanged.body, "current body");
}
