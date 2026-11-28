//! Shared `DaemonHarness` for integration tests. Spawns the daemon
//! in-process against a tempdir socket, waits for a successful `Ping`
//! round-trip (not just socket-path existence) so the first real client
//! connect doesn't race the listener's `accept`, and tears the daemon down
//! on drop.
//!
//! Lives here (rather than copy-pasted across CLI / MCP suites) so the
//! spawn / shutdown shape stays uniform and the per-test socket lifetime
//! is impossible to leak.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hallouminate_config::{Config, LoggingConfig};
use hallouminate_daemon::oplog::{self, OpLog, OperationRecord};
use hallouminate_daemon::{
    DaemonRequest, DaemonRequestPayload, DaemonState, IDLE_READ_TIMEOUT, connect_at,
    serve_with_idle_timeout,
};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::Dispatch;
use tracing_subscriber::layer::SubscriberExt;

pub struct DaemonHarness {
    socket: PathBuf,
    cwd: PathBuf,
    state_dir: PathBuf,
    state: DaemonState,
    _tmp: tempfile::TempDir,
    handle: Option<JoinHandle<anyhow::Result<()>>>,
    shutdown: Option<oneshot::Sender<()>>,
    row_runtime: Option<RowRuntime>,
}

enum RowCapture {
    Off,
    On(OpLog),
}

/// Runtime that serves a row-capturing daemon. Every runtime thread has the
/// capture dispatcher as its default: the registry releases a closed span's
/// parent through the default dispatcher, and aborted handlers drop outside
/// any `WithDispatch` scope.
struct RowRuntime {
    runtime: Runtime,
    dispatch: Dispatch,
}

impl RowRuntime {
    fn new(log: OpLog) -> Self {
        let dispatch = Dispatch::new(tracing_subscriber::registry().with(oplog::layer(log)));
        let thread_dispatch = dispatch.clone();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .on_thread_start(move || {
                std::mem::forget(tracing::dispatcher::set_default(&thread_dispatch));
            })
            .build()
            .expect("row-capture runtime");
        RowRuntime { runtime, dispatch }
    }

    fn stop(self) {
        let RowRuntime { runtime, dispatch } = self;
        tracing::dispatcher::with_default(&dispatch, || runtime.shutdown_background());
    }
}

impl DaemonHarness {
    /// Boot a daemon against the given config, bound to a tempdir socket,
    /// using the production idle-read timeout.
    pub async fn spawn(cfg: Config) -> Self {
        Self::spawn_with_idle_timeout(cfg, IDLE_READ_TIMEOUT).await
    }

    /// Same as [`Self::spawn`], but with an explicit per-connection
    /// idle-read timeout — lets tests exercise the idle-timeout behavior
    /// without waiting out the real production default.
    pub async fn spawn_with_idle_timeout(cfg: Config, idle_timeout: Duration) -> Self {
        Self::boot(cfg, idle_timeout, |_state_dir| RowCapture::Off).await
    }

    /// Same as [`Self::spawn`], but the served daemon also writes its
    /// operations-log rows to [`Self::state_dir`] with the `logging` settings.
    pub async fn spawn_with_oplog(cfg: Config, logging: LoggingConfig) -> Self {
        Self::boot(cfg, IDLE_READ_TIMEOUT, |state_dir| {
            RowCapture::On(OpLog::in_dir(state_dir, &logging))
        })
        .await
    }

    /// Same as [`Self::spawn_with_oplog`], but the rows go to `log`.
    pub async fn spawn_with_oplog_writer(cfg: Config, log: OpLog) -> Self {
        Self::boot(cfg, IDLE_READ_TIMEOUT, |_state_dir| RowCapture::On(log)).await
    }

    async fn boot(
        cfg: Config,
        idle_timeout: Duration,
        capture: impl FnOnce(&Path) -> RowCapture,
    ) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("daemon.sock");
        let state_dir = tmp.path().join("state");

        // Per repo-config-discovery: every daemon request walks its `cwd`
        // looking for `.hallouminate/config.toml`. Seed an empty repo-layer
        // file in the harness tempdir so tests can pass `harness.cwd()` as
        // the request envelope's `cwd`. An empty TOML file parses to
        // `Config::default()`, which merges trivially into the baseline.
        let cwd = tmp.path().to_path_buf();
        let hallou_dir = cwd.join(".hallouminate");
        std::fs::create_dir_all(&hallou_dir).expect("mkdir .hallouminate");
        std::fs::write(hallou_dir.join("config.toml"), "").expect("write empty repo config");

