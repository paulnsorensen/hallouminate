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
    let startup = config::load_startup(cli.logging_config_path())?;
    let _log_guard = logging::init(&startup.logging)?;
    cli::dispatch(cli).await
}
