use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

mod config;
mod ground;
mod hook;
mod index;

pub use config::{
    ConfigDownloadArgs, ConfigInitArgs, ConfigShowArgs, ConfigValidateArgs, cmd_config_download,
    cmd_config_init, cmd_config_show, cmd_config_validate,
};
pub use ground::{GroundArgs, cmd_ground, run_ground};
pub use hook::{HookArgs, cmd_hook_install, cmd_hook_uninstall};
pub use index::{
    AD_HOC_CORPUS_NAME, CorpusReport, IndexArgs, IndexReport, cmd_index, run_index, select_corpora,
};

/// CLI surface for output format selection. Mirrors `domain::ground::Format`
/// but kept in the app layer to keep `ValueEnum` (a clap dep) out of the
/// domain module.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    #[default]
    Outline,
    Json,
    JsonPretty,
}

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
    /// Boot the MCP server on stdio. Exposes `ground`, `index`,
    /// `list_corpora`, `list_files`, `add_markdown`, `read_markdown`, and
    /// `delete_markdown` tools to MCP-aware clients (Claude Desktop, Claude
    /// Code, etc.). The process runs until stdin closes.
    Serve,
    /// Boot the local daemon: single owner of the LanceDB ground directory,
    /// repository registry, and per-corpus mutation locks. CLI and MCP
    /// clients talk to it over a Unix domain socket. Stays in the
    /// foreground until killed; only one instance per socket can run.
    Daemon(DaemonCli),
}

