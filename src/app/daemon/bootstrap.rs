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
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::net::UnixStream;

use super::daemon_socket_path;

const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Ensure a daemon is reachable, spawning a detached one if not.
///
/// No-ops when `HALLOUMINATE_SOCKET` is set: the explicit-socket convention
/// means the caller (tests, custom launchers) manages daemon lifecycle.
pub async fn ensure_daemon_running() -> anyhow::Result<()> {
    if has_explicit_socket_override(std::env::var_os("HALLOUMINATE_SOCKET").as_deref()) {
        return Ok(());
    }

    let socket = daemon_socket_path();
    if UnixStream::connect(&socket).await.is_ok() {
        return Ok(());
    }

    // Stderr capture for the auto-spawned daemon. Lives in the XDG state
    // dir alongside the rotating tracing log; the bootstrap log catches
    // anything emitted before the subscriber installs (panics, early
    // config errors) and is the fallback diagnostic when the daemon
    // refuses to come up.
    let log_path =
        crate::app::xdg::xdg_path("XDG_STATE_HOME", "~/.local/state", &["hallouminate", "daemon-bootstrap.log"]);
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

/// Pure predicate split out so tests can exercise both branches without
/// mutating process env (unsafe on edition 2024).
fn has_explicit_socket_override(env_value: Option<&std::ffi::OsStr>) -> bool {
    env_value.is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_socket_env_does_not_bypass_spawn() {
        assert!(!has_explicit_socket_override(None));
    }

    #[test]
    fn empty_socket_env_does_not_bypass_spawn() {
        // POSIX/XDG convention: empty env value treated as unset.
        assert!(!has_explicit_socket_override(Some(std::ffi::OsStr::new(""))));
    }

    #[test]
    fn set_socket_env_bypasses_spawn() {
        // Test harnesses set HALLOUMINATE_SOCKET to per-test sockets and
        // manage their own daemon. ensure_daemon_running must no-op there
        // so a stray hallouminate daemon doesn't get spawned during tests.
        assert!(has_explicit_socket_override(Some(std::ffi::OsStr::new(
            "/tmp/test.sock"
        ))));
    }
}
