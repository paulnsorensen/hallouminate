use clap::Parser;

use hallouminate::app;

fn main() -> anyhow::Result<()> {
    let cli = app::cli::Cli::parse();
    app::cli::dispatch(cli)
}
