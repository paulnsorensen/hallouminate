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
use super::client::connect_at;
use super::ipc::{DaemonRequest, DaemonRequestPayload, DaemonResponse, StatusReport};
use super::oplog::{ClientCall, Outcome};
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

/// [`status`] that records its daemon call on `call`.
///
/// # Errors
///
/// Returns the same errors as [`status`].
pub async fn status_logged(call: &mut ClientCall) -> anyhow::Result<DaemonStatus> {
    let socket = daemon_socket_path()?;
    status_with(&socket, Some(call)).await
}

async fn status_at(socket: &Path) -> anyhow::Result<DaemonStatus> {
    status_with(socket, None).await
}

pub(super) async fn status_with(
    socket: &Path,
    call: Option<&mut ClientCall>,
) -> anyhow::Result<DaemonStatus> {
    let Ok(client) = connect_at(socket).await else {
        if let Some(call) = call {
            call.unanswered(Outcome::Unavailable);
        }
        return Ok(DaemonStatus::NotRunning);
    };
    let req = DaemonRequest::new(
        std::env::current_dir().unwrap_or_default(),
        DaemonRequestPayload::Status,
    );
    let response = match call {
        Some(call) => {
            call.ready();
            client
                .call_raw_with_timeout_logged(req, STATUS_TIMEOUT, call)
                .await
        }
        None => client.call_raw_with_timeout(req, STATUS_TIMEOUT).await,
    };
    match response {
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
    let socket = daemon_socket_path()?;
    stop_at(&socket).await
}

/// [`stop`] that records its daemon call on `call`.
///
/// # Errors
///
/// Returns the same errors as [`stop`].
pub async fn stop_logged(call: &mut ClientCall) -> anyhow::Result<()> {
    let socket = daemon_socket_path()?;
    stop_with(&socket, Some(call)).await
}

pub(super) async fn stop_at(socket: &Path) -> anyhow::Result<()> {
    stop_with(socket, None).await
}

async fn stop_with(socket: &Path, call: Option<&mut ClientCall>) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    let Ok(client) = connect_at(socket).await else {
        if let Some(call) = call {
            call.unanswered(Outcome::Unavailable);
        }
        return Ok(());
    };
    let req = DaemonRequest::new(
        std::env::current_dir().unwrap_or_default(),
        DaemonRequestPayload::Shutdown,
    );
    let _ = match call {
        Some(call) => {
            call.ready();
            client
                .call_raw_with_timeout_logged(req, STOP_TIMEOUT, call)
                .await
        }
        None => client.call_raw_with_timeout(req, STOP_TIMEOUT).await,
    };

    loop {
        if !socket.exists() || UnixStream::connect(socket).await.is_err() {
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

/// [`restart`] that records its daemon calls on `call`. The row outcome is
/// the restart result: when no daemon ran, it is the outcome of the start.
///
/// # Errors
///
/// Returns the same errors as [`restart`].
pub async fn restart_logged(call: &mut ClientCall) -> anyhow::Result<()> {
    let socket = daemon_socket_path()?;
    restart_at(&socket, Some(call), ensure_daemon_running).await
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
    let socket = daemon_socket_path()?;
    restart_at(&socket, None, respawn).await
}

async fn restart_at<F, Fut>(
    socket: &Path,
    mut call: Option<&mut ClientCall>,
    respawn: F,
) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    stop_with(socket, call.as_deref_mut()).await?;
    let result = respawn().await;
    if let Some(call) = call {
        match &result {
            Ok(()) => call.settle(Outcome::Ok),
            Err(_) => call.settle(Outcome::Unavailable),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::ipc::{DebtLevel, TripState, WatcherCounters};
    use super::super::oplog::{Client, OpLog, OperationRecord, RecordKind};
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    async fn restart_row_outcome(respawn: anyhow::Result<()>) -> OperationRecord {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let log_dir = dir.path().join("log");
        let oplog = OpLog::in_dir(&log_dir, &hallouminate_config::LoggingConfig::default());
        let mut call = ClientCall::start(&oplog, Client::Cli, "restart", "shutdown", None);
        let _ = restart_at(&socket, Some(&mut call), || async move { respawn }).await;
        call.finish(Outcome::Ok);
        let mut rows = Vec::new();
        for entry in std::fs::read_dir(&log_dir).expect("log dir") {
            let path = entry.expect("log dir entry").path();
            for line in std::fs::read_to_string(&path).expect("segment").lines() {
                rows.push(serde_json::from_str::<OperationRecord>(line).expect("row json"));
            }
        }
        let [row] = <[OperationRecord; 1]>::try_from(rows).expect("one restart row");
        let RecordKind::ClientCall(fields) = &row.kind else {
            panic!("restart row must be a client_call row: {row:?}");
        };
        assert_eq!(fields.daemon_round_trip_ms, None, "{row:?}");
        row
    }

    #[tokio::test]
    async fn restart_without_running_daemon_records_start_outcome() {
        let started = restart_row_outcome(Ok(())).await;
        assert_eq!(started.outcome, Outcome::Ok, "{started:?}");
        let failed = restart_row_outcome(Err(anyhow::anyhow!("no start"))).await;
        assert_eq!(failed.outcome, Outcome::Unavailable, "{failed:?}");
    }

    #[tokio::test]
    async fn status_reports_running_for_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind socket");
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

    #[tokio::test]
    async fn stop_shuts_down_live_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind socket");
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