        let state = DaemonState::open(cfg, None).await.expect("open state");
        let (tx, rx) = oneshot::channel();
        let socket_clone = socket.clone();
        let served = {
            let state = state.clone();
            async move {
                let server = serve_with_idle_timeout(&state, &socket_clone, idle_timeout);
                tokio::pin!(server);
                tokio::select! {
                    res = &mut server => res,
                    _ = rx => {
                        state.shutdown_token().cancel();
                        server.await
                    },
                }
            }
        };
        let (handle, row_runtime) = match capture(&state_dir) {
            RowCapture::Off => (tokio::spawn(served), None),
            RowCapture::On(log) => {
                let row_runtime = RowRuntime::new(log);
                (row_runtime.runtime.spawn(served), Some(row_runtime))
            }
        };
        // Wait for a successful Ping round-trip, not just socket-path
        // existence: `bind` creates the path before `accept` is ready to
        // service connections, so a bare `socket.exists()` check leaves a
        // window where the very first client connect can race the listener
        // and see ECONNREFUSED.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("daemon never became ready to serve: {}", socket.display());
            }
            if let Ok(client) = connect_at(&socket).await
                && client
                    .call_raw(DaemonRequest::new(cwd.clone(), DaemonRequestPayload::Ping))
                    .await
                    .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        DaemonHarness {
            socket,
            cwd,
            state_dir,
            state,
            _tmp: tmp,
            handle: Some(handle),
            shutdown: Some(tx),
            row_runtime,
        }
    }

    /// Stops the daemon and waits for its task to release owned resources.
    ///
    /// # Errors
    ///
    /// Returns an error when shutdown exceeds five seconds, the task panics,
    /// or serving fails.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let Some(mut handle) = self.handle.take() else {
            return Ok(());
        };
        match tokio::time::timeout(Duration::from_secs(5), &mut handle).await {
            Ok(result) => result??,
            Err(_) => {
                handle.abort();
                let _ = handle.await;
                anyhow::bail!("daemon harness shutdown timed out after five seconds");
            }
        }
        Ok(())
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Handle to the served daemon state, for tests that hold its write lane
    /// to stall background work.
    pub fn state(&self) -> &DaemonState {
        &self.state
    }

    /// Path of the harness tempdir, which contains an empty
    /// `.hallouminate/config.toml`. Use as the `cwd` field of any
    /// `DaemonRequest` sent through a client connected to this harness.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Directory that receives the operations-log segments of a daemon from
    /// [`Self::spawn_with_oplog`]. It does not exist before the first row.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Every row in the operations-log segments under [`Self::state_dir`].
    pub fn rows(&self) -> Vec<OperationRecord> {
        read_segment_rows(&self.state_dir)
    }

    /// Waits up to five seconds for a row with `request_id`, then returns it.
    pub async fn row_for(&self, request_id: &str) -> OperationRecord {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            for row in self.rows() {
                if row.request_id.as_str() == request_id {
                    return row;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("no operations-log row for request {request_id}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Stops serving without the graceful drain: the daemon is shutting down
    /// and every in-flight handler is aborted, as after the drain deadline.
    pub fn abort_in_flight(&mut self) {
        self.state.shutdown_token().cancel();
        self.shutdown.take();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Returns the operations-log segment paths in name order.
pub fn segment_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut segments = Vec::new();
    for entry in entries {
        let path = entry.expect("segment dir entry").path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("operations-") && name.ends_with(".jsonl") {
            segments.push(path);
        }
    }
    segments.sort();
    segments
}

/// Returns the operations-log rows in segment order, oldest segment first.
pub fn read_segment_rows(dir: &Path) -> Vec<OperationRecord> {
    let mut rows = Vec::new();
    for segment in segment_paths(dir) {
        let text = std::fs::read_to_string(&segment).expect("read segment");
        for line in text.lines() {
            let row: OperationRecord = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("segment line is not a row ({error}): {line}"));
            rows.push(row);
        }
    }
    rows
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if let Some(row_runtime) = self.row_runtime.take() {
            row_runtime.stop();
        }
    }
}
