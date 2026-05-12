use clap::Parser;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(short, long, default_value = "world")]
    name: String,
}

pub fn run() {
    let cli = Cli::parse();
    println!("Hello, {}!", cli.name);
}
