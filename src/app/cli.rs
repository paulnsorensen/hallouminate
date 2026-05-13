use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

mod config;
mod ground;
mod hook;
mod index;

pub use config::{cmd_config_init, cmd_config_show, ConfigInitArgs, ConfigShowArgs};
pub use ground::{cmd_ground, run_ground, GroundArgs};
pub use hook::{cmd_hook_install, cmd_hook_uninstall, HookArgs};
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
    pub corpus: Option<String>,
    #[arg(long)]
    pub pretty: bool,
    #[arg(long, value_name = "N")]
    pub top_files: Option<usize>,
    #[arg(long, value_name = "N")]
    pub chunks_per_file: Option<usize>,
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

impl From<GroundCli> for GroundArgs {
    fn from(cli: GroundCli) -> Self {
        Self {
            query: cli.query,
            corpus: cli.corpus,
            pretty: cli.pretty,
            top_files: cli.top_files,
            chunks_per_file: cli.chunks_per_file,
            limit: cli.limit,
            config: cli.config,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum HookAction {
    Install {
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
    Uninstall {
        #[arg(long, value_name = "PATH")]
        repo: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    Init {
        #[arg(long)]
        force: bool,
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
    Show {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Index(args) => cmd_index(args.into()).await,
        Command::Ground(args) => cmd_ground(args.into()).await,
        Command::Hook { action } => match action {
            HookAction::Install { repo } => cmd_hook_install(HookArgs { repo }),
            HookAction::Uninstall { repo } => cmd_hook_uninstall(HookArgs { repo }),
        },
        Command::Config { action } => match action {
            ConfigAction::Init { force, path } => {
                cmd_config_init(ConfigInitArgs { force, path })
            }
            ConfigAction::Show { config } => cmd_config_show(ConfigShowArgs { config }),
        },
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
                assert_eq!(args.corpus, None);
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
            "--corpus",
            "docs",
            "--pretty",
            "--top-files",
            "5",
            "--chunks-per-file",
            "2",
            "--limit",
            "20",
        ])
        .expect("parse ground with flags");
        match cli.command {
            Command::Ground(args) => {
                assert_eq!(args.query, "tokio");
                assert_eq!(args.corpus.as_deref(), Some("docs"));
                assert!(args.pretty);
                assert_eq!(args.top_files, Some(5));
                assert_eq!(args.chunks_per_file, Some(2));
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
        let install = Cli::try_parse_from(["hallouminate", "hook", "install"])
            .expect("parse hook install");
        match install.command {
            Command::Hook {
                action: HookAction::Install { repo },
            } => assert_eq!(repo, None),
            other => panic!("wrong variant: {other:?}"),
        }
        let uninstall = Cli::try_parse_from(["hallouminate", "hook", "uninstall"])
            .expect("parse hook uninstall");
        match uninstall.command {
            Command::Hook {
                action: HookAction::Uninstall { repo },
            } => assert_eq!(repo, None),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_hook_install_with_repo_path() {
        let cli =
            Cli::try_parse_from(["hallouminate", "hook", "install", "--repo", "/tmp/r"])
                .expect("parse hook install --repo");
        match cli.command {
            Command::Hook {
                action: HookAction::Install { repo },
            } => assert_eq!(repo, Some(PathBuf::from("/tmp/r"))),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_config_init_and_show() {
        let init = Cli::try_parse_from(["hallouminate", "config", "init"])
            .expect("parse config init");
        match init.command {
            Command::Config {
                action: ConfigAction::Init { force, path },
            } => {
                assert!(!force);
                assert_eq!(path, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let show = Cli::try_parse_from(["hallouminate", "config", "show"])
            .expect("parse config show");
        match show.command {
            Command::Config {
                action: ConfigAction::Show { config },
            } => assert_eq!(config, None),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_config_init_with_force_and_path() {
        let cli = Cli::try_parse_from([
            "hallouminate",
            "config",
            "init",
            "--force",
            "--path",
            "/tmp/cfg.toml",
        ])
        .expect("parse");
        match cli.command {
            Command::Config {
                action: ConfigAction::Init { force, path },
            } => {
                assert!(force);
                assert_eq!(path, Some(PathBuf::from("/tmp/cfg.toml")));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_config_show_with_explicit_config_path() {
        let cli = Cli::try_parse_from([
            "hallouminate",
            "config",
            "show",
            "--config",
            "/tmp/c.toml",
        ])
        .expect("parse");
        match cli.command {
            Command::Config {
                action: ConfigAction::Show { config },
            } => assert_eq!(config, Some(PathBuf::from("/tmp/c.toml"))),
            other => panic!("wrong variant: {other:?}"),
        }
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
