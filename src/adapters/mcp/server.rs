//! Stdio MCP server boot for hallouminate. The transport owns stdin/stdout;
//! every other write (logs, errors, readiness announcements) must go to
//! stderr or we corrupt the JSON-RPC stream.

use anyhow::Context;
use rmcp::ServiceExt;
use rmcp::transport::stdio;

use super::tools::HallouminateTools;

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn serve_stdio() -> anyhow::Result<()> {
    eprintln!("hallouminate {SERVER_VERSION} MCP server listening on stdio");
    // MCP servers are long-lived processes whose CWD is set by whatever
    // spawned them (Claude Code, an editor, a shell). Capture it once as a
    // fallback; tool handlers prefer MCP roots when the client exposes them.
    let cwd = std::env::current_dir().context("capturing MCP server cwd at startup")?;
    let server = HallouminateTools::new(cwd);
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}
