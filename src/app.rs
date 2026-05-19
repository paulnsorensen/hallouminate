pub mod cli;
pub mod config;
pub mod daemon;
pub mod input_error;
pub mod logging;
pub mod xdg;

use clap::Parser;

pub async fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let _log_guard = logging::init()?;
    cli::dispatch(cli).await
}
