//! Application entry layer: CLI parsing, configuration loading, the daemon,
//! logging, and the MCP server.

pub mod cli;
pub mod config;
pub mod daemon;
pub mod input_error;
pub mod logging;
pub mod mcp;
pub mod xdg;

use clap::Parser;

/// Parse CLI arguments, initialize logging, and dispatch to the selected
/// command.
pub async fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let _log_guard = logging::init()?;
    cli::dispatch(cli).await
}
