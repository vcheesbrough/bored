// bored API client for the agent.
//
// Responsibilities:
//   - Deserialize the subset of SSE events the agent cares about.
//   - Maintain an in-memory cache of column IDs → names so we don't hit the
//     API on every CardMoved event.
//   - Provide a thin `update_card` helper used after Claude returns the new body.

use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::Config;

// ── SSE event types ───────────────────────────────────────────────────────────

/// The subset of bored board events that this agent needs to deserialize.
///
/// The `#[serde(tag = "type", rename_all = "snake_case")]` configuration
/// mirrors the server-side `BoardEvent` encoding exactly: the JSON field
/// `"type"` acts as the discriminator, and the variant names are lowercased
/// with underscores (e.g. `CardMoved` → `"card_moved"`).
///
/// Any variant not listed here falls through to `Other` so the agent can
/// ignore board-level events, column events, etc. without failing to parse.
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BoardEvent {
    /// A card was moved to a different column or position.
    ///
    /// `card.column_id` is the *destination* column; `from_column_id` is the
    /// *source* column the card was moved away from.
    CardMoved {
        card: shared::Card,
        from_column_id: String,
    },
    /// Catch-all for every other event variant (CardCreated, CardUpdated,
    /// ColumnCreated, BoardUpdated, …). `#[serde(other)]` tells serde to use
    /// this arm when the `"type"` field doesn't match any named variant.
    #[serde(other)]
    Other,
}

// ── Column cache ──────────────────────────────────────────────────────────────

/// In-memory map from column ID → column name.
///
/// The cache is populated on startup and refreshed on every SSE reconnect.
/// An explicit refresh is also triggered at runtime when we encounter a column
/// ID that isn't in the map (handles columns created after the agent started).
pub struct ColumnCache {
    /// The inner map. Keys are bored column IDs (ULID strings); values are the
    /// human-readable column names displayed in the UI.
    map: HashMap<String, String>,
}

impl ColumnCache {
    /// Create an empty cache. Call `refresh()` before first use.
    pub fn new() -> Self {
        ColumnCache {
            map: HashMap::new(),
        }
    }

    /// How many columns are currently cached.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Fetch all columns for the configured board and rebuild the cache.
    ///
    /// Uses `GET /api/boards/:board_id/columns` — there is no single-column
    /// lookup endpoint, so we always load the full list and replace the map.
    pub async fn refresh(&mut self, client: &reqwest::Client, config: &Config) -> Result<()> {
        let url = format!(
            "{}/api/boards/{}/columns",
            config.bored_api_url, config.board_id
        );

        let columns: Vec<shared::Column> = client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch columns")?
            .error_for_status()
            .context("Server returned error for columns request")?
            .json()
            .await
            .context("Failed to deserialize columns response")?;

        // Replace the entire map — this is safe because the agent is single-
        // threaded and doesn't share the cache across tasks.
        self.map = columns.into_iter().map(|col| (col.id, col.name)).collect();

        Ok(())
    }

    /// Resolve a column ID to its name.
    ///
    /// If the ID is not in the cache (new column created after the agent
    /// started), we refresh the entire cache once and try again. Returns
    /// `None` only if the column genuinely doesn't exist in the board.
    pub async fn resolve(
        &mut self,
        client: &reqwest::Client,
        config: &Config,
        column_id: &str,
    ) -> Option<String> {
        // Fast path: ID already cached.
        if let Some(name) = self.map.get(column_id) {
            return Some(name.clone());
        }

        // Slow path: refresh the cache and try again.
        tracing::debug!(column_id = %column_id, "Column ID not in cache, refreshing");
        if let Err(e) = self.refresh(client, config).await {
            tracing::warn!(error = %e, "Column cache refresh failed during resolve");
        }

        self.map.get(column_id).cloned()
    }
}

// ── Card update ───────────────────────────────────────────────────────────────

/// Update the body of a card via `PUT /api/cards/:id`.
///
/// Only the `body` field is sent — `position` and `column_id` are left `None`
/// so the server doesn't move or reorder the card as a side effect.
pub async fn update_card(
    client: &reqwest::Client,
    config: &Config,
    card_id: &str,
    new_body: &str,
) -> Result<()> {
    let url = format!("{}/api/cards/{}", config.bored_api_url, card_id);

    // `UpdateCardRequest` lives in the shared crate so the types always match
    // whatever the backend expects.
    let payload = shared::UpdateCardRequest {
        body: Some(new_body.to_string()),
        position: None,
        column_id: None,
    };

    client
        .put(&url)
        .json(&payload)
        .send()
        .await
        .context("Failed to send update_card request")?
        .error_for_status()
        .context("update_card request returned an error status")?;

    Ok(())
}
