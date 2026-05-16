//! Stdio MCP server boot for hallouminate. The transport owns stdin/stdout;
//! every other write (logs, errors, readiness announcements) must go to
//! stderr or we corrupt the JSON-RPC stream.

use rmcp::transport::stdio;
use rmcp::ServiceExt;

use super::tools::HallouminateTools;

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn serve_stdio() -> anyhow::Result<()> {
    eprintln!(
        "hallouminate {SERVER_VERSION} MCP server listening on stdio"
    );
    let server = HallouminateTools::new();
    let running = server.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}
