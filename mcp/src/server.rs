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

// ── Server struct ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BoredMcp {
    // Reuse a single reqwest::Client across all tool calls — it manages
    // a connection pool internally, so sharing it is both correct and efficient.
    client: reqwest::Client,
    base_url: String,
}

impl BoredMcp {
    pub fn new(base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
        }
    }

    // Build a full API URL from a path fragment, e.g. "boards/abc/columns".
    fn api(&self, path: &str) -> String {
        format!("{}/api/{}", self.base_url, path)
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
        let resp = self
            .client
            .get(self.api("boards"))
            .send()
            .await
            .map_err(mcp_err)?;
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
            .client
            .post(self.api("boards"))
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .map_err(mcp_err)?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(description = "Delete a board and all its columns and cards permanently.")]
    async fn delete_board(
        &self,
        Parameters(BoardIdParams { board_id }): Parameters<BoardIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .client
            .delete(self.api(&format!("boards/{board_id}")))
            .send()
            .await
            .map_err(mcp_err)?;
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
            .client
            .get(self.api(&format!("boards/{board_id}/columns")))
            .send()
            .await
            .map_err(mcp_err)?;
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
            .client
            .post(self.api(&format!("boards/{board_id}/columns")))
            .json(&serde_json::json!({ "name": name, "position": 99999 }))
            .send()
            .await
            .map_err(mcp_err)?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(description = "Delete a column and all its cards permanently.")]
    async fn delete_column(
        &self,
        Parameters(ColumnIdParams { column_id }): Parameters<ColumnIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .client
            .delete(self.api(&format!("columns/{column_id}")))
            .send()
            .await
            .map_err(mcp_err)?;
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
            .client
            .get(self.api(&format!("columns/{column_id}/cards")))
            .send()
            .await
            .map_err(mcp_err)?;
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
            .client
            .get(self.api(&format!("cards/{card_id}")))
            .send()
            .await
            .map_err(mcp_err)?;
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
            .client
            .post(self.api(&format!("columns/{column_id}/cards")))
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(mcp_err)?;
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
            .client
            .put(self.api(&format!("cards/{card_id}")))
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(mcp_err)?;
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
            .client
            .post(self.api(&format!("cards/{card_id}/move")))
            .json(&serde_json::json!({ "column_id": column_id, "position": position }))
            .send()
            .await
            .map_err(mcp_err)?;
        json_text(require_ok(resp).await?).await
    }

    #[tool(description = "Delete a card permanently.")]
    async fn delete_card(
        &self,
        Parameters(CardIdParams { card_id }): Parameters<CardIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let resp = self
            .client
            .delete(self.api(&format!("cards/{card_id}")))
            .send()
            .await
            .map_err(mcp_err)?;
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
