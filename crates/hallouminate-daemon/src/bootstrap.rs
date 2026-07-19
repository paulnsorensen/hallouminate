//! Daemon auto-spawn for the MCP `serve` command.
//!
//! `ground` / `index` / etc. deliberately fail loudly when the daemon is
//! unreachable (see `lib.rs`) — interactive users get a clear "start the
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

const CONNECTION_BUDGET: Duration = Duration::from_secs(90);
const INITIAL_POLL_INTERVAL: Duration = Duration::from_millis(200);
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Upper bound on the version probe's `Ping` round-trip. `call_raw` has no
/// built-in timeout, so a daemon that accepts the connection but never replies
/// would otherwise hang `ensure_daemon_running` at MCP startup. Cap it: an
/// elapsed probe is treated as unverifiable (→ restart), the same fate as a
/// transport error or a legacy bare-`"pong"` reply.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Size cap for `daemon-bootstrap.log`. The file is append-only across every
/// auto-spawn and failed start, so with no cap repeated restarts (or a daemon
/// that fails to come up in a crash loop) grow it forever. It is a startup
/// diagnostic, not an audit trail, so a simple truncate-at-startup is enough.
const MAX_BOOTSTRAP_LOG_BYTES: u64 = 1024 * 1024;

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
    let log_path = hallouminate_config::xdg::xdg_path(
        "XDG_STATE_HOME",
        "~/.local/state",
        &["hallouminate", "daemon-bootstrap.log"],
    );
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    truncate_log_if_oversized(&log_path, MAX_BOOTSTRAP_LOG_BYTES)?;
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

    let started = std::time::Instant::now();
    let socket_for_connect = socket.clone();
    wait_for_daemon_socket(
        &socket,
        &log_path,
        CONNECTION_BUDGET,
        move || {
            let socket = socket_for_connect.clone();
            async move { UnixStream::connect(socket).await.is_ok() }
        },
        tokio::time::sleep,
        || started.elapsed(),
    )
    .await
}

async fn wait_for_daemon_socket<Connect, ConnectFuture, Sleep, SleepFuture, Elapsed>(
    socket: &Path,
    log_path: &Path,
    connection_budget: Duration,
    mut connect: Connect,
    mut sleep: Sleep,
    mut elapsed: Elapsed,
) -> anyhow::Result<()>
where
    Connect: FnMut() -> ConnectFuture,
    ConnectFuture: std::future::Future<Output = bool>,
    Sleep: FnMut(Duration) -> SleepFuture,
    SleepFuture: std::future::Future<Output = ()>,
    Elapsed: FnMut() -> Duration,
{
    let mut poll_interval = INITIAL_POLL_INTERVAL;
    loop {
        if connect().await {
            return Ok(());
        }

        let remaining = connection_budget.saturating_sub(elapsed());
        if remaining.is_zero() {
            break;
        }

        sleep(poll_interval.min(remaining)).await;
        poll_interval = poll_interval.saturating_mul(2).min(MAX_POLL_INTERVAL);
    }

    anyhow::bail!(
        "daemon socket {} did not become reachable within the {}s total connection budget; see bootstrap log {}",
        socket.display(),
        connection_budget.as_secs(),
        log_path.display(),
    )
}

/// Pure predicate split out so tests can exercise both branches without
/// mutating process env (unsafe on edition 2024).
fn has_explicit_socket_override(env_value: Option<&std::ffi::OsStr>) -> bool {
    env_value.is_some_and(|v| !v.is_empty())
}

