// MCP tool implementations for the bored board app.
//
// Each tool maps to one or more bored REST API calls made via reqwest.
// Tools return `Result<CallToolResult, McpError>`:
//   - Success → `CallToolResult::success(vec![Content::text("...")])`
//   - Failure → `Err(McpError)` built with `mcp_err()`
//
// The `#[tool_router(server_handler)]` macro generates:
//   - A `ToolRouter<BoredMcp>` that dispatches incoming `tools/call`
//     requests to the correct async fn.
//   - A blanket `ServerHandler` impl so the struct can be passed to
//     `.serve()` directly without a separate impl block.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use rmcp::{
    // `Parameters<T>` is a newtype wrapper used by the tool macro to
    // deserialise the incoming JSON params into a typed struct.
    handler::server::wrapper::Parameters,
    // `CallToolResult`, `Content`, `Implementation`, and `ServerInfo` are MCP
    // protocol types; `ServerCapabilities` configures what the server exposes.
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars,
    tool,
    tool_handler,
    tool_router,
    // `ErrorData` is the MCP error envelope; we alias it for brevity.
    ErrorData as McpError,
    // `ServerHandler` is the trait we implement to wire the server into the
    // MCP runtime; `tool_handler` is the macro that fills in the boilerplate.
    ServerHandler,
};
use serde::Deserialize;
use tokio::sync::Mutex;

