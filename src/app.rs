pub mod cli;
pub mod config;
pub mod daemon;
pub mod input_error;

use clap::Parser;

pub async fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    cli::dispatch(cli).await
}
