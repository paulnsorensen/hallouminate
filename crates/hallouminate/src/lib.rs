//! Application entry layer: CLI parsing, configuration loading, the daemon,
//! logging, and the MCP server.

pub mod cli;
pub mod input_error;
pub mod logging;
pub mod mcp;

use clap::Parser;

/// Parse CLI arguments, initialize logging, and dispatch to the selected
/// command.
pub async fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let startup = hallouminate_config::load_startup(cli.logging_config_path())?;
    let _log_guard = logging::init(log_role(&cli.command), &startup.logging)?;
    cli::dispatch(cli, startup).await
}

fn log_role(command: &cli::Command) -> logging::LogRole {
    match command {
        cli::Command::Serve => logging::LogRole::Serve,
        cli::Command::Daemon(cli::DaemonCli { action, config: _ }) => match action {
            None => logging::LogRole::Daemon,
            Some(cli::DaemonAction::Run) => logging::LogRole::Daemon,
            Some(cli::DaemonAction::Stop) => logging::LogRole::Cli,
            Some(cli::DaemonAction::Restart) => logging::LogRole::Cli,
            Some(cli::DaemonAction::Status) => logging::LogRole::Cli,
        },
        cli::Command::Index(_) => logging::LogRole::Cli,
        cli::Command::Ground(_) => logging::LogRole::Cli,
        cli::Command::Hook { .. } => logging::LogRole::Cli,
        cli::Command::InitRepo(_) => logging::LogRole::Cli,
        cli::Command::Config { .. } => logging::LogRole::Cli,
        cli::Command::Wiki { .. } => logging::LogRole::Cli,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_role_follows_the_subcommand() -> anyhow::Result<()> {
        let cases = [
            (vec!["hallouminate", "daemon"], logging::LogRole::Daemon),
            (
                vec!["hallouminate", "daemon", "run"],
                logging::LogRole::Daemon,
            ),
            (
                vec!["hallouminate", "daemon", "status"],
                logging::LogRole::Cli,
            ),
            (
                vec!["hallouminate", "daemon", "restart"],
                logging::LogRole::Cli,
            ),
            (vec!["hallouminate", "serve"], logging::LogRole::Serve),
            (
                vec!["hallouminate", "ground", "query"],
                logging::LogRole::Cli,
            ),
        ];
        for (args, expected) in cases {
            let cli = cli::Cli::try_parse_from(&args)?;
            assert_eq!(log_role(&cli.command), expected, "{args:?}");
        }
        Ok(())
    }
}
