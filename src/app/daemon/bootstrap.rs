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
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::net::UnixStream;

use super::client::connect_at;
use super::daemon_socket_path;
use super::ipc::{DaemonRequest, DaemonRequestPayload, DaemonResponse};

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
        // A daemon is already listening. `flock` guarantees it's the only one,
        // but NOT that it's our version: after a binary upgrade, this fresh
        // MCP server could silently drive a daemon spawned from the old
        // release (Curd C). Adopt it only when its reported version matches
        // ours.
        if running_daemon_version_matches(&socket).await {
            return Ok(());
        }
        // Skew: stop the stale daemon, then fall through to the spawn path to
        // bring up a fresh one. This is exactly `lifecycle::restart`'s
        // stop→respawn sequence, open-coded here because `restart`'s respawn
        // step IS `ensure_daemon_running` — calling it would recurse.
        tracing::info!(
            target: "hallouminate::daemon",
            ours = env!("CARGO_PKG_VERSION"),
            "running daemon version mismatch or unverifiable; restarting it",
        );
        super::lifecycle::stop().await?;
    }

    // Stderr capture for the auto-spawned daemon. Lives in the XDG state
    // dir alongside the rotating tracing log; the bootstrap log catches
    // anything emitted before the subscriber installs (panics, early
    // config errors) and is the fallback diagnostic when the daemon
    // refuses to come up.
    let log_path = crate::app::xdg::xdg_path(
        "XDG_STATE_HOME",
        "~/.local/state",
        &["hallouminate", "daemon-bootstrap.log"],
    );
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

/// Probe the already-running daemon's version via `Ping`. Returns `true` only
/// when the daemon reports our `CARGO_PKG_VERSION`.
///
/// Tolerant by design (see the [`PongResult`](super::ipc::PongResult) wire-compat
/// note): a daemon from a release before the version field answers the bare
/// `"pong"` string — which has no `version` — and any transport or daemon-side
/// error is likewise unverifiable. All of these resolve to `false` (→ restart)
/// rather than erroring, so an unverifiable daemon is replaced, never adopted.
async fn running_daemon_version_matches(socket: &Path) -> bool {
    let client = match connect_at(socket).await {
        Ok(c) => c,
        Err(e) => {
            // Don't swallow the probe failure silently on this non-interactive
            // bootstrap path: log it so an alive-but-unprobeable daemon (which
            // we then treat as skew and restart) is diagnosable.
            tracing::debug!(
                target: "hallouminate::daemon",
                error = %e,
                "version probe could not connect to the running daemon; treating as unverifiable",
            );
            return false;
        }
    };
    match client
        .call_raw(DaemonRequest {
            cwd: std::env::current_dir().unwrap_or_default(),
            payload: DaemonRequestPayload::Ping,
        })
        .await
    {
        Ok(resp) => pong_reports_our_version(&resp),
        Err(e) => {
            tracing::debug!(
                target: "hallouminate::daemon",
                error = %e,
                "version probe Ping failed; treating the daemon as unverifiable",
            );
            false
        }
    }
}

/// Pure skew-detection predicate: does this `Ping` response report OUR
/// version? Split out from the I/O so the tolerance contract is unit-testable
/// without a live socket. A non-`Ok` envelope, a missing `version` (the old
/// bare-`"pong"` daemon), or a different version all yield `false` → restart.
fn pong_reports_our_version(resp: &DaemonResponse) -> bool {
    matches!(
        resp,
        DaemonResponse::Ok { result }
            if result.get("version").and_then(|v| v.as_str()) == Some(env!("CARGO_PKG_VERSION"))
    )
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
        assert!(!has_explicit_socket_override(Some(std::ffi::OsStr::new(
            ""
        ))));
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

    // ── Curd C: version-skew detection ───────────────────────────────────

    #[test]
    fn pong_with_our_version_is_a_match() {
        let resp = DaemonResponse::ok(&super::super::ipc::PongResult {
            version: env!("CARGO_PKG_VERSION").to_string(),
        });
        assert!(pong_reports_our_version(&resp), "same version must match");
    }

    #[test]
    fn pong_with_a_different_version_is_a_mismatch() {
        // A daemon from another release reports its own version; the new
        // client must NOT adopt it.
        let resp = DaemonResponse::ok(&super::super::ipc::PongResult {
            version: "0.0.0-stale".to_string(),
        });
        assert!(
            !pong_reports_our_version(&resp),
            "a different version must not match"
        );
    }

    #[test]
    fn bare_pong_string_from_old_daemon_is_a_mismatch() {
        // Pre-Curd-C daemons answer Ping with the bare string `"pong"`, which
        // has no `version`. The client must treat that as a mismatch (→
        // restart), never as a match or a hard error.
        let resp = DaemonResponse::ok(&"pong");
        assert!(
            !pong_reports_our_version(&resp),
            "legacy bare-pong daemon must be treated as skew"
        );
    }

    #[test]
    fn error_response_is_a_mismatch() {
        let resp = DaemonResponse::internal("boom");
        assert!(
            !pong_reports_our_version(&resp),
            "an error response is unverifiable → mismatch"
        );
    }
}
