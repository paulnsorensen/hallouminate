//! Tracing subscriber wiring.
//!
//! Installs a `tracing-appender` daily-rolling file appender at
//! `$XDG_STATE_HOME/hallouminate/hallouminate.log.YYYY-MM-DD`
//! (fallback `~/.local/state/hallouminate/`). Retention is 14 files via
//! `max_log_files` — cleanup runs at each rotation boundary, which for the
//! daemon means once per day.
//!
//! The returned `WorkerGuard` must be held for the process lifetime so the
//! background writer thread keeps draining. Drop it and pending logs are
//! lost.
//!
//! Filter precedence: `HALLOUMINATE_LOG` env var → `RUST_LOG` → default
//! `hallouminate=info`. Use the same syntax as `RUST_LOG`
//! (`hallouminate=debug,lance=warn`).

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

const STATE_FALLBACK_BASE: &str = "~/.local/state";
const STATE_SUBDIR: &str = "hallouminate";
const LOG_FILENAME_PREFIX: &str = "hallouminate";
const LOG_FILENAME_SUFFIX: &str = "log";
const MAX_LOG_FILES: usize = 14;
const DEFAULT_FILTER: &str = "hallouminate=info";

/// Install the global tracing subscriber. Returns a guard that must be
/// held for the process lifetime — dropping it shuts down the background
/// writer and drops pending log records.
pub fn init() -> anyhow::Result<WorkerGuard> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;

    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILENAME_PREFIX)
        .filename_suffix(LOG_FILENAME_SUFFIX)
        .max_log_files(MAX_LOG_FILES)
        .build(&dir)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_env("HALLOUMINATE_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .try_init()
        .map_err(|e| anyhow::anyhow!("install tracing subscriber: {e}"))?;

    Ok(guard)
}

fn state_dir() -> PathBuf {
    state_dir_from(std::env::var_os("XDG_STATE_HOME").as_deref())
}

fn state_dir_from(xdg_state_home: Option<&std::ffi::OsStr>) -> PathBuf {
    let base = xdg_state_home
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(shellexpand::tilde(STATE_FALLBACK_BASE).into_owned()));
    base.join(STATE_SUBDIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_honors_xdg_state_home_when_set() {
        let dir = state_dir_from(Some(std::ffi::OsStr::new("/var/state")));
        assert_eq!(dir, PathBuf::from("/var/state/hallouminate"));
    }

    #[test]
    fn state_dir_treats_empty_xdg_state_home_as_unset() {
        let dir = state_dir_from(Some(std::ffi::OsStr::new("")));
        assert!(
            dir.ends_with(".local/state/hallouminate"),
            "unexpected fallback: {dir:?}"
        );
    }

    #[test]
    fn state_dir_falls_back_to_local_state_under_home() {
        let dir = state_dir_from(None);
        assert!(
            dir.ends_with(".local/state/hallouminate"),
            "unexpected fallback: {dir:?}"
        );
    }
}
