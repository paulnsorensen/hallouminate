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
use hallouminate::app::daemon::{DaemonState, IDLE_READ_TIMEOUT, serve_with_idle_timeout};
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
    /// Boot a daemon against the given config, bound to a tempdir socket,
    /// using the production idle-read timeout.
    pub async fn spawn(cfg: Config) -> Self {
        Self::spawn_with_idle_timeout(cfg, IDLE_READ_TIMEOUT).await
    }

    /// Same as [`Self::spawn`], but with an explicit per-connection
    /// idle-read timeout — lets tests exercise the idle-timeout behavior
    /// without waiting out the real production default.
    pub async fn spawn_with_idle_timeout(cfg: Config, idle_timeout: Duration) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
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
                res = serve_with_idle_timeout(&state, &socket_clone, idle_timeout) => res,
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
