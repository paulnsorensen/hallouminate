use clap::Parser;

pub mod adapters;
pub mod app;
pub mod domains;

fn main() {
    let cli = app::cli::Cli::parse();
    app::cli::dispatch(cli);
}
