//! Daemon lifecycle client operations backing `daemon stop|restart|status`.
//!
//! These reuse the existing owner-only control socket as the channel — no
//! pidfile, no PID discovery. `stop` sends the IPC `Shutdown` request and
//! polls until the socket disappears; `status` probes liveness with `Ping`;
//! `restart` stops a running daemon (if any) then re-spawns via
//! `ensure_daemon_running`.

use std::time::Duration;

use tokio::net::UnixStream;

use super::bootstrap::ensure_daemon_running;
use super::client::connect_at;
use super::ipc::{DaemonRequest, DaemonRequestPayload};
use super::socket::daemon_socket_path;

const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(50);
/// Bound on the `Ping` round trip `status` uses to probe liveness — an
/// accepted-but-silent socket must report `NotRunning`, not hang the CLI.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Liveness of the daemon for `daemon status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    Running,
    NotRunning,
}

/// Probe the daemon: `Ping` over the control socket. A connect failure (no
/// socket, or a stale socket with no listener) maps to `NotRunning`.
pub async fn status() -> anyhow::Result<DaemonStatus> {
    let socket = daemon_socket_path();
    let client = match connect_at(&socket).await {
        Ok(c) => c,
        Err(_) => return Ok(DaemonStatus::NotRunning),
    };
    match client
        .call_raw_with_timeout(
            DaemonRequest {
                cwd: std::env::current_dir().unwrap_or_default(),
                payload: DaemonRequestPayload::Ping,
            },
            STATUS_TIMEOUT,
        )
        .await
    {
        Ok(_) => Ok(DaemonStatus::Running),
        Err(_) => Ok(DaemonStatus::NotRunning),
    }
}

/// Ask the running daemon to shut down and wait until the socket is gone.
///
/// No-ops (returns `Ok`) when no daemon is reachable — stopping an
/// already-stopped daemon is success, not an error. The `Shutdown` request
/// is config-independent on the server side, so `cwd` does not need to
/// resolve a repo config.
pub async fn stop() -> anyhow::Result<()> {
    let socket = daemon_socket_path();
    // Start the deadline before sending `Shutdown`, not after `call_raw`
    // returns — otherwise an accepted-but-silent socket lets the send itself
    // hang indefinitely and the poll loop below never gets a chance to time
    // out.
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    let client = match connect_at(&socket).await {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    // Send Shutdown. A transport error here can mean the daemon raced us and
    // already closed; treat that as "already stopping" and fall through to
    // the socket-gone poll rather than failing. Bounded by the same deadline
    // so a wedged daemon can't hang this call past `STOP_TIMEOUT`.
    let _ = client
        .call_raw_with_timeout(
            DaemonRequest {
                cwd: std::env::current_dir().unwrap_or_default(),
                payload: DaemonRequestPayload::Shutdown,
            },
            STOP_TIMEOUT,
        )
        .await;

    loop {
        // Socket file removed by the daemon's cleanup, OR present-but-dead
        // (connect refused) — either means it's down.
        if !socket.exists() || UnixStream::connect(&socket).await.is_err() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not stop within {}s (socket {} still reachable)",
                STOP_TIMEOUT.as_secs(),
                socket.display(),
            );
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}

/// Stop the running daemon (if any), then spawn a fresh one and wait for it
/// to become reachable.
pub async fn restart() -> anyhow::Result<()> {
    restart_with(ensure_daemon_running).await
}

/// `restart` with an injectable respawn step — the test seam behind
/// [`restart`]. Production calls `restart` (respawn = `ensure_daemon_running`).
///
/// The integration suite sets `HALLOUMINATE_SOCKET`, which makes the
/// production `ensure_daemon_running` a deliberate no-op (the explicit-socket
/// convention hands lifecycle to the caller). A test calling `restart()`
/// directly would therefore stop the daemon and never bring it back, asserting
/// nothing about the stop→respawn→reachable sequence. Injecting an in-process
/// `serve` as the respawn lets the suite drive that full sequence against a
/// controllable socket and assert the daemon is genuinely reachable afterward.
#[doc(hidden)]
pub async fn restart_with<F, Fut>(respawn: F) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    stop().await?;
    respawn().await
}
