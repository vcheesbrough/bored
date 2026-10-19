use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};

use crate::audit;
use crate::auth::Claims;
use crate::db::{DbOperation, DbQuery, Traced as _};
use crate::error::ApiError;
use crate::events::{BoardEvent, BroadcastEvent};
use crate::models::{DbCard, DbColumn};

/// One column by id, or `None`. Shared by every handler here that starts from
/// a column in the path.
async fn load_column(
    state: &AppState,
    col_id: &str,
    name: &'static str,
) -> Result<Option<DbColumn>, ApiError> {
    sqlx::query_as("SELECT * FROM columns WHERE id = $1")
        .bind(col_id)
        .fetch_optional(&state.db)
        .traced(DbQuery::new(name, DbOperation::Select, "columns"))
        .await
        .map_err(ApiError::from)
}

/// A board's columns in display order. `id` breaks ties so two columns that
/// share a position still come back in a stable order.
async fn board_columns(
    state: &AppState,
    board_ulid: &str,
    name: &'static str,
) -> Result<Vec<DbColumn>, ApiError> {
    sqlx::query_as("SELECT * FROM columns WHERE board_id = $1 ORDER BY position, id")
        .bind(board_ulid)
        .fetch_all(&state.db)
        .traced(DbQuery::new(name, DbOperation::Select, "columns"))
        .await
        .map_err(ApiError::from)
}
use crate::routes::boards::{AppState, editor_sub, find_board_by_slug};

#[tracing::instrument(skip_all, fields(bored.board.slug = %board_slug))]
pub async fn list_columns(
    State(state): State<AppState>,
    Path(board_slug): Path<String>,
) -> Result<Json<Vec<shared::Column>>, ApiError> {
    let board = match find_board_by_slug(&state.db, &board_slug).await? {
        Some(b) => b,
        None => return Err(ApiError::NotFound),
    };
    let columns = board_columns(&state, &board.id, "columns.list_columns").await?;

    Ok(Json(columns.into_iter().map(DbColumn::into_api).collect()))
}

