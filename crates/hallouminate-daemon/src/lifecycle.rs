//! Daemon lifecycle client operations backing `daemon stop|restart|status`.
//!
//! These reuse the existing owner-only control socket as the channel — no
//! pidfile, no PID discovery. `stop` sends the IPC `Shutdown` request and
//! polls until the socket disappears; `status` probes liveness with `Ping`;
//! `restart` stops a running daemon (if any) then re-spawns via
//! `ensure_daemon_running`.

use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
use super::transport::LocalStream;

use super::bootstrap::ensure_daemon_running;
use super::client::connect_at;
use super::ipc::{DaemonRequest, DaemonRequestPayload, DaemonResponse, StatusReport};
use super::socket::daemon_socket_path;

const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(50);
/// Bound on the `Status` round trip `status` uses — an accepted-but-silent
/// socket must report `NotRunning`, not hang the CLI.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Result of `daemon status`: a reachable daemon answers with its full
/// [`StatusReport`]; anything unreachable is `NotRunning`.
#[derive(Debug, Clone)]
pub enum DaemonStatus {
    Running(StatusReport),
    NotRunning,
}

/// Query the daemon's self-status over the control socket. Transport
/// failures (no socket, stale socket with no listener, silent or timed-out
/// peer) map to `NotRunning`; a daemon that answers but returns an error or
/// an unparseable payload is a real fault and surfaces as `Err` — it IS
/// running, so reporting `NotRunning` would be a lie.
pub async fn status() -> anyhow::Result<DaemonStatus> {
    let socket = daemon_socket_path()?;
    status_at(&socket).await
}

async fn status_at(socket: &Path) -> anyhow::Result<DaemonStatus> {
    let Ok(client) = connect_at(socket).await else {
        return Ok(DaemonStatus::NotRunning);
    };
    match client
        .call_raw_with_timeout(
            DaemonRequest {
                cwd: std::env::current_dir().unwrap_or_default(),
                payload: DaemonRequestPayload::Status,
            },
            STATUS_TIMEOUT,
        )
        .await
    {
        Ok(DaemonResponse::Ok { result }) => {
            let report: StatusReport = serde_json::from_value(result)
                .map_err(|e| anyhow::anyhow!("daemon returned unexpected status payload: {e}"))?;
            Ok(DaemonStatus::Running(report))
        }
        Ok(DaemonResponse::Err { kind, message }) => {
            anyhow::bail!("daemon status request failed ({kind:?}): {message}")
        }
        Err(_) => Ok(DaemonStatus::NotRunning),
    }
}

/// Requests shutdown and waits for the daemon to release its endpoint.
///
/// Stopping an already-stopped daemon succeeds. Shutdown does not require a
/// repository configuration.
///
/// # Errors
///
/// Returns an error if the daemon does not stop before the deadline, or if
/// the Windows endpoint or single-instance lock cannot be checked.
pub async fn stop() -> anyhow::Result<()> {
    let socket = daemon_socket_path()?;
    stop_at(&socket).await
}

pub(super) async fn stop_at(socket: &Path) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    let client = match connect_at(socket).await {
        Ok(client) => Some(client),
        #[cfg(unix)]
        Err(_error) => return Ok(()),
        #[cfg(windows)]
        Err(error) => {
            if !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|cause| cause.kind() == std::io::ErrorKind::NotFound)
            {
                return Err(error);
            }
            None
        }
    };
    if let Some(client) = client {
        let _ = client
            .call_raw_with_timeout(
                DaemonRequest {
                    cwd: std::env::current_dir().unwrap_or_default(),
                    payload: DaemonRequestPayload::Shutdown,
                },
                deadline.saturating_duration_since(std::time::Instant::now()),
            )
            .await;
    }

    loop {
        #[cfg(unix)]
        let stopped = !socket.exists() || LocalStream::connect(socket).await.is_err();
        #[cfg(windows)]
        let stopped = daemon_lock_released(socket)?;
        if stopped {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not stop within {}s (endpoint {})",
                STOP_TIMEOUT.as_secs(),
                socket.display(),
            );
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}

#[cfg(windows)]
fn daemon_lock_released(socket: &Path) -> std::io::Result<bool> {
    use std::fs::{OpenOptions, TryLockError};

    // The pipe can stop accepting clients before the daemon releases its lock.
    let path = super::server::lock_path_for(socket);
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error),
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

#[cfg(test)]
mod tests {
    use super::super::ipc::{DebtLevel, TripState, WatcherCounters};
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn status_reports_running_for_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let listener = crate::transport::LocalListener::bind(&socket).expect("bind socket");
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(read_half);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read request");
                if line.is_empty() {
                    continue;
                }
                let response = DaemonResponse::ok(&StatusReport {
                    per_task: Vec::new(),
                    debt: DebtLevel::Ok,
                    defer_count: 0,
                    watcher: WatcherCounters::default(),
                    trips: TripState::None,
                    degraded_watch_roots: 0,
                });
                let mut response = serde_json::to_string(&response).expect("serialize response");
                response.push('\n');
                write_half
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
                break;
            }
        });

        let status = status_at(&socket).await.expect("status");

        match status {
            DaemonStatus::Running(report) => assert_eq!(report.defer_count, 0),
            DaemonStatus::NotRunning => panic!("live socket must report Running"),
        }
        server.await.expect("server task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_shuts_down_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let listener = crate::transport::LocalListener::bind(&socket).expect("bind socket");
        let socket_for_server = socket.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(read_half);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read request");
                if line.is_empty() {
                    continue;
                }
                let request: DaemonRequest =
                    serde_json::from_str(line.trim_end()).expect("deserialize shutdown request");
                match request.payload {
                    DaemonRequestPayload::Shutdown => {}
                    other => panic!("expected Shutdown, got {other:?}"),
                }
                let response = DaemonResponse::ok(&"stopping");
                let mut response = serde_json::to_string(&response).expect("serialize response");
                response.push('\n');
                write_half
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
                drop(write_half);
                drop(listener);
                std::fs::remove_file(&socket_for_server).expect("remove socket");
                break;
            }
        });

        stop_at(&socket).await.expect("stop");

        assert!(!socket.exists(), "socket must be removed");
        server.await.expect("server task");
    }
}