// ── Parameter structs ─────────────────────────────────────────────────────────
// Each struct is the typed parameter set for one tool.
// `JsonSchema` lets rmcp auto-generate the JSON Schema that Claude sees
// in the tool list, so it knows which fields are required and their types.
// The doc-comments on fields become the field descriptions in that schema.

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateBoardParams {
    /// The display name for the new board.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BoardIdParams {
    /// The ID of the board (returned by list_boards or create_board).
    pub board_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateColumnParams {
    /// The ID of the board to add the column to.
    pub board_id: String,
    /// The display name for the new column.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ColumnIdParams {
    /// The ID of the column (returned by list_columns or create_column).
    pub column_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateCardParams {
    /// The ID of the column to add the card to.
    pub column_id: String,
    /// The markdown body of the card.
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CardIdParams {
    /// The ID of the card (returned by list_cards or create_card).
    pub card_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateCardParams {
    /// The ID of the card to update.
    pub card_id: String,
    /// The new markdown body.
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveCardParams {
    /// The ID of the card to move.
    pub card_id: String,
    /// The destination column ID.
    pub column_id: String,
    /// 0-based position within the destination column (0 = top).
    pub position: i32,
}

// ── Auth token manager ────────────────────────────────────────────────────────
//
// When the bored backend is configured with OIDC, every API call must carry an
// `Authorization: Bearer <jwt>` header. We obtain that JWT via the OAuth2
// `client_credentials` grant against Authentik's token endpoint, cache it in
// memory, and refresh proactively shortly before it expires.
//
// We DO NOT verify the token's signature here — the bored backend is the
// authority on validity. We only peek at the `exp` claim (without verification)
// to know when to refresh.

/// Configuration for the client_credentials flow. Read once from env vars.
/// Manual `Debug` impl below redacts `client_secret` so accidental `{:?}`
/// formatting cannot leak the credential to logs or panic messages.
#[derive(Clone)]
pub struct ClientCredentialsConfig {
    /// Full URL of the OIDC token endpoint (e.g.
    /// `https://auth.desync.link/application/o/token/`).
    pub token_url: String,
    /// Client ID issued by Authentik for this MCP service account.
    pub client_id: String,
    /// Client secret. Sensitive — never logged.
    pub client_secret: String,
    /// Optional scope to request. When unset, Authentik returns the default
    /// scope set associated with the provider (which already includes the
    /// per-env access scope).
    pub scope: Option<String>,
}

impl std::fmt::Debug for ClientCredentialsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentialsConfig")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("scope", &self.scope)
            .finish()
    }
}

impl ClientCredentialsConfig {
    /// Read from env vars. Returns `None` if `OIDC_TOKEN_URL` is unset, in
    /// which case the MCP server runs unauthenticated (matches the backend's
    /// auth-disabled mode, used for local hacking against an open backend).
    pub fn from_env() -> Option<Self> {
        let token_url = std::env::var("OIDC_TOKEN_URL").ok()?;
        let client_id = std::env::var("MCP_CLIENT_ID")
            .expect("MCP_CLIENT_ID required when OIDC_TOKEN_URL is set");
        let client_secret = std::env::var("MCP_CLIENT_SECRET")
            .expect("MCP_CLIENT_SECRET required when OIDC_TOKEN_URL is set");
        let scope = std::env::var("MCP_SCOPE").ok();
        Some(Self {
            token_url,
            client_id,
            client_secret,
            scope,
        })
    }
}

/// Cached token plus its expiry deadline (from the JWT's `exp` claim).
/// No `Debug` derive — `access_token` is a live bearer credential and any
/// `{:?}` formatting would leak it.
#[derive(Clone)]
struct TokenState {
    access_token: String,
    /// Refresh ~60s before this. Computed once at fetch time so there's no
    /// system-time call on the hot path.
    refresh_at: Instant,
}

/// Fetches and caches client_credentials tokens.
/// Cheap to share via `Arc<TokenManager>` — internal state is behind a Mutex.
pub struct TokenManager {
    config: ClientCredentialsConfig,
    http: reqwest::Client,
    state: Mutex<Option<TokenState>>,
}

/// Token endpoint response shape. Per RFC 6749 §5.1 the response contains
/// access_token, token_type, expires_in (and may include scope). We don't
/// rely on `expires_in` — we read `exp` from the JWT itself to be robust
/// to clock skew between the IdP and this process.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// JWT payload subset — only the `exp` claim is needed for proactive refresh.
#[derive(Debug, Deserialize)]
struct TokenExp {
    exp: u64,
}

impl TokenManager {
    pub fn new(config: ClientCredentialsConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            state: Mutex::new(None),
        }
    }

    /// Get a current bearer token, refreshing if necessary.
    /// Returns the raw JWT string ready to drop into an Authorization header.
    pub async fn get_token(&self) -> Result<String, String> {
        let mut state = self.state.lock().await;
        if let Some(s) = state.as_ref() {
            if Instant::now() < s.refresh_at {
                return Ok(s.access_token.clone());
            }
        }
        // Need to acquire/refresh. Holding the mutex across the network call
        // serialises concurrent refreshes — fine for an MCP server doing
        // human-paced tool calls; not the right choice for a high-QPS service.
        let new_state = self.fetch().await?;
        let token = new_state.access_token.clone();
        *state = Some(new_state);
        Ok(token)
    }

    /// Make the actual token request and parse the result.
    async fn fetch(&self) -> Result<TokenState, String> {
        let mut form = vec![
            ("grant_type", "client_credentials"),
            ("client_id", self.config.client_id.as_str()),
            ("client_secret", self.config.client_secret.as_str()),
        ];
        if let Some(scope) = &self.config.scope {
            form.push(("scope", scope.as_str()));
        }
        let resp = self
            .http
            .post(&self.config.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("token endpoint {status}: {body}"));
        }
        let body: TokenResponse = resp
            .json()
            .await
            .map_err(|e| format!("token response parse failed: {e}"))?;
        // Peek at exp without verifying — we trust the IdP we just talked to.
        let exp = parse_exp_unverified(&body.access_token)
            .map_err(|e| format!("token has no parseable exp: {e}"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Schedule refresh 60s before exp. If the token is already past exp
        // (broken IdP clock), refresh_at sits at `now`, forcing a refetch on
        // every call until the situation resolves.
        let lifetime = exp.saturating_sub(now);
        let refresh_in = Duration::from_secs(lifetime.saturating_sub(60));
        let refresh_at = Instant::now() + refresh_in;
        Ok(TokenState {
            access_token: body.access_token,
            refresh_at,
        })
    }
}

/// Decode the JWT payload without signature verification and pull `exp`.
/// Used only for refresh scheduling — never for trust decisions.
fn parse_exp_unverified(token: &str) -> Result<u64, String> {
    let payload_b64 = token
        .split('.')
        .nth(1)
        .ok_or_else(|| "JWT missing payload segment".to_string())?;
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64.trim_end_matches('='))
        .or_else(|_| {
            // Some IdPs include padding; try the padded engine as a fallback.
            base64::engine::general_purpose::URL_SAFE.decode(payload_b64)
        })
        .map_err(|e| format!("base64 decode: {e}"))?;
    let exp: TokenExp =
        serde_json::from_slice(&payload_bytes).map_err(|e| format!("JSON parse: {e}"))?;
    Ok(exp.exp)
}

// ── Server struct ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BoredMcp {
    // Reuse a single reqwest::Client across all tool calls — it manages
    // a connection pool internally, so sharing it is both correct and efficient.
    client: reqwest::Client,
    base_url: String,
    /// Optional token manager. When `None`, requests go out unauthenticated —
    /// matches the backend's auth-disabled mode for local hacking.
    auth: Option<Arc<TokenManager>>,
}

impl BoredMcp {
    pub fn new(base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            auth: None,
        }
    }

    /// Builder-style attach a token manager. Called once at startup if
    /// `OIDC_TOKEN_URL` is present in the environment.
    pub fn with_auth(mut self, manager: Arc<TokenManager>) -> Self {
        self.auth = Some(manager);
        self
    }

    // Build a full API URL from a path fragment, e.g. "boards/abc/columns".
    fn api(&self, path: &str) -> String {
        format!("{}/api/{}", self.base_url, path)
    }

    /// Send a `RequestBuilder` with the current bearer token attached if
    /// auth is configured. Each tool composes its request via
    /// `self.client.<method>()` then pipes it through this single helper —
    /// keeping the auth concern in one place.
    async fn send(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::Response, McpError> {
        let req = if let Some(auth) = self.auth.as_ref() {
            let token = auth.get_token().await.map_err(mcp_err)?;
            builder.bearer_auth(token)
        } else {
            builder
        };
        req.send().await.map_err(mcp_err)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

// Convert any Display error into an MCP internal-error envelope.
// JSON-RPC internal error code is -32603.
fn mcp_err(e: impl std::fmt::Display) -> McpError {
    McpError::new(rmcp::model::ErrorCode(-32603), e.to_string(), None)
}

// Assert that the HTTP response is 2xx, otherwise surface the status and body
// as an MCP error so Claude sees a useful message instead of a silent failure.
async fn require_ok(resp: reqwest::Response) -> Result<reqwest::Response, McpError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Err(mcp_err(format!("API returned {status}: {body}")))
}

// Deserialise the response body to a pretty-printed JSON string for Claude.
async fn json_text(resp: reqwest::Response) -> Result<CallToolResult, McpError> {
    let val: serde_json::Value = resp.json().await.map_err(mcp_err)?;
    let text = serde_json::to_string_pretty(&val).map_err(mcp_err)?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

// ── Tool implementations ───────────────────────────────────────────────────────
//
// `#[tool_router]` on the impl block:
//   1. Collects every method annotated with `#[tool(...)]`.
//   2. Generates a `ToolRouter<BoredMcp>` field and dispatches incoming
//      `tools/call` requests to the correct async fn.
//
// We then write `impl ServerHandler for BoredMcp` manually (annotated with
// `#[tool_handler]` so rmcp wires the router in) so we can override
// `get_info()` and return a meaningful server name instead of the default
// "rmcp" that `from_build_env()` produces inside the library crate.
//
// Each `#[tool(description = "...")]` method must take `&self` plus
// optional `Parameters<T>` and return `Result<CallToolResult, McpError>`.

#[tool_router]
impl BoredMcp {
    // ── Boards ────────────────────────────────────────────────────────────────

    #[tool(
        description = "List all boards. Returns a JSON array of board objects with id, name, created_at, updated_at."
    )]
    async fn list_boards(&self) -> Result<CallToolResult, McpError> {
        let resp = self.send(self.client.get(self.api("boards"))).await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(
        description = "Create a new board with the given name. Returns the created board object including its id."
    )]
    async fn create_board(
        &self,
        Parameters(CreateBoardParams { name }): Parameters<CreateBoardParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(
                self.client
                    .post(self.api("boards"))
                    .json(&serde_json::json!({ "name": name })),
            )
            .await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(description = "Delete a board and all its columns and cards permanently.")]
    async fn delete_board(
        &self,
        Parameters(BoardIdParams { board_id }): Parameters<BoardIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(self.client.delete(self.api(&format!("boards/{board_id}"))))
            .await?;
        require_ok(resp).await?;
        Ok(CallToolResult::success(vec![Content::text(
            "Board deleted.",
        )]))
    }

    // ── Columns ───────────────────────────────────────────────────────────────

    #[tool(
        description = "List all columns in a board, ordered by position. Returns a JSON array with id, board_id, name, position."
    )]
    async fn list_columns(
        &self,
        Parameters(BoardIdParams { board_id }): Parameters<BoardIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(
                self.client
                    .get(self.api(&format!("boards/{board_id}/columns"))),
            )
            .await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(
        description = "Create a new column at the end of a board. Returns the created column object including its id."
    )]
    async fn create_column(
        &self,
        Parameters(CreateColumnParams { board_id, name }): Parameters<CreateColumnParams>,
    ) -> Result<CallToolResult, McpError> {
        // Use a large position so the new column always appends to the end
        // rather than requiring the caller to track current column count.
        let resp = self
            .send(
                self.client
                    .post(self.api(&format!("boards/{board_id}/columns")))
                    .json(&serde_json::json!({ "name": name, "position": 99999 })),
            )
            .await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(description = "Delete a column and all its cards permanently.")]
    async fn delete_column(
        &self,
        Parameters(ColumnIdParams { column_id }): Parameters<ColumnIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(
                self.client
                    .delete(self.api(&format!("columns/{column_id}"))),
            )
            .await?;
        require_ok(resp).await?;
        Ok(CallToolResult::success(vec![Content::text(
            "Column deleted.",
        )]))
    }

    // ── Cards ─────────────────────────────────────────────────────────────────

    #[tool(
        description = "List all cards in a column, ordered by position. Returns a JSON array with id, column_id, body, position."
    )]
    async fn list_cards(
        &self,
        Parameters(ColumnIdParams { column_id }): Parameters<ColumnIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(
                self.client
                    .get(self.api(&format!("columns/{column_id}/cards"))),
            )
            .await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(
        description = "Get the full details of a single card by id, including its complete markdown body."
    )]
    async fn get_card(
        &self,
        Parameters(CardIdParams { card_id }): Parameters<CardIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(self.client.get(self.api(&format!("cards/{card_id}"))))
            .await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(
        description = "Create a new card with a markdown body in a column. Returns the created card object including its id."
    )]
    async fn create_card(
        &self,
        Parameters(CreateCardParams { column_id, body }): Parameters<CreateCardParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(
                self.client
                    .post(self.api(&format!("columns/{column_id}/cards")))
                    .json(&serde_json::json!({ "body": body })),
            )
            .await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(
        description = "Update the markdown body of an existing card. Returns the updated card object."
    )]
    async fn update_card(
        &self,
        Parameters(UpdateCardParams { card_id, body }): Parameters<UpdateCardParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(
                self.client
                    .put(self.api(&format!("cards/{card_id}")))
                    .json(&serde_json::json!({ "body": body })),
            )
            .await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(
        description = "Move a card to a different column and/or position. Position is 0-based (0 = top of column). Returns the updated card."
    )]
    async fn move_card(
        &self,
        Parameters(MoveCardParams {
            card_id,
            column_id,
            position,
        }): Parameters<MoveCardParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(
                self.client
                    .post(self.api(&format!("cards/{card_id}/move")))
                    .json(&serde_json::json!({ "column_id": column_id, "position": position })),
            )
            .await?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(description = "Delete a card permanently.")]
    async fn delete_card(
        &self,
        Parameters(CardIdParams { card_id }): Parameters<CardIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .send(self.client.delete(self.api(&format!("cards/{card_id}"))))
            .await?;
        require_ok(resp).await?;
        Ok(CallToolResult::success(vec![Content::text(
            "Card deleted.",
        )]))
    }
}

// ── ServerHandler impl ────────────────────────────────────────────────────────
//
// `#[tool_handler]` wires the ToolRouter generated above into the MCP runtime.
// We override `get_info()` so the server reports its name as "bored" rather
// than the library default "rmcp" (which comes from `CARGO_CRATE_NAME` inside
// the rmcp crate, not ours).

#[tool_handler]
impl ServerHandler for BoredMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("bored", env!("CARGO_PKG_VERSION")))
    }
}
