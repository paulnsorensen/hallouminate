//! Daemon lifecycle client operations backing `daemon stop|restart|status`.
//!
//! These reuse the existing owner-only control socket as the channel — no
//! pidfile, no PID discovery. `stop` sends the IPC `Shutdown` request and
//! polls until the socket disappears; `status` probes liveness with `Ping`;
//! `restart` stops a running daemon (if any) then re-spawns via
//! `ensure_daemon_running`.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use super::bootstrap::ensure_daemon_running;
use super::client::connect_primary_or_sibling;
use super::ipc::{DaemonRequest, DaemonRequestPayload, DaemonResponse, StatusReport};
use super::socket::daemon_socket_paths;

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
    let paths = daemon_socket_paths()?;
    status_at(paths.canonical(), paths.legacy()).await
}

async fn status_at(primary: &Path, sibling: Option<&Path>) -> anyhow::Result<DaemonStatus> {
    let Some(client) = connect_primary_or_sibling(primary, sibling).await else {
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

/// Ask the running daemon to shut down and wait until the socket is gone.
///
/// No-ops (returns `Ok`) when no daemon is reachable — stopping an
/// already-stopped daemon is success, not an error. The `Shutdown` request
/// is config-independent on the server side, so `cwd` does not need to
/// resolve a repo config.
pub async fn stop() -> anyhow::Result<()> {
    let paths = daemon_socket_paths()?;
    stop_at(paths.canonical(), paths.legacy()).await
}

async fn stop_at(primary: &Path, sibling: Option<&Path>) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    let Some(client) = connect_primary_or_sibling(primary, sibling).await else {
        return Ok(());
    };
    let socket = client.socket_path().to_path_buf();
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

#[cfg(test)]
mod tests {
    use super::super::ipc::{DebtLevel, TripState, WatcherCounters};
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn status_uses_live_sibling_when_primary_is_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("primary.sock");
        let sibling = dir.path().join("sibling.sock");
        let listener = tokio::net::UnixListener::bind(&sibling).expect("bind sibling");
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

        let status = status_at(&primary, Some(&sibling))
            .await
            .expect("status through sibling");

        match status {
            DaemonStatus::Running(report) => assert_eq!(report.defer_count, 0),
            DaemonStatus::NotRunning => panic!("live sibling must report Running"),
        }
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn stop_shuts_down_live_sibling_when_primary_is_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let primary = dir.path().join("primary.sock");
        let sibling = dir.path().join("sibling.sock");
        let listener = tokio::net::UnixListener::bind(&sibling).expect("bind sibling");
        let sibling_for_server = sibling.clone();
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
                std::fs::remove_file(&sibling_for_server).expect("remove sibling socket");
                break;
            }
        });

        stop_at(&primary, Some(&sibling))
            .await
            .expect("stop through sibling");

        assert!(!sibling.exists(), "sibling socket must be removed");
        server.await.expect("server task");
    }
}
