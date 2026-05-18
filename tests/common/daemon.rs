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
use hallouminate::app::daemon::{DaemonState, serve};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub struct DaemonHarness {
    socket: PathBuf,
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
        let socket = tmp.path().join("daemon.sock");
        let state = DaemonState::open(cfg).await.expect("open state");
        let (tx, rx) = oneshot::channel();
        let socket_clone = socket.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                res = serve(&state, &socket_clone) => res,
                _ = rx => Ok(()),
            }
        });
        // Wait for the socket to appear so the first client connect
        // doesn't race the bind.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            if std::time::Instant::now() > deadline {
                panic!("daemon socket never appeared: {}", socket.display());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        DaemonHarness {
            socket,
            _tmp: tmp,
            handle: Some(handle),
            shutdown: Some(tx),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
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
