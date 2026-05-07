use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

mod ground;
mod index;

pub use ground::{cmd_ground, run_ground, FusionChoice, GroundArgs};
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
    Ground(GroundCli),
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

#[derive(Debug, Args)]
pub struct GroundCli {
    pub query: String,
    #[arg(long)]
    pub pretty: bool,
    #[arg(long, value_name = "N")]
    pub top_files: Option<usize>,
    #[arg(long, value_name = "N")]
    pub chunks_per_file: Option<usize>,
    #[arg(long, value_enum)]
    pub fusion: Option<FusionChoice>,
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

impl From<GroundCli> for GroundArgs {
    fn from(cli: GroundCli) -> Self {
        Self {
            query: cli.query,
            pretty: cli.pretty,
            top_files: cli.top_files,
            chunks_per_file: cli.chunks_per_file,
            fusion: cli.fusion,
            limit: cli.limit,
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
        Command::Ground(args) => cmd_ground(args.into()),
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
    fn parses_ground_subcommand_with_query() {
        let cli = Cli::try_parse_from(["hallouminate", "ground", "spice melange"])
            .expect("parse ground");
        match cli.command {
            Command::Ground(args) => {
                assert_eq!(args.query, "spice melange");
                assert!(!args.pretty);
                assert_eq!(args.fusion, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_ground_with_overrides() {
        let cli = Cli::try_parse_from([
            "hallouminate",
            "ground",
            "tokio",
            "--pretty",
            "--top-files",
            "5",
            "--chunks-per-file",
            "2",
            "--fusion",
            "convex",
            "--limit",
            "20",
        ])
        .expect("parse ground with flags");
        match cli.command {
            Command::Ground(args) => {
                assert_eq!(args.query, "tokio");
                assert!(args.pretty);
                assert_eq!(args.top_files, Some(5));
                assert_eq!(args.chunks_per_file, Some(2));
                assert_eq!(args.fusion, Some(FusionChoice::Convex));
                assert_eq!(args.limit, Some(20));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn rejects_ground_without_query() {
        let err =
            Cli::try_parse_from(["hallouminate", "ground"]).expect_err("query required");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
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