#[tracing::instrument(skip_all, fields(bored.board.slug = %board_slug))]
pub async fn create_column(
    State(state): State<AppState>,
    Path(board_slug): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::CreateColumnRequest>,
) -> Result<(StatusCode, Json<shared::Column>), ApiError> {
    let board = match find_board_by_slug(&state.db, &board_slug).await? {
        Some(b) => b,
        None => return Err(ApiError::NotFound),
    };
    let board_ulid = board.id;

    let id = ulid::Ulid::new().to_string().to_lowercase();
    let editor = editor_sub(&claims);

    let column: Option<DbColumn> = sqlx::query_as(
        "INSERT INTO columns (id, board_id, name, position, last_edited_by) \
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(&id)
    .bind(&board_ulid)
    .bind(&payload.name)
    .bind(payload.position)
    .bind(&editor)
    .fetch_optional(&state.db)
    .traced(DbQuery::new(
        "columns.create_column",
        DbOperation::Insert,
        "columns",
    ))
    .await?;

    match column {
        Some(c) => {
            let api_col = c.into_api();
            let snapshot_after = serde_json::to_value(api_col.clone())?;
            audit::record_and_broadcast(
                &state.db,
                &state.events,
                audit::AuditRecord {
                    claims: &claims,
                    board_id: board_ulid.clone(),
                    entity_type: "column",
                    entity_id: &api_col.id,
                    action: "create",
                    snapshot_before: None,
                    snapshot_after: Some(snapshot_after),
                    restored_from: None,
                    batch_group: None,
                    audit_edit_session: None,
                },
            )
            .await?;

            let _ = state.events.send(BroadcastEvent {
                board_id: board_ulid,
                event: BoardEvent::ColumnCreated {
                    column: api_col.clone(),
                },
            });
            Ok((StatusCode::CREATED, Json(api_col)))
        }
        // A create that reports success always returns its row.
        None => Err(ApiError::internal("create returned no column row")),
    }
}

#[tracing::instrument(skip_all, fields(bored.column.id = %col_id))]
pub async fn update_column(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::UpdateColumnRequest>,
) -> Result<Json<shared::Column>, ApiError> {
    let existing = load_column(&state, &col_id, "columns.update_column").await?;

    // Destructure early to capture the board ID for the SSE event.
    let existing = match existing {
        Some(c) => c,
        None => return Err(ApiError::NotFound),
    };
    let board_id = existing.board_id.clone();

    // A partial update: only the fields the request supplied change.
    if payload.name.is_none() && payload.position.is_none() {
        return Ok(Json(existing.into_api()));
    }
    let snapshot_before = serde_json::to_value(existing.clone().into_api())?;

    // `COALESCE($n, column)` keeps the stored value for a field the request
    // left out (bound as NULL), so one fixed statement covers every
    // combination.
    let column: Option<DbColumn> = sqlx::query_as(
        "UPDATE columns SET name = COALESCE($2, name), position = COALESCE($3, position), \
         last_edited_by = $4 WHERE id = $1 RETURNING *",
    )
    .bind(&col_id)
    .bind(payload.name.as_deref())
    .bind(payload.position)
    .bind(editor_sub(&claims))
    .fetch_optional(&state.db)
    .traced(DbQuery::new(
        "columns.update_column",
        DbOperation::Update,
        "columns",
    ))
    .await?;

    match column {
        Some(c) => {
            let api_col = c.into_api();
            let snapshot_after = serde_json::to_value(api_col.clone())?;
            let action = if payload.position.is_some() {
                "move"
            } else {
                "update"
            };

            audit::record_and_broadcast(
                &state.db,
                &state.events,
                audit::AuditRecord {
                    claims: &claims,
                    board_id: board_id.clone(),
                    entity_type: "column",
                    entity_id: &api_col.id,
                    action,
                    snapshot_before: Some(snapshot_before),
                    snapshot_after: Some(snapshot_after),
                    restored_from: None,
                    batch_group: None,
                    audit_edit_session: None,
                },
            )
            .await?;

            let _ = state.events.send(BroadcastEvent {
                board_id,
                event: BoardEvent::ColumnUpdated {
                    column: api_col.clone(),
                },
            });
            Ok(Json(api_col))
        }
        None => Err(ApiError::NotFound),
    }
}

#[tracing::instrument(skip_all, fields(bored.column.id = %col_id))]
pub async fn delete_column(
    State(state): State<AppState>,
    Path(col_id): Path<String>,
    claims: Extension<Claims>,
) -> Result<StatusCode, ApiError> {
    let existing = load_column(&state, &col_id, "columns.delete_column").await?;

    // Destructure early to capture the board ID for the SSE event.
    let existing = match existing {
        Some(c) => c,
        None => return Err(ApiError::NotFound),
    };
    let board_id = existing.board_id.clone();
    let batch = audit::new_batch_group();

    let cards: Vec<DbCard> =
        sqlx::query_as("SELECT * FROM cards WHERE column_id = $1 ORDER BY position, id")
            .bind(&col_id)
            .fetch_all(&state.db)
            .traced(DbQuery::new(
                "columns.delete_column",
                DbOperation::Select,
                "cards",
            ))
            .await?;

    for card in cards {
        let entity_id = card.id.clone();
        // Links go first so their delete rows sit under the same batch as the
        // card they belonged to.
        crate::routes::links::cascade_delete_card_links(
            &state,
            &claims,
            &board_id,
            &entity_id,
            Some(&batch),
        )
        .await?;
        let snapshot_before = serde_json::to_value(card.clone().into_api())?;
        audit::record_and_broadcast(
            &state.db,
            &state.events,
            audit::AuditRecord {
                claims: &claims,
                board_id: board_id.clone(),
                entity_type: "card",
                entity_id: &entity_id,
                action: "delete",
                snapshot_before: Some(snapshot_before),
                snapshot_after: None,
                restored_from: None,
                batch_group: Some(batch.clone()),
                audit_edit_session: None,
            },
        )
        .await?;

        sqlx::query("DELETE FROM cards WHERE id = $1")
            .bind(&entity_id)
            .execute(&state.db)
            .traced(DbQuery::new(
                "columns.delete_column",
                DbOperation::Delete,
                "cards",
            ))
            .await?;
    }

    let col_snap = serde_json::to_value(existing.clone().into_api())?;
    audit::record_and_broadcast(
        &state.db,
        &state.events,
        audit::AuditRecord {
            claims: &claims,
            board_id: board_id.clone(),
            entity_type: "column",
            entity_id: &col_id,
            action: "delete",
            snapshot_before: Some(col_snap),
            snapshot_after: None,
            restored_from: None,
            batch_group: Some(batch),
            audit_edit_session: None,
        },
    )
    .await?;

    sqlx::query("DELETE FROM columns WHERE id = $1")
        .bind(&col_id)
        .execute(&state.db)
        .traced(DbQuery::new(
            "columns.delete_column",
            DbOperation::Delete,
            "columns",
        ))
        .await?;

    let _ = state.events.send(BroadcastEvent {
        board_id,
        event: BoardEvent::ColumnDeleted { column_id: col_id },
    });

    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/boards/:slug/columns/reorder`
///
/// Accepts a complete ordered list of column IDs (`{ order: ["id1","id2",…] }`)
/// and assigns `position = index` to each. This is a bulk operation — the client
/// sends the full desired order and the server rewrites every position in one
/// pass. Column IDs not present in the list are skipped (their position is
/// unchanged), so the caller must include every column to guarantee a consistent
/// result. The board is looked up by slug; the ULID is used for the DB query
/// guard that prevents cross-board IDOR writes.
#[tracing::instrument(skip_all, fields(bored.board.slug = %board_slug))]
pub async fn reorder_columns(
    State(state): State<AppState>,
    Path(board_slug): Path<String>,
    claims: Extension<Claims>,
    Json(payload): Json<shared::ColumnsReorderRequest>,
) -> Result<Json<Vec<shared::Column>>, ApiError> {
    let board = match find_board_by_slug(&state.db, &board_slug).await? {
        Some(b) => b,
        None => return Err(ApiError::NotFound),
    };
    let board_ulid = board.id;

    let editor = editor_sub(&claims);
    let batch = audit::new_batch_group();

    // Update each column's position to its index in the supplied order.
    // The WHERE clause scopes the update to this board, so a foreign column ID
    // silently no-ops (matches zero rows) rather than mutating another board's
    // state — preventing cross-board IDOR writes.
    for (index, col_id) in payload.order.iter().enumerate() {
        let before = load_column(&state, col_id, "columns.reorder_columns").await?;
        let Some(col_before) = before else {
            continue;
        };
        if col_before.board_id != board_ulid {
            continue;
        }
        if col_before.position == index as i32 {
            continue;
        }

        let snapshot_before = serde_json::to_value(col_before.into_api())?;

        // Single RETURNING so we never apply an UPDATE without a confirmed row for audit,
        // and never skip auditing after a successful position write (second SELECT could yield None).
        let updated: Vec<DbColumn> = sqlx::query_as(
            "UPDATE columns SET position = $2, last_edited_by = $3 \
             WHERE id = $1 AND board_id = $4 RETURNING *",
        )
        .bind(col_id)
        .bind(index as i32)
        .bind(&editor)
        .bind(&board_ulid)
        .fetch_all(&state.db)
        .traced(DbQuery::new(
            "columns.reorder_columns",
            DbOperation::Update,
            "columns",
        ))
        .await?;
        let mut it = updated.into_iter();
        let Some(col_after) = it.next() else {
            return Err(ApiError::internal("reorder update matched no column"));
        };
        if it.next().is_some() {
            return Err(ApiError::internal(
                "reorder update matched more than one column",
            ));
        }
        let snapshot_after = serde_json::to_value(col_after.into_api())?;

        audit::record_and_broadcast(
            &state.db,
            &state.events,
            audit::AuditRecord {
                claims: &claims,
                board_id: board_ulid.clone(),
                entity_type: "column",
                entity_id: col_id.as_str(),
                action: "move",
                snapshot_before: Some(snapshot_before),
                snapshot_after: Some(snapshot_after),
                restored_from: None,
                batch_group: Some(batch.clone()),
                audit_edit_session: None,
            },
        )
        .await?;
    }

    // Re-fetch the full ordered list so we can return it and broadcast it.
    let columns = board_columns(&state, &board_ulid, "columns.reorder_columns").await?;

    let api_cols: Vec<shared::Column> = columns.into_iter().map(DbColumn::into_api).collect();

    let _ = state.events.send(BroadcastEvent {
        board_id: board_ulid,
        event: BoardEvent::ColumnsReordered {
            columns: api_cols.clone(),
        },
    });

    Ok(Json(api_cols))
}
