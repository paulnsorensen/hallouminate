//! Tracing subscriber wiring.
//!
//! Installs a `tracing-appender` daily-rolling file appender at
//! `$XDG_STATE_HOME/hallouminate/hallouminate.YYYY-MM-DD.log`
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
use tracing_subscriber::{EnvFilter, fmt};

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
        .filename_prefix("hallouminate")
        .filename_suffix("log")
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
    crate::xdg::xdg_path("XDG_STATE_HOME", "~/.local/state", &["hallouminate"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_ends_under_hallouminate_subdir() {
        let dir = state_dir();
        assert!(
            dir.ends_with("hallouminate"),
            "state dir must terminate in the app subdir: {dir:?}"
        );
        assert!(dir.is_absolute(), "state dir must be absolute: {dir:?}");
    }
}
