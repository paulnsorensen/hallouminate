//! Shared `DaemonHarness` for integration tests. Spawns the daemon
//! in-process against a tempdir socket, waits for the socket to appear
//! (so the first client connect doesn't race the bind), and tears the
//! daemon down on drop.
//!
//! Lives here (rather than copy-pasted across CLI / MCP suites) so the
//! spawn / shutdown shape stays uniform and the per-test socket lifetime
//! is impossible to leak.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hallouminate::app::config::Config;
use hallouminate::app::daemon::{DaemonState, connect_at, serve};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub struct DaemonHarness {
    socket: PathBuf,
    cwd: PathBuf,
    _tmp: tempfile::TempDir,
    handle: Option<JoinHandle<anyhow::Result<()>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl DaemonHarness {
    /// Boot a daemon against the given config, bound to a tempdir socket.
    /// The returned harness owns the tempdir, daemon task, and shutdown
    /// channel; drop tears them all down.
    pub async fn spawn(cfg: Config) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        // On unix this is the real socket file path; on Windows the transport
        // derives a per-path named pipe from it (no on-disk artifact), so the
        // readiness wait below must connect-probe rather than stat the path.
        let socket = tmp.path().join("daemon.sock");

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
        let handle = tokio::spawn(async move {
            tokio::select! {
                res = serve(&state, &socket_clone) => res,
                _ = rx => Ok(()),
            }
        });
        // Wait until the daemon accepts a connection so the first real client
        // call doesn't race the bind. A connect-probe is the one cross-platform
        // readiness check — `socket.exists()` is meaningless for a named pipe
        // (there is no on-disk file) and racy even on unix (a stale socket file
        // can exist before the listener binds).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if connect_at(&socket).await.is_ok() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("daemon never became reachable: {}", socket.display());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        DaemonHarness {
            socket,
            cwd,
            _tmp: tmp,
            handle: Some(handle),
            shutdown: Some(tx),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Path of the harness tempdir, which contains an empty
    /// `.hallouminate/config.toml`. Use as the `cwd` field of any
    /// `DaemonRequest` sent through a client connected to this harness.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
