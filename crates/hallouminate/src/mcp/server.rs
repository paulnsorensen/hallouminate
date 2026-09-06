//! Stdio MCP server boot for hallouminate. The transport owns stdin/stdout;
//! every other write (logs, errors, readiness announcements) must go to
//! stderr or we corrupt the JSON-RPC stream.

use rmcp::ServiceExt;
use rmcp::transport::stdio;

use super::tools::HallouminateTools;

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn serve_stdio() -> anyhow::Result<()> {
    eprintln!("hallouminate {SERVER_VERSION} MCP server listening on stdio");
    // Every tool call carries its own required `cwd`; the server holds no
    // startup-captured directory to fall back to.
    let server = HallouminateTools::new();
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}