/// Caps `daemon-bootstrap.log` at `max_bytes` before the next append. Called
/// once per `ensure_daemon_running` invocation, right before the log is
/// (re)opened in append mode, so a log that grew past the cap on a prior
/// spawn (or crash loop) is reset to empty instead of appended to forever.
/// A missing file (first run) is not an error.
fn truncate_log_if_oversized(path: &Path, max_bytes: u64) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > max_bytes => std::fs::File::create(path).map(|_| ()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
    let probe = client.call_raw(DaemonRequest {
        cwd: std::env::current_dir().unwrap_or_default(),
        payload: DaemonRequestPayload::Ping,
    });
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Ok(Ok(resp)) => pong_reports_our_version(&resp),
        Ok(Err(e)) => {
            tracing::debug!(
                target: "hallouminate::daemon",
                error = %e,
                "version probe Ping failed; treating the daemon as unverifiable",
            );
            false
        }
        Err(_elapsed) => {
            // The daemon accepted the connection but did not answer within
            // `PROBE_TIMEOUT`. A wedged daemon must not hang startup — treat
            // the silence as unverifiable so the caller restarts it.
            tracing::debug!(
                target: "hallouminate::daemon",
                timeout_secs = PROBE_TIMEOUT.as_secs(),
                "version probe Ping timed out; treating the daemon as unverifiable",
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

    #[tokio::test]
    async fn socket_available_after_thirty_seconds_connects_within_total_budget() {
        let elapsed = std::rc::Rc::new(std::cell::Cell::new(Duration::ZERO));
        let connect_attempts = std::rc::Rc::new(std::cell::Cell::new(0));
        let connect_elapsed = elapsed.clone();
        let observed_connect_attempts = connect_attempts.clone();
        let sleep_elapsed = elapsed.clone();
        let socket = Path::new("/tmp/hallouminate-delayed.sock");
        let log = Path::new("/tmp/hallouminate-bootstrap.log");

        wait_for_daemon_socket(
            socket,
            log,
            CONNECTION_BUDGET,
            || {
                observed_connect_attempts.set(observed_connect_attempts.get() + 1);
                std::future::ready(connect_elapsed.get() > Duration::from_secs(30))
            },
            |delay| {
                sleep_elapsed.set(sleep_elapsed.get() + delay);
                std::future::ready(())
            },
            || elapsed.get(),
        )
        .await
        .expect("daemon becoming available in the second window must connect");

        assert_eq!(elapsed.get(), Duration::from_millis(30_400));
        assert_eq!(
            connect_attempts.get(),
            33,
            "polling must continue past 30 seconds without restarting the wait"
        );
    }

    #[tokio::test]
    async fn unavailable_socket_exhausts_total_budget_with_connection_context() {
        let elapsed = std::rc::Rc::new(std::cell::Cell::new(Duration::ZERO));
        let sleep_elapsed = elapsed.clone();
        let delays = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let observed_delays = delays.clone();
        let socket = Path::new("/tmp/hallouminate-unavailable.sock");
        let log = Path::new("/tmp/hallouminate-bootstrap.log");

        let error = wait_for_daemon_socket(
            socket,
            log,
            CONNECTION_BUDGET,
            || std::future::ready(false),
            |delay| {
                observed_delays.borrow_mut().push(delay);
                sleep_elapsed.set(sleep_elapsed.get() + delay);
                std::future::ready(())
            },
            || elapsed.get(),
        )
        .await
        .expect_err("an unavailable daemon must exhaust its bounded budget");
        let error = error.to_string();
        let delays = delays.borrow();

        assert_eq!(elapsed.get(), CONNECTION_BUDGET);
        assert_eq!(delays.len(), 92);
        assert_eq!(
            &delays[..3],
            &[
                INITIAL_POLL_INTERVAL,
                Duration::from_millis(400),
                Duration::from_millis(800),
            ],
        );
        for delay in &delays[3..delays.len() - 1] {
            assert_eq!(*delay, MAX_POLL_INTERVAL);
        }
        assert_eq!(
            delays.last().copied(),
            Some(Duration::from_millis(600)),
            "the final sleep must be clipped to the remaining budget"
        );
        assert_eq!(
            error,
            "daemon socket /tmp/hallouminate-unavailable.sock did not become reachable within the 90s total connection budget; see bootstrap log /tmp/hallouminate-bootstrap.log"
        );
    }

    // ── bootstrap log size cap ──

    #[test]
    fn undersized_log_is_left_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon-bootstrap.log");
        std::fs::write(&path, b"small").expect("write");
        truncate_log_if_oversized(&path, 1024).expect("truncate check");
        assert_eq!(std::fs::read(&path).expect("read"), b"small");
    }

    #[test]
    fn oversized_log_is_truncated_to_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon-bootstrap.log");
        std::fs::write(&path, vec![b'x'; 2048]).expect("write");
        truncate_log_if_oversized(&path, 1024).expect("truncate");
        assert_eq!(
            std::fs::read(&path).expect("read").len(),
            0,
            "log past the cap must be reset to empty, not left to grow further"
        );
    }

    #[test]
    fn missing_log_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.log");
        assert!(truncate_log_if_oversized(&path, 1024).is_ok());
    }
}
