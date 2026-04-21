// bored-agent-poc — a standalone binary that watches the bored SSE stream and
// automatically annotates cards when they are moved between columns.
//
// High-level flow:
//   1. Read config from environment variables.
//   2. Fetch all columns for the target board into an in-memory cache so we can
//      resolve column IDs to human-readable names quickly.
//   3. Open the SSE stream for the board and process events forever, reconnecting
//      after any transient network failure.
//   4. On each `card_moved` event: look up the from/to column names, call
//      Claude via the Anthropic messages API, and execute the returned
//      `update_card` tool call against the bored REST API.

mod anthropic;
mod bored;

use anyhow::{Context, Result};
use tracing::info;

/// All runtime configuration comes from environment variables so the binary
/// can run in any environment without recompilation.
pub struct Config {
    /// Base URL of the bored API, e.g. "https://bored.desync.link" (no trailing slash).
    pub bored_api_url: String,
    /// Anthropic API key used to authenticate calls to the messages endpoint.
    pub anthropic_api_key: String,
    /// The bored board ID the agent should watch. Only events for this board
    /// are processed; events for other boards are ignored by the SSE filter.
    pub board_id: String,
}

impl Config {
    /// Read configuration from the environment, returning an error if any
    /// required variable is missing or empty.
    fn from_env() -> Result<Self> {
        Ok(Config {
            bored_api_url: std::env::var("BORED_API_URL")
                .context("BORED_API_URL must be set (e.g. https://bored.desync.link)")?,
            anthropic_api_key: std::env::var("ANTHROPIC_API_KEY")
                .context("ANTHROPIC_API_KEY must be set")?,
            board_id: std::env::var("BOARD_ID")
                .context("BOARD_ID must be set to the bored board ID to watch")?,
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
    let http_client = reqwest::Client::new();

    info!(board_id = %config.board_id, "bored-agent-poc starting");

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
        match run_sse_loop(&config, &http_client, &mut column_cache).await {
            Ok(()) => {
                // The server closed the stream cleanly (unusual but handled).
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
///   - Fields other than `data` (id, event, retry) are not used by bored.
async fn run_sse_loop(
    config: &Config,
    http_client: &reqwest::Client,
    column_cache: &mut bored::ColumnCache,
) -> Result<()> {
    use futures_util::StreamExt;

    let url = format!(
        "{}/api/events?board_id={}",
        config.bored_api_url, config.board_id
    );

    info!(url = %url, "Connecting to SSE stream");

    // We keep the connection open for as long as events arrive. reqwest's
    // bytes_stream() gives us a Stream<Item = Result<Bytes>> so we receive
    // data as the server flushes it rather than buffering the entire response.
    let response = http_client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to SSE stream")?;

    // Bail early if the server returned an error status (e.g. 401, 503).
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("SSE endpoint returned {}", status);
    }

    info!("Connected to SSE stream");

    // `pending_data` accumulates the `data:` line(s) for the current logical
    // event. In bored's case each event is a single `data:` line, but the SSE
    // spec allows multi-line data (subsequent `data:` lines are newline-joined).
    let mut pending_data: Option<String> = None;
    // `line_buf` holds bytes that arrived in the current chunk but have not yet
    // been terminated by a `\n`. The next chunk will complete them.
    let mut line_buf = String::new();

    let mut byte_stream = response.bytes_stream();

    while let Some(chunk_result) = byte_stream.next().await {
        let chunk = chunk_result.context("Error reading SSE chunk")?;

        // Decode the bytes as UTF-8. Invalid sequences are replaced with the
        // Unicode replacement character — acceptable for a logging/annotation
        // agent since we'd just log a parse error on the next step.
        let text = String::from_utf8_lossy(&chunk);
        line_buf.push_str(&text);

        // Process every complete line (terminated by \n) in the buffer.
        // We leave any trailing incomplete line in `line_buf` for the next chunk.
        while let Some(newline_pos) = line_buf.find('\n') {
            // Trim the optional carriage-return before the newline (CRLF streams).
            let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
            // Consume this line from the buffer.
            line_buf = line_buf[newline_pos + 1..].to_string();

            if line.is_empty() {
                // An empty line signals the end of a logical SSE event.
                // Dispatch whatever `data:` content we collected.
                if let Some(data) = pending_data.take() {
                    handle_event(config, http_client, column_cache, &data).await;
                }
            } else if let Some(data) = line.strip_prefix("data: ") {
                // Append to pending data, joining multiple `data:` lines with \n.
                match pending_data.as_mut() {
                    Some(existing) => {
                        existing.push('\n');
                        existing.push_str(data);
                    }
                    None => pending_data = Some(data.to_string()),
                }
            }
            // Lines starting with ':' (keep-alive pings) and other field names
            // (event:, id:, retry:) are intentionally ignored.
        }
    }

    Ok(())
}

/// Parse one SSE data payload and, if it is a `card_moved` event, invoke the
/// annotation pipeline. Errors are logged rather than propagated so a single
/// bad event does not kill the connection.
async fn handle_event(
    config: &Config,
    http_client: &reqwest::Client,
    column_cache: &mut bored::ColumnCache,
    data: &str,
) {
    // Deserialize the JSON. Any unknown event types are silently skipped via
    // the `#[serde(other)]` fallback variant in BoardEvent.
    let event: bored::BoardEvent = match serde_json::from_str(data) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, raw = %data, "Failed to parse SSE event");
            return;
        }
    };

    // All other event types (CardCreated, CardUpdated, ColumnCreated, …)
    // are ignored — the agent only reacts to column transitions.
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

        if let Err(e) =
            annotate_card(config, http_client, column_cache, card, &from_column_id).await
        {
            tracing::error!(error = %e, "Failed to annotate card");
        }
    }
}

/// Core annotation pipeline:
///   1. Resolve from/to column names (refresh cache on miss).
///   2. Call Claude with the card body and column context.
///   3. Execute the returned `update_card` tool call.
async fn annotate_card(
    config: &Config,
    http_client: &reqwest::Client,
    column_cache: &mut bored::ColumnCache,
    card: shared::Card,
    from_column_id: &str,
) -> Result<()> {
    // Resolve column names, refreshing the cache if either ID is unknown.
    // This handles columns created after the agent started.
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
        "Calling Claude to annotate card"
    );

    // Build the user-visible message that gives Claude full context.
    let user_message = format!(
        "Card #{number} was just moved from column \"{from}\" to column \"{to}\".\n\n\
         Here is the current card body:\n\n\
         ---\n\
         {body}\n\
         ---\n\n\
         Please append a single brief blockquote (using > Markdown syntax) that \
         notes the column transition and any observation about the card's current \
         state or readiness. Do not rewrite or remove any existing content — only \
         append. Return the complete updated card body via the update_card tool.",
        number = card.number,
        from = from_name,
        to = to_name,
        body = card.body,
    );

    // Call the Anthropic messages API and get back the new body from the tool call.
    let anthropic = anthropic::AnthropicClient::new(http_client.clone(), &config.anthropic_api_key);
    let new_body = anthropic.call_update_card(&user_message).await?;

    info!(
        card_number = card.number,
        new_body_len = new_body.len(),
        "Claude returned updated body, writing to bored API"
    );

    // Persist the annotated body via PUT /api/cards/:id.
    bored::update_card(http_client, config, &card.id, &new_body).await?;

    info!(card_number = card.number, "Card annotated successfully");
    Ok(())
}
