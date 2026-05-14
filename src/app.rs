pub mod cli;
pub mod config;

use clap::Parser;

pub async fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    cli::dispatch(cli).await
}
