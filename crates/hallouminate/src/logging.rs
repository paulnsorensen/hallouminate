//! Tracing subscriber wiring.
//!
//! Installs a byte-rotating appender at
//! `$XDG_STATE_HOME/hallouminate/hallouminate.log`
//! (fallback `~/.local/state/hallouminate/`). Numbered archives use
//! `hallouminate.log.1`, `.2`, and so on. Per-file and total byte limits come
//! from the baseline `[logging]` configuration.
//!
//! The returned `WorkerGuard` must be held while logging is active. Dropping it
//! drains and flushes queued records before joining the background writer.
//!
//! Filter precedence: `HALLOUMINATE_LOG` env var → `RUST_LOG` → default
//! `hallouminate=info`. Use the same syntax as `RUST_LOG`
//! (`hallouminate=debug,lance=warn`).

use std::path::{Path, PathBuf};

use file_rotate::compression::Compression;
use file_rotate::suffix::AppendCount;
use file_rotate::{ContentLimit, FileRotate};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_FILTER: &str = "hallouminate=info";

/// Install the global tracing subscriber. Returns a guard that must be held
/// while logging is active. Dropping the guard drains and flushes queued records
/// before joining the background writer.
pub fn init(config: &hallouminate_config::LoggingConfig) -> anyhow::Result<WorkerGuard> {
    let (non_blocking, guard) = log_writer(&state_dir(), config)?;

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

fn log_writer(
    dir: &Path,
    config: &hallouminate_config::LoggingConfig,
) -> anyhow::Result<(NonBlocking, WorkerGuard)> {
    let hallouminate_config::LoggingConfig {
        max_file_bytes,
        max_total_bytes,
    } = *config;
    let max_file_bytes = usize::try_from(max_file_bytes)
        .map_err(|_| anyhow::anyhow!("logging.max_file_bytes exceeds this platform's usize"))?;
    let total_slots = max_total_bytes / u64::try_from(max_file_bytes)?;
    let rotated_files = usize::try_from(total_slots.saturating_sub(1))
        .map_err(|_| anyhow::anyhow!("logging.max_total_bytes exceeds this platform's usize"))?;

    std::fs::create_dir_all(dir)?;
    let appender = FileRotate::new(
        dir.join("hallouminate.log"),
        AppendCount::new(rotated_files),
        ContentLimit::Bytes(max_file_bytes),
        Compression::None,
        None,
    );
    // Stay lossy (the crate default): the async daemon must not block a tokio
    // worker on a full log buffer. Flush-on-exit is guaranteed by `WorkerGuard`
    // drop, not by non-lossy mode.
    let (writer, guard) = NonBlockingBuilder::default().finish(appender);
    Ok((writer, guard))
}

fn state_dir() -> PathBuf {
    hallouminate_config::xdg::xdg_path("XDG_STATE_HOME", "~/.local/state", &["hallouminate"])
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    use tracing_subscriber::fmt::MakeWriter;
    #[test]
    fn state_dir_ends_under_hallouminate_subdir() {
        let dir = state_dir();
        assert!(
            dir.ends_with("hallouminate"),
            "state dir must terminate in the app subdir: {dir:?}"
        );
        assert!(dir.is_absolute(), "state dir must be absolute: {dir:?}");
    }

    #[test]
    fn size_rotation_bounds_active_and_total_bytes() -> anyhow::Result<()> {
        const MAX_FILE_BYTES: u64 = 64;
        const MAX_TOTAL_BYTES: u64 = 192;
        const BLOCK: &[u8; 32] = b"0123456789abcdef0123456789abcdef";
        const SENTINEL: &[u8] = b"drain-complete\n";

        let dir = tempfile::tempdir()?;
        let config = hallouminate_config::LoggingConfig {
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
        };
        let (writer, guard) = log_writer(dir.path(), &config)?;
        let mut writer = writer.make_writer();
        for _ in 0..10 {
            writer.write_all(BLOCK)?;
        }
        writer.write_all(SENTINEL)?;
        drop(writer);
        drop(guard);

        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir.path())? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let path = entry.path();
                let bytes = std::fs::read(&path)?;
                files.push((path, bytes));
            }
        }
        assert!(!files.is_empty(), "appender must create a log file");

        let mut active_index = None;
        for (index, (_, bytes)) in files.iter().enumerate() {
            let mut contains_sentinel = false;
            for window in bytes.windows(SENTINEL.len()) {
                if window == SENTINEL {
                    contains_sentinel = true;
                    break;
                }
            }
            if contains_sentinel {
                assert!(
                    active_index.is_none(),
                    "sentinel must occur in one log file"
                );
                active_index = Some(index);
            }
        }
        let Some(active_index) = active_index else {
            panic!("final sentinel must be drained before the guard returns");
        };

        let active_size = u64::try_from(files[active_index].1.len())?;
        assert!(
            active_size <= MAX_FILE_BYTES,
            "active log is {active_size} bytes, above {MAX_FILE_BYTES}"
        );

        let mut rotated_files = 0;
        let mut total_bytes = 0;
        for (index, (path, bytes)) in files.iter().enumerate() {
            let size = u64::try_from(bytes.len())?;
            total_bytes += size;
            if index != active_index {
                rotated_files += 1;
                assert!(
                    size <= MAX_FILE_BYTES,
                    "rotated log {path:?} is {size} bytes, above {MAX_FILE_BYTES}"
                );
            }
        }
        assert!(rotated_files > 0, "fixed writes must trigger size rotation");
        assert!(
            total_bytes <= MAX_TOTAL_BYTES,
            "retained logs total {total_bytes} bytes, above {MAX_TOTAL_BYTES}"
        );

        Ok(())
    }

    /// `max_total_bytes == max_file_bytes` is the floor of `total_slots =
    /// max_total_bytes / max_file_bytes` (integer division yields exactly 1
    /// slot, so `rotated_files` computes to 0). No archives are kept — only
    /// the active file, itself still capped at `max_file_bytes` — and total
    /// bytes on disk never exceed the configured limit.
    #[test]
    fn size_rotation_with_equal_limits_keeps_no_archives() -> anyhow::Result<()> {
        const MAX_FILE_BYTES: u64 = 64;
        const BLOCK: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

        let dir = tempfile::tempdir()?;
        let config = hallouminate_config::LoggingConfig {
            max_file_bytes: MAX_FILE_BYTES,
            max_total_bytes: MAX_FILE_BYTES,
        };
        let (writer, guard) = log_writer(dir.path(), &config)?;
        let mut writer = writer.make_writer();
        for _ in 0..10 {
            writer.write_all(BLOCK)?;
        }
        drop(writer);
        drop(guard);

        let mut total_bytes: u64 = 0;
        let mut file_count = 0;
        for entry in std::fs::read_dir(dir.path())? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                file_count += 1;
                total_bytes += entry.metadata()?.len();
            }
        }
        assert_eq!(
            file_count, 1,
            "equal limits must retain only the active file"
        );
        assert!(
            total_bytes <= MAX_FILE_BYTES,
            "retained logs total {total_bytes} bytes, above {MAX_FILE_BYTES}"
        );

        Ok(())
    }
}
