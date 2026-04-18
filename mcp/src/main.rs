// Entry point for the bored MCP server.
//
// Transport: stdio — Claude Code spawns this binary and communicates
// over its stdin/stdout. All tracing output goes to stderr so it
// doesn't corrupt the MCP JSON-RPC stream on stdout.
//
// Configuration (env vars):
//   BORED_API_URL  — base URL of the bored backend, e.g. http://localhost:3000
//                    Defaults to http://localhost:3000 when unset.

mod server;

use anyhow::Result;
// ServiceExt provides the `.serve()` method on our handler.
use rmcp::ServiceExt;
// stdio() returns a (reader, writer) pair wired to the process stdin/stdout.
use rmcp::transport::io::stdio;
use server::BoredMcp;

#[tokio::main]
async fn main() -> Result<()> {
    // Direct all tracing output to stderr — stdout is reserved for the
    // MCP JSON-RPC transport. ANSI colours are disabled because stderr
    // from a subprocess is often captured raw.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let base_url = std::env::var("BORED_API_URL")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());

    tracing::info!(base_url, "bored MCP server starting");

    // `.serve(stdio())` hands the handler to the MCP runtime and begins
    // reading requests from stdin / writing responses to stdout.
    let service = match BoredMcp::new(base_url).serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("MCP serve error: {e:?}");
            return Err(anyhow::anyhow!("serve failed: {e:?}"));
        }
    };

    // Block until the client disconnects (Claude Code closes the pipe).
    service.waiting().await?;

    tracing::info!("bored MCP server shut down");
    Ok(())
}
