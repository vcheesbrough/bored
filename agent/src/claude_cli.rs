// Inference via the Claude Code CLI (`claude --print`).
//
// Rather than calling api.anthropic.com directly (which uses a separate,
// tightly-throttled quota for OAuth tokens), we shell out to the `claude`
// binary. It routes inference through the same path as Claude Code itself,
// drawing from the user's full Claude Max subscription budget.
//
// No tool use needed here — we just give Claude an explicit output contract
// in the system prompt and treat stdout as the updated card body.

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// System prompt passed via `--system-prompt`. Instructs Claude to output
/// *only* the raw Markdown body so we can write stdout directly back to bored.
const SYSTEM_PROMPT: &str = "\
You are a project management assistant for a kanban board. \
Output ONLY the complete updated card body in Markdown — \
the original content completely unchanged, \
with a single brief blockquote (using > Markdown syntax) appended at the end \
that notes the column transition and any concise observation about readiness. \
No preamble, no explanation, no code fences — just the raw Markdown.";

/// Wraps the `claude` CLI binary for one-shot inference calls.
pub struct ClaudeClient {
    /// Absolute path to the `claude` binary.
    /// Defaults to "claude" (resolved via PATH).
    bin: String,
}

impl ClaudeClient {
    pub fn new() -> Self {
        ClaudeClient {
            bin: "claude".to_string(),
        }
    }

    /// Append a transition blockquote to `card_body` and return the full
    /// updated body. Calls `claude --print` with the card context piped to
    /// stdin; stdout is the new body.
    pub async fn append_transition_note(
        &self,
        card_number: u32,
        card_body: &str,
        from_column: &str,
        to_column: &str,
    ) -> Result<String> {
        // Build the user-facing message that gives Claude full context.
        // The system prompt handles the output format contract.
        let user_message = format!(
            "Card #{number} was just moved from \"{from}\" to \"{to}\".\n\n\
             Current card body:\n\n\
             {body}",
            number = card_number,
            from = from_column,
            to = to_column,
            body = card_body,
        );

        // Spawn `claude --print` and pipe the message through stdin.
        // Using stdin instead of a positional argument avoids any OS arg-length
        // concerns and keeps special characters safe without shell escaping.
        let mut child = Command::new(&self.bin)
            .args(["--print", "--system-prompt", SYSTEM_PROMPT])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("Failed to spawn claude binary — is it installed and on PATH?")?;

        // Write the message and close stdin so the process knows input is done.
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(user_message.as_bytes())
                .await
                .context("Failed to write prompt to claude stdin")?;
            // Drop closes the pipe, signalling EOF to the process.
        }

        let output = child
            .wait_with_output()
            .await
            .context("Failed to wait for claude process")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "claude exited with status {:?}: {}",
                output.status.code(),
                stderr.trim()
            );
        }

        let body = String::from_utf8(output.stdout)
            .context("claude stdout was not valid UTF-8")?
            .trim()
            .to_string();

        if body.is_empty() {
            bail!("claude returned an empty response");
        }

        Ok(body)
    }
}
