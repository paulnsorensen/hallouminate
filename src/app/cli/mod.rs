use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Index,
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

pub fn dispatch(cli: Cli) {
    match cli.command {
        Command::Index => println!("todo: index"),
        Command::Ground => println!("todo: ground"),
        Command::Hook { action } => match action {
            HookAction::Install => println!("todo: hook install"),
            HookAction::Uninstall => println!("todo: hook uninstall"),
        },
        Command::Config { action } => match action {
            ConfigAction::Init => println!("todo: config init"),
            ConfigAction::Show => println!("todo: config show"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_subcommand() {
        let cli = Cli::try_parse_from(["hallouminate", "index"]).expect("parse index");
        assert!(matches!(cli.command, Command::Index));
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
