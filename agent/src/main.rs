// agent-poc — a standalone binary that watches the bored SSE stream and
// automatically annotates cards when they are moved between columns.
//
// High-level flow:
//   1. Read config from environment variables.
//   2. Fetch all columns for the target board into an in-memory cache so we can
//      resolve column IDs to human-readable names quickly.
//   3. Open the SSE stream for the board and process events forever, reconnecting
//      after any transient network failure.
//   4. On each `card_moved` event: look up the from/to column names, shell out
//      to `claude --print` for the annotation, and write the result back via
//      PUT /api/cards/:id.
//
// Inference is routed through the Claude Code CLI rather than the raw Anthropic
// API so it draws from the user's Claude Max subscription quota instead of a
// separate, tightly-throttled OAuth API bucket.

mod bored;
mod claude_cli;

use anyhow::{Context, Result};
use tracing::info;

/// All runtime configuration comes from environment variables so the binary
/// can run in any environment without recompilation.
pub struct Config {
    /// Base URL of the bored API, e.g. "https://bored.desync.link" (no trailing slash).
    pub bored_api_url: String,
    /// The bored board ID the agent should watch. Only events for this board
    /// are processed; events for other boards are ignored by the SSE filter.
    pub board_id: String,
    /// Optional Bearer token sent as `Authorization: Bearer <token>` on every
    /// bored API request. Matches the BORED_API_TOKEN convention used by the
    /// MCP server. Not required if the bored instance has no auth enabled.
    pub api_token: Option<String>,
}

