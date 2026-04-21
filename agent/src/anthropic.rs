// Thin wrapper around the Anthropic messages API.
//
// This module makes a single-shot call to `POST /v1/messages` with one tool
// defined (`update_card`), forcing Claude to always respond by calling that
// tool. The caller gets back the `body` string from the tool input and can
// write it directly to the bored API.
//
// We use raw reqwest rather than an SDK crate to keep the dependency tree
// small and to show the full API contract explicitly — useful when the user
// is learning how the Anthropic API works under the hood.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Model to use for annotation. claude-sonnet-4-6 gives a good balance of
/// speed and quality for this kind of short-form writing task.
const MODEL: &str = "claude-sonnet-4-6";

/// System prompt that sets the agent's role and writing constraints.
/// Kept as a constant so it's easy to tune without hunting through the code.
const SYSTEM_PROMPT: &str = "\
You are a project management assistant for a kanban board. \
A card has been moved between columns. \
Review the card body and append a single brief blockquote \
(using Markdown > syntax) that notes the column transition \
and any concise observation about the card's current state or readiness. \
Do not rewrite, edit, or remove any existing content — only append. \
Return the complete updated card body via the update_card tool.";

// ── Request types ─────────────────────────────────────────────────────────────

/// The top-level request body for `POST /v1/messages`.
#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    /// Upper bound on generated tokens. 2048 is generous for a single appended
    /// blockquote but keeps us well within the API limits.
    max_tokens: u32,
    system: &'a str,
    messages: Vec<Message<'a>>,
    tools: Vec<ToolDefinition>,
    /// `{"type":"tool","name":"update_card"}` forces Claude to call exactly
    /// that tool rather than choosing to respond with text.
    tool_choice: serde_json::Value,
}

/// A single turn in the conversation.
#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

/// The JSON Schema definition of one tool that Claude can call.
#[derive(Serialize)]
struct ToolDefinition {
    name: String,
    description: String,
    /// Inline JSON Schema object describing the tool's input parameters.
    input_schema: serde_json::Value,
}

// ── Response types ────────────────────────────────────────────────────────────

/// Top-level response from `POST /v1/messages`.
#[derive(Deserialize)]
struct MessagesResponse {
    /// One or more content blocks. With `tool_choice` set to a specific tool
    /// there will always be exactly one `tool_use` block.
    content: Vec<ContentBlock>,
    stop_reason: String,
}

/// A single content block in the response.
///
/// `#[serde(tag = "type", rename_all = "snake_case")]` mirrors the Anthropic
/// API encoding where `"type": "tool_use"` selects this variant.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    /// A text response (won't occur when tool_choice forces a specific tool,
    /// but included for correctness in case the API evolves).
    // The field must be named `text` to match the JSON key, but we never read
    // it — the arm exists only for serde exhaustiveness.
    Text {
        #[allow(dead_code)]
        text: String,
    },
    /// Claude chose to call a tool. `input` is a raw JSON object whose shape
    /// matches the tool's `input_schema`.
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
}

// ── Client ────────────────────────────────────────────────────────────────────

/// Anthropic API client. Holds the reqwest::Client (shared with the rest of
/// the agent to reuse connections) and the API key.
pub struct AnthropicClient {
    /// Shared HTTP connection pool.
    client: reqwest::Client,
    /// Anthropic API key. Sent as the `x-api-key` header on every request.
    api_key: String,
}

impl AnthropicClient {
    pub fn new(client: reqwest::Client, api_key: &str) -> Self {
        AnthropicClient {
            client,
            api_key: api_key.to_string(),
        }
    }

    /// Call Claude with the given user message and extract the `body` string
    /// from the forced `update_card` tool call.
    ///
    /// Returns the complete new card body as a plain string ready to be
    /// written back to the bored API.
    pub async fn call_update_card(&self, user_message: &str) -> Result<String> {
        // Define the single tool Claude is allowed to call.
        let update_card_tool = ToolDefinition {
            name: "update_card".to_string(),
            description: "Update the full body of the kanban card. \
                          Return the entire card body including all original \
                          content plus the newly appended blockquote."
                .to_string(),
            // JSON Schema: the tool accepts a single required string field.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "body": {
                        "type": "string",
                        "description": "The complete updated card body in Markdown."
                    }
                },
                "required": ["body"]
            }),
        };

        let request_body = MessagesRequest {
            model: MODEL,
            max_tokens: 2048,
            system: SYSTEM_PROMPT,
            messages: vec![Message {
                role: "user",
                content: user_message,
            }],
            tools: vec![update_card_tool],
            // Force Claude to call update_card rather than replying with text.
            tool_choice: json!({ "type": "tool", "name": "update_card" }),
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            // The API version header is required on every request.
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", &self.api_key)
            .json(&request_body)
            .send()
            .await
            .context("Failed to send request to Anthropic API")?;

        let status = response.status();

        // Capture the body text before consuming the response so we can
        // include it in the error message if deserialization fails.
        let body_text = response
            .text()
            .await
            .context("Failed to read Anthropic API response body")?;

        if !status.is_success() {
            bail!(
                "Anthropic API returned {} — body: {}",
                status,
                &body_text[..body_text.len().min(500)]
            );
        }

        let parsed: MessagesResponse =
            serde_json::from_str(&body_text).context("Failed to deserialize Anthropic response")?;

        if parsed.stop_reason != "tool_use" {
            bail!("Expected stop_reason=tool_use, got: {}", parsed.stop_reason);
        }

        // Find the tool_use block (should always be present given tool_choice).
        for block in parsed.content {
            if let ContentBlock::ToolUse { name, input } = block {
                if name == "update_card" {
                    // Extract the `body` string from the tool input JSON.
                    let body = input["body"]
                        .as_str()
                        .context("update_card tool input missing 'body' string field")?
                        .to_string();
                    return Ok(body);
                }
            }
        }

        bail!("Anthropic response contained no update_card tool_use block")
    }
}
