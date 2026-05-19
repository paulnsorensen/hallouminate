//! Daemon auto-spawn for the MCP `serve` command.
//!
//! `ground` / `index` / etc. deliberately fail loudly when the daemon is
//! unreachable (see `mod.rs`) — interactive users get a clear "start the
//! daemon" hint. The MCP transport (`hallouminate serve`) is non-interactive:
//! the caller is Claude Code or another MCP host, and the user does not see
//! daemon-bootstrap errors. So `serve` auto-spawns a daemon on first launch.
//!
//! The flock in `acquire_single_instance` makes this safe under concurrent
//! `serve` launches: only one spawned daemon wins the lock; the others exit
//! cleanly and every `serve` polls the same socket.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::net::UnixStream;

use super::daemon_socket_path;

const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const LOG_PATH: &str = "~/.cache/hallouminate/daemon.log";

/// Ensure a daemon is reachable, spawning a detached one if not.
///
/// No-ops when `HALLOUMINATE_SOCKET` is set: the explicit-socket convention
/// means the caller (tests, custom launchers) manages daemon lifecycle.
pub async fn ensure_daemon_running() -> anyhow::Result<()> {
    if std::env::var_os("HALLOUMINATE_SOCKET").is_some_and(|v| !v.is_empty()) {
        return Ok(());
    }

    let socket = daemon_socket_path();
    if UnixStream::connect(&socket).await.is_ok() {
        return Ok(());
    }

    let log_path = PathBuf::from(shellexpand::tilde(LOG_PATH).into_owned());
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log.try_clone()?;

    let exe = std::env::current_exe()?;
    Command::new(&exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()?;

    let deadline = std::time::Instant::now() + SPAWN_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(&socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    anyhow::bail!(
        "daemon did not start within {}s; see {}",
        SPAWN_TIMEOUT.as_secs(),
        log_path.display()
    )
}