impl Config {
    fn from_env() -> Result<Self> {
        Ok(Config {
            bored_api_url: std::env::var("BORED_API_URL")
                .context("BORED_API_URL must be set (e.g. https://bored.desync.link)")?,
            board_id: std::env::var("BOARD_ID")
                .context("BOARD_ID must be set to the bored board ID to watch")?,
            api_token: std::env::var("BORED_API_TOKEN").ok(),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialise structured logging. RUST_LOG controls the filter level;
    // defaults to "info" if unset. Logs go to stderr so they don't interfere
    // with any potential stdout pipelines.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = Config::from_env()?;

    // A single reqwest::Client manages an internal connection pool and can be
    // shared freely across async tasks. We create one here and pass references
    // down to avoid re-creating it on every reconnect.
    //
    // If BORED_API_TOKEN is set we attach it as a default Authorization header
    // so every request (column fetch, SSE stream, card update) is authenticated
    // without threading the token through each call site.
    let mut default_headers = reqwest::header::HeaderMap::new();
    if let Some(token) = &config.api_token {
        let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token))
            .context("BORED_API_TOKEN contains invalid header characters")?;
        default_headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    let http_client = reqwest::Client::builder()
        .default_headers(default_headers)
        .build()
        .context("Failed to build HTTP client")?;
    let claude = claude_cli::ClaudeClient::new();

    info!(board_id = %config.board_id, "agent-poc starting");

    // Prime the column cache before entering the event loop so the first
    // CardMoved event can resolve column names immediately.
    let mut column_cache = bored::ColumnCache::new();
    column_cache
        .refresh(&http_client, &config)
        .await
        .context("Failed to load initial columns")?;

    info!(columns = column_cache.len(), "Column cache ready");

    // SSE reconnect loop. If the connection drops for any reason (network
    // blip, server restart, etc.) we wait briefly and try again. The agent is
    // stateless between connections — the column cache is refreshed on every
    // reconnect so it stays current.
    loop {
        match run_sse_loop(&config, &http_client, &claude, &mut column_cache).await {
            Ok(()) => {
                info!("SSE stream ended, reconnecting in 2 seconds");
            }
            Err(e) => {
                tracing::warn!(error = %e, "SSE stream error, reconnecting in 2 seconds");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Refresh columns on reconnect so we don't serve stale names for any
        // columns that were created or renamed while we were disconnected.
        if let Err(e) = column_cache.refresh(&http_client, &config).await {
            tracing::warn!(error = %e, "Column cache refresh failed (will retry on next reconnect)");
        }
    }
}

/// Connect to the bored SSE stream and process events until the connection
/// closes or an unrecoverable error occurs.
///
/// SSE wire format (RFC 8895):
///   - Lines starting with `data:` carry the event JSON payload.
///   - Lines starting with `:` are comments (keep-alive pings) — ignored.
///   - An empty line marks the end of one logical event.
async fn run_sse_loop(
    config: &Config,
    http_client: &reqwest::Client,
    claude: &claude_cli::ClaudeClient,
    column_cache: &mut bored::ColumnCache,
) -> Result<()> {
    use futures_util::StreamExt;

    let url = format!(
        "{}/api/events?board_id={}",
        config.bored_api_url, config.board_id
    );

    info!(url = %url, "Connecting to SSE stream");

    let response = http_client
        .get(&url)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .context("Failed to connect to SSE stream")?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("SSE endpoint returned {}", status);
    }

    info!("Connected to SSE stream");

    let mut pending_data: Option<String> = None;
    let mut line_buf = String::new();
    let mut byte_stream = response.bytes_stream();

    while let Some(chunk_result) = byte_stream.next().await {
        let chunk = chunk_result.context("Error reading SSE chunk")?;

        let text = String::from_utf8_lossy(&chunk);
        line_buf.push_str(&text);

        while let Some(newline_pos) = line_buf.find('\n') {
            let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
            line_buf = line_buf[newline_pos + 1..].to_string();

            if line.is_empty() {
                if let Some(data) = pending_data.take() {
                    handle_event(config, http_client, claude, column_cache, &data).await;
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                // RFC 8895 permits both `data:value` and `data: value`; strip
                // the optional leading space so both forms are handled.
                let data = data.strip_prefix(' ').unwrap_or(data);
                match pending_data.as_mut() {
                    Some(existing) => {
                        existing.push('\n');
                        existing.push_str(data);
                    }
                    None => pending_data = Some(data.to_string()),
                }
            }
        }
    }

    Ok(())
}

/// Parse one SSE data payload and, if it is a `card_moved` event, invoke the
/// annotation pipeline.
async fn handle_event(
    config: &Config,
    http_client: &reqwest::Client,
    claude: &claude_cli::ClaudeClient,
    column_cache: &mut bored::ColumnCache,
    data: &str,
) {
    let event: bored::BoardEvent = match serde_json::from_str(data) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, raw = %data, "Failed to parse SSE event");
            return;
        }
    };

    if let bored::BoardEvent::CardMoved {
        card,
        from_column_id,
    } = event
    {
        info!(
            card_id = %card.id,
            card_number = card.number,
            from = %from_column_id,
            to = %card.column_id,
            "CardMoved event received"
        );

        if let Err(e) = annotate_card(
            config,
            http_client,
            claude,
            column_cache,
            card,
            &from_column_id,
        )
        .await
        {
            tracing::error!(error = %e, "Failed to annotate card");
        }
    }
}

/// Core annotation pipeline:
///   1. Resolve from/to column names (refresh cache on miss).
///   2. Shell out to `claude --print` for the updated card body.
///   3. Write the result back via PUT /api/cards/:id.
async fn annotate_card(
    config: &Config,
    http_client: &reqwest::Client,
    claude: &claude_cli::ClaudeClient,
    column_cache: &mut bored::ColumnCache,
    card: shared::Card,
    from_column_id: &str,
) -> Result<()> {
    let from_name = column_cache
        .resolve(http_client, config, from_column_id)
        .await
        .unwrap_or_else(|| from_column_id.to_string());

    let to_name = column_cache
        .resolve(http_client, config, &card.column_id)
        .await
        .unwrap_or_else(|| card.column_id.clone());

    info!(
        card_number = card.number,
        from_column = %from_name,
        to_column = %to_name,
        "Calling claude CLI to annotate card"
    );

    let new_body = claude
        .append_transition_note(card.number, &card.body, &from_name, &to_name)
        .await?;

    info!(
        card_number = card.number,
        new_body_len = new_body.len(),
        "Claude returned updated body, writing to bored API"
    );

    bored::update_card(http_client, config, &card.id, &new_body).await?;

    info!(card_number = card.number, "Card annotated successfully");
    Ok(())
}
