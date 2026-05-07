pub mod cli;
pub mod config;

use clap::Parser;

pub fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    cli::dispatch(cli)
}