#[derive(Debug, Args)]
pub struct DaemonCli {
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

impl From<DaemonCli> for crate::app::daemon::DaemonArgs {
    fn from(cli: DaemonCli) -> Self {
        Self { config: cli.config }
    }
}

#[derive(Debug, Args)]
pub struct IndexCli {
    #[arg(long)]
    pub corpus: Option<String>,
    /// Unsupported in the daemon-backed v1 — always errors with
    /// "paths_from is not supported via the daemon yet". Kept hidden so old
    /// scripts surface the message instead of a clap-level "unknown flag".
    #[arg(long, value_name = "FILE", hide = true)]
    pub paths_from: Option<PathBuf>,
    /// Accepted for backward compatibility with pre-layered-config scripts;
    /// no longer consulted locally. The daemon owns config resolution
    /// (XDG baseline at startup + repo-layer discovery per request), so to
    /// change what corpora are visible, restart the daemon
    /// (`hallouminate daemon --config ...`) or edit `.hallouminate/config.toml`
    /// in the working directory's repo.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Override the daemon socket path. Mirrors the `HALLOUMINATE_SOCKET`
    /// env var the daemon itself reads; lets test fixtures pin per-test
    /// sockets without env mutation.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl From<IndexCli> for IndexArgs {
    fn from(cli: IndexCli) -> Self {
        Self {
            corpus: cli.corpus,
            paths_from: cli.paths_from,
            config: cli.config,
            socket: cli.socket,
        }
    }
}

#[derive(Debug, Args)]
pub struct GroundCli {
    pub query: String,
    #[arg(long)]
    pub corpus: Option<String>,
    /// Output format. Default `outline` is the token-efficient ripgrep-style
    /// view. `json` and `json-pretty` emit the full structured response.
    /// Conflicts with `--full`.
    #[arg(long, value_enum, default_value_t = FormatArg::Outline, conflicts_with = "full")]
    pub format: FormatArg,
    /// Shorthand for `--format json-pretty`. The human-readable full view.
    #[arg(long, conflicts_with = "format")]
    pub full: bool,
    /// Trim each chunk's snippet to N chars (ending with `…` if truncated).
    /// Applies to every format — orthogonal to `--format` / `--full`.
    #[arg(long, value_name = "N")]
    pub snippet_chars: Option<usize>,
    #[arg(long, value_name = "N")]
    pub top_files: Option<usize>,
    #[arg(long, value_name = "N")]
    pub chunks_per_file: Option<usize>,
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
    /// Accepted for backward compatibility; same caveat as `hallouminate
    /// index --config`. The daemon owns config resolution (XDG baseline
    /// at startup + repo-layer discovery per request), so this flag is
    /// only consulted locally for the cosmetic outline path-prefix-strip
    /// and not for the actual search.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Override the daemon socket path. Mirrors `HALLOUMINATE_SOCKET`; lets
    /// test fixtures pin per-test sockets without env mutation.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl From<GroundCli> for GroundArgs {
    fn from(cli: GroundCli) -> Self {
        use crate::domain::ground::Format;
        let format = if cli.full {
            Format::JsonPretty
        } else {
            match cli.format {
                FormatArg::Outline => Format::Outline,
                FormatArg::Json => Format::Json,
                FormatArg::JsonPretty => Format::JsonPretty,
            }
        };
        Self {
            query: cli.query,
            corpus: cli.corpus,
            format,
            snippet_chars: cli.snippet_chars,
            top_files: cli.top_files,
            chunks_per_file: cli.chunks_per_file,
            limit: cli.limit,
            config: cli.config,
            socket: cli.socket,
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
        /// Working directory for repo-config discovery (defaults to current dir).
        #[arg(long, value_name = "PATH")]
        cwd: Option<PathBuf>,
    },
    Download {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Parse the config, print a summary, and flag unknown top-level keys
    /// (e.g. `[[corpora]]` typo for `[[corpus]]`).
    Validate {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Working directory for repo-config discovery (defaults to current dir).
        #[arg(long, value_name = "PATH")]
        cwd: Option<PathBuf>,
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
            ConfigAction::Init { force, path } => cmd_config_init(ConfigInitArgs { force, path }),
            ConfigAction::Show { config, cwd } => {
                cmd_config_show(ConfigShowArgs { config, cwd })
            }
            ConfigAction::Download { config } => cmd_config_download(ConfigDownloadArgs { config }),
            ConfigAction::Validate { config, cwd } => {
                cmd_config_validate(ConfigValidateArgs { config, cwd })
            }
        },
        Command::Serve => {
            crate::app::daemon::ensure_daemon_running().await?;
            crate::adapters::mcp::serve_stdio().await
        }
        Command::Daemon(args) => crate::app::daemon::run_daemon(args.into()).await,
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
    fn parses_ground_subcommand_with_query_and_outline_default() {
        let cli =
            Cli::try_parse_from(["hallouminate", "ground", "spice melange"]).expect("parse ground");
        match cli.command {
            Command::Ground(args) => {
                assert_eq!(args.query, "spice melange");
                assert!(!args.full);
                assert_eq!(args.format, FormatArg::Outline);
                assert_eq!(args.corpus, None);
                assert_eq!(args.snippet_chars, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_ground_with_format_json_flag() {
        let cli = Cli::try_parse_from(["hallouminate", "ground", "q", "--format", "json"])
            .expect("parse");
        match cli.command {
            Command::Ground(args) => {
                assert_eq!(args.format, FormatArg::Json);
                assert!(!args.full);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_ground_with_full_flag() {
        let cli = Cli::try_parse_from(["hallouminate", "ground", "q", "--full"]).expect("parse");
        match cli.command {
            Command::Ground(args) => {
                assert!(args.full);
                // GroundArgs conversion maps --full → Format::JsonPretty.
                let ga: GroundArgs = args.into();
                assert_eq!(ga.format, crate::domain::ground::Format::JsonPretty);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_ground_with_snippet_chars() {
        let cli = Cli::try_parse_from(["hallouminate", "ground", "q", "--snippet-chars", "80"])
            .expect("parse");
        match cli.command {
            Command::Ground(args) => assert_eq!(args.snippet_chars, Some(80)),
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
            "--format",
            "json-pretty",
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
                assert_eq!(args.format, FormatArg::JsonPretty);
                assert_eq!(args.top_files, Some(5));
                assert_eq!(args.chunks_per_file, Some(2));
                assert_eq!(args.limit, Some(20));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn rejects_ground_without_query() {
        let err = Cli::try_parse_from(["hallouminate", "ground"]).expect_err("query required");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn rejects_full_and_format_together() {
        // The two flags are mutually exclusive — clap should error before
        // dispatch instead of letting the user wonder which one won.
        let err =
            Cli::try_parse_from(["hallouminate", "ground", "q", "--full", "--format", "json"])
                .expect_err("conflicting flags must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn rejects_removed_pretty_flag() {
        // Pre-1.0 break: --pretty was the old name for what's now --full.
        // clap must surface a clean unknown-arg error instead of silently
        // ignoring the flag and emitting compact JSON.
        let err = Cli::try_parse_from(["hallouminate", "ground", "q", "--pretty"])
            .expect_err("--pretty must be rejected as unknown");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn parses_daemon_subcommand_without_args() {
        let cli = Cli::try_parse_from(["hallouminate", "daemon"]).expect("parse daemon");
        match cli.command {
            Command::Daemon(args) => {
                assert!(args.config.is_none(), "--config defaults to None");
                let inner: crate::app::daemon::DaemonArgs = args.into();
                assert!(inner.config.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_daemon_subcommand_with_config_flag() {
        let cli = Cli::try_parse_from(["hallouminate", "daemon", "--config", "/tmp/cfg.toml"])
            .expect("parse daemon --config");
        match cli.command {
            Command::Daemon(args) => {
                assert_eq!(args.config, Some(PathBuf::from("/tmp/cfg.toml")));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parses_hook_install_and_uninstall() {
        let install =
            Cli::try_parse_from(["hallouminate", "hook", "install"]).expect("parse hook install");
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
        let cli = Cli::try_parse_from(["hallouminate", "hook", "install", "--repo", "/tmp/r"])
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
        let init =
            Cli::try_parse_from(["hallouminate", "config", "init"]).expect("parse config init");
        match init.command {
            Command::Config {
                action: ConfigAction::Init { force, path },
            } => {
                assert!(!force);
                assert_eq!(path, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let show =
            Cli::try_parse_from(["hallouminate", "config", "show"]).expect("parse config show");
        match show.command {
            Command::Config {
                action: ConfigAction::Show { config, cwd: _ },
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
        let cli =
            Cli::try_parse_from(["hallouminate", "config", "show", "--config", "/tmp/c.toml"])
                .expect("parse");
        match cli.command {
            Command::Config {
                action: ConfigAction::Show { config, cwd: _ },
            } => assert_eq!(config, Some(PathBuf::from("/tmp/c.toml"))),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_config_download_with_explicit_config_path() {
        let cli = Cli::try_parse_from([
            "hallouminate",
            "config",
            "download",
            "--config",
            "/tmp/c.toml",
        ])
        .expect("parse");
        match cli.command {
            Command::Config {
                action: ConfigAction::Download { config },
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
