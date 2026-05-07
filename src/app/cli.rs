use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

mod index;

pub use index::{cmd_index, IndexArgs};

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Index(IndexCli),
    Ground,
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Debug, Args)]
pub struct IndexCli {
    #[arg(long)]
    pub corpus: Option<String>,
    #[arg(long, value_name = "FILE")]
    pub paths_from: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

impl From<IndexCli> for IndexArgs {
    fn from(cli: IndexCli) -> Self {
        Self {
            corpus: cli.corpus,
            paths_from: cli.paths_from,
            config: cli.config,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum HookAction {
    Install,
    Uninstall,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    Init,
    Show,
}

pub fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Index(args) => cmd_index(args.into()),
        Command::Ground => {
            println!("todo: ground");
            Ok(())
        }
        Command::Hook { action } => {
            match action {
                HookAction::Install => println!("todo: hook install"),
                HookAction::Uninstall => println!("todo: hook uninstall"),
            }
            Ok(())
        }
        Command::Config { action } => {
            match action {
                ConfigAction::Init => println!("todo: config init"),
                ConfigAction::Show => println!("todo: config show"),
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_subcommand() {
        let cli = Cli::try_parse_from(["hallouminate", "index"]).expect("parse index");
        assert!(matches!(cli.command, Command::Index(_)));
    }

    #[test]
    fn parses_index_with_corpus_and_paths_from() {
        let cli = Cli::try_parse_from([
            "hallouminate",
            "index",
            "--corpus",
            "docs",
            "--paths-from",
            "/tmp/p.txt",
        ])
        .expect("parse");
        match cli.command {
            Command::Index(args) => {
                assert_eq!(args.corpus.as_deref(), Some("docs"));
                assert_eq!(args.paths_from, Some(PathBuf::from("/tmp/p.txt")));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_ground_subcommand() {
        let cli = Cli::try_parse_from(["hallouminate", "ground"]).expect("parse ground");
        assert!(matches!(cli.command, Command::Ground));
    }

    #[test]
    fn parses_hook_install_and_uninstall() {
        let install =
            Cli::try_parse_from(["hallouminate", "hook", "install"]).expect("parse hook install");
        assert!(matches!(
            install.command,
            Command::Hook {
                action: HookAction::Install
            }
        ));
        let uninstall = Cli::try_parse_from(["hallouminate", "hook", "uninstall"])
            .expect("parse hook uninstall");
        assert!(matches!(
            uninstall.command,
            Command::Hook {
                action: HookAction::Uninstall
            }
        ));
    }

    #[test]
    fn parses_config_init_and_show() {
        let init =
            Cli::try_parse_from(["hallouminate", "config", "init"]).expect("parse config init");
        assert!(matches!(
            init.command,
            Command::Config {
                action: ConfigAction::Init
            }
        ));
        let show =
            Cli::try_parse_from(["hallouminate", "config", "show"]).expect("parse config show");
        assert!(matches!(
            show.command,
            Command::Config {
                action: ConfigAction::Show
            }
        ));
    }

    #[test]
    fn rejects_missing_subcommand() {
        let err = Cli::try_parse_from(["hallouminate"]).expect_err("requires subcommand");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn rejects_unknown_hook_action() {
        let err = Cli::try_parse_from(["hallouminate", "hook", "frobnicate"])
            .expect_err("unknown hook action");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
