//! Daemon accept loop.
//!
//! `run_daemon` binds the configured socket, takes a single-instance lock
//! via `flock`, and dispatches one request per connection. The protocol is
//! intentionally minimal: read one JSON line, write one JSON line, close.
//! Per-corpus serialization and the global write-lane live in
//! `dispatch::dispatch`; the accept loop is only responsible for surfacing
//! framing/IO errors.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::app::config::{self, Config};

use super::dispatch::dispatch;
use super::ipc::{DaemonRequest, DaemonResponse};
use super::socket::daemon_socket_path;
use super::state::DaemonState;

#[derive(Debug, Default, Clone)]
pub struct DaemonArgs {
    pub config: Option<PathBuf>,
}

/// How long `handle_connection` waits for a client to send its request
/// line before giving up and closing the connection. Guards against a
/// client that opens a connection and never writes (or writes a partial
/// line with no trailing newline), which would otherwise pin a
/// `BufReader::read_line` await forever and leak the per-connection task.
pub const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Boot the daemon and serve until SIGINT/SIGTERM (or stdin close on the
/// rare debug invocations). Returns `Err` if another daemon is already
/// holding the single-instance lock on the configured socket directory.
pub async fn run_daemon(args: DaemonArgs) -> anyhow::Result<()> {
    let cfg = config::load_xdg(args.config.as_deref())?;
    // Capture the baseline source path so the dispatcher can name it in
    // scalar-conflict diagnostics (AC #7). When the user passed
    // `--config PATH`, that path *is* the baseline; otherwise the XDG path
    // is what `load_xdg` consulted.
    let xdg_path = args.config.clone().unwrap_or_else(config::xdg_config_path);
    let socket_path = daemon_socket_path();
    serve_with_config(cfg, Some(xdg_path), &socket_path).await
}

/// Production wiring that takes the lock first, *then* opens the LanceDB
/// handle and model. Critical for the single-instance invariant: a second
/// daemon launched against the same socket must never briefly co-own the
/// ground directory before failing on the lock — that's exactly the
/// multi-process LanceDB race the daemon exists to prevent.
async fn serve_with_config(
    cfg: Config,
    xdg_path: Option<PathBuf>,
    socket_path: &Path,
) -> anyhow::Result<()> {
    prepare_socket_dir(socket_path).await?;
    let lock_path = lock_path_for(socket_path);
    let lock = acquire_single_instance(&lock_path)?;
    let state = DaemonState::open(cfg, xdg_path).await?;
    remove_stale_socket(socket_path).await;
    let watcher = super::watch::spawn_corpus_watcher(&state);
    spawn_signal_handlers(&state);
    spawn_idle_exit(&state, state.baseline().daemon.idle_exit_secs);
    tokio::spawn(super::dispatch::catch_up_index(state.clone()));
    let result = serve_on_listener(&state, socket_path, IDLE_READ_TIMEOUT).await;
    drop(watcher);
    cleanup(lock, socket_path).await;
    result
}

/// Wire SIGINT and SIGTERM onto the daemon's shutdown token so a `kill` (or
/// Ctrl-C in the foreground) drains the accept loop and runs the same
/// flock-drop + socket-removal cleanup as the IPC `Shutdown` request, rather
/// than dying on default signal disposition and leaving a stale socket.
///
/// The SIGTERM stream is registered **synchronously** (before the function
/// returns), so on return the process's default-terminate disposition is
/// already overridden — a `kill -TERM` after this returns reaches the token,
/// not the default killer. This synchronous postcondition is what the SIGTERM
/// integration test relies on to raise the signal without a spawn race.
pub fn spawn_signal_handlers(state: &DaemonState) {
    let token = state.shutdown_token().clone();
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "hallouminate::daemon", error = %e, "failed to install SIGTERM handler");
            return;
        }
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!(target: "hallouminate::daemon", "received SIGINT; shutting down");
            }
            _ = sigterm.recv() => {
                tracing::info!(target: "hallouminate::daemon", "received SIGTERM; shutting down");
            }
        }
        token.cancel();
    });
}

/// Spawn the process-level idle-exit watcher (ADR-001/003). When the activity
/// clock is quiet for `idle_exit_secs` and no connection is active, cancel the
/// shutdown token — the same clean exit SIGTERM drives — so the OS reclaims all
/// memory (the ONNX BFCArena included); the next CLI/MCP use respawns the
/// daemon. `idle_exit_secs == 0` disables it.
fn spawn_idle_exit(state: &DaemonState, idle_exit_secs: u64) {
    if idle_exit_secs == 0 {
        return;
    }
    let state = state.clone();
    let cancel = state.shutdown_token().clone();
    tokio::spawn(async move {
        loop {
            // Sleep to the deadline, not a fixed period: recomputing the
            // remaining window each iteration bounds idle-exit overshoot to
            // ~one short sleep regardless of `idle_exit_secs`. The `.max(1)`
            // floor avoids a busy-loop when the deadline has already passed
            // but a connection is still active (`should_idle_exit` false).
            let secs = state.secs_until_idle(idle_exit_secs).max(1);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(secs)) => {
                    if state.should_idle_exit(idle_exit_secs) {
                        tracing::info!(
                            target: "hallouminate::daemon",
                            idle_secs = idle_exit_secs,
                            "daemon idle-exit; exiting so the OS reclaims all memory",
                        );
                        state.shutdown_token().cancel();
                        break;
                    }
                }
            }
        }
    });
}

/// Remove the socket file, then release the single-instance flock (dropping
/// the `File` releases the advisory lock, POSIX). This order matters: if the
/// flock were released first, a respawning daemon could win it, remove the
/// stale socket, and bind a fresh one — which this process's trailing
/// `remove_file` would then delete, leaving the new daemon bound but
/// unreachable. Removing the socket first instead costs only a benign window
/// where a racing respawn sees the socket gone while the flock is briefly
/// still held and bounces with a clear "already holds" error.
async fn cleanup(lock: std::fs::File, socket_path: &Path) {
    let _ = tokio::fs::remove_file(socket_path).await;
    drop(lock);
}

async fn prepare_socket_dir(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| anyhow::anyhow!("create socket parent dir {}: {e}", parent.display()))?;
        // 0o700: owner-only access. Without this, another local user on a
        // shared machine could traverse the parent dir, connect to the
        // socket, and issue mutating requests — the daemon has no
        // peer-credential auth on the wire.
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        if let Err(e) = tokio::fs::set_permissions(parent, perms).await {
            tracing::warn!(
                target: "hallouminate::daemon",
                parent = %parent.display(),
                error = %e,
                "failed to set socket parent permissions; continuing with default",
            );
        }
    }
    Ok(())
}

/// Remove a leftover socket before binding, tolerating a missing file.
///
/// A `NotFound` error is the common, benign case (no prior daemon) and is
/// silently ignored. Any other error — typically `PermissionDenied` — is
/// logged at `warn`: it leaves the stale socket in place, so the subsequent
/// `bind` fails with a confusing `EADDRINUSE`, and the log is the only breadcrumb
/// pointing at the real (permissions) cause.
async fn remove_stale_socket(socket_path: &Path) {
    if let Err(e) = tokio::fs::remove_file(socket_path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            target: "hallouminate::daemon",
            socket = %socket_path.display(),
            error = %e,
            "failed to remove stale socket before bind; bind may fail with address-in-use",
        );
    }
}

/// Public for tests: drive the accept loop against an already-opened
/// `DaemonState` and a known socket path. The accept loop breaks when
/// `state.shutdown_token()` is cancelled — the IPC `Shutdown` request
/// cancels that token, so `serve` returns once shutdown is requested (or
/// on an unrecoverable bind error). After the loop breaks, the caller runs
/// cleanup: dropping the single-instance flock and removing the socket.
pub async fn serve(state: &DaemonState, socket_path: &Path) -> anyhow::Result<()> {
    serve_with_idle_timeout(state, socket_path, IDLE_READ_TIMEOUT).await
}

/// Same as [`serve`], but with an explicit per-connection idle-read
/// timeout instead of the production [`IDLE_READ_TIMEOUT`] default. Public
/// so integration tests can exercise the timeout behavior without waiting
/// out the real 30s default.
pub async fn serve_with_idle_timeout(
    state: &DaemonState,
    socket_path: &Path,
    idle_timeout: Duration,
) -> anyhow::Result<()> {
    prepare_socket_dir(socket_path).await?;
    let lock_path = lock_path_for(socket_path);
    let lock = acquire_single_instance(&lock_path)?;
    // Stale socket cleanup. If a previous daemon crashed without removing
    // its socket, the next bind would fail with EADDRINUSE. Holding the
    // flock above guarantees only one daemon is alive, so removing the
    // socket here is safe.
    remove_stale_socket(socket_path).await;
    let watcher = super::watch::spawn_corpus_watcher(state);
    spawn_idle_exit(state, state.baseline().daemon.idle_exit_secs);
    let result = serve_on_listener(state, socket_path, idle_timeout).await;
    drop(watcher);
    cleanup(lock, socket_path).await;
    result
}

async fn serve_on_listener(
    state: &DaemonState,
    socket_path: &Path,
    idle_timeout: Duration,
) -> anyhow::Result<()> {
    let listener = UnixListener::bind(socket_path).map_err(|e| {
        tracing::error!(
            target: "hallouminate::daemon",
            socket = %socket_path.display(),
            error = %e,
            "failed to bind daemon socket",
        );
        anyhow::anyhow!("bind {}: {e}", socket_path.display())
    })?;
    // Tighten the socket itself to owner-only access — belt to the parent
    // dir's 0o700 suspenders. Logged-but-ignored on failure so a tempfs
    // backend that refuses chmod doesn't crash the daemon.
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    if let Err(e) = tokio::fs::set_permissions(socket_path, perms).await {
        tracing::warn!(
            target: "hallouminate::daemon",
            socket = %socket_path.display(),
            error = %e,
            "failed to set socket permissions; continuing with default",
        );
    }
    tracing::info!(
        target: "hallouminate::daemon",
        socket = %socket_path.display(),
        "daemon listening"
    );

    let shutdown = state.shutdown_token().clone();
    loop {
        // Drain semantics (spec Curd 1 open question): cancelling the token
        // stops accepting *new* connections but does not abort in-flight
        // `handle_connection` tasks — they were spawned detached and finish
        // their one-shot request/response on their own. The cancel only
        // breaks this accept loop, after which the caller runs cleanup.
        let (stream, _addr) = tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!(target: "hallouminate::daemon", "shutdown requested; stopping accept loop");
                break;
            }
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(target: "hallouminate::daemon", error = %e, "accept error");
                    continue;
                }
            },
        };
        let state = state.clone();
        let conn = state.enter_connection();
        tokio::spawn(async move {
            // Held for the handler's lifetime; decrements the active-connection
            // count on drop so idle-exit never fires mid-request (ADR-003).
            let _conn = conn;
            if let Err(e) = handle_connection(state, stream, idle_timeout).await {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    error = %e,
                    "connection handler errored"
                );
            }
        });
    }
    Ok(())
}

async fn handle_connection(
    state: DaemonState,
    stream: UnixStream,
    idle_timeout: Duration,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = match tokio::time::timeout(idle_timeout, reader.read_line(&mut line)).await {
        Ok(res) => res?,
        Err(_) => {
            tracing::debug!(
                target: "hallouminate::daemon",
                timeout_secs = idle_timeout.as_secs_f64(),
                "connection idle timeout waiting for request line; closing",
            );
            return Ok(());
        }
    };
    if n == 0 {
        return Ok(());
    }
    let response = match serde_json::from_str::<DaemonRequest>(line.trim_end()) {
        Ok(req) => dispatch(&state, req).await,
        Err(e) => DaemonResponse::invalid_params(format!("invalid request: {e}")),
    };
    // Request completed; stamp the activity clock so idle-exit keys on real
    // request throughput, not just embed use (ADR-003).
    state.touch_activity();
    let mut text = serde_json::to_string(&response)?;
    text.push('\n');
    let write_result = tokio::time::timeout(idle_timeout, async {
        write_half.write_all(text.as_bytes()).await?;
        write_half.flush().await
    })
    .await;
    match write_result {
        Ok(res) => res?,
        Err(_) => {
            tracing::debug!(
                target: "hallouminate::daemon",
                timeout_secs = idle_timeout.as_secs_f64(),
                "connection idle timeout writing response; closing",
            );
        }
    }
    Ok(())
}

fn lock_path_for(socket_path: &Path) -> PathBuf {
    let mut s = socket_path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// Take a non-blocking advisory lock on the lockfile next to the socket.
/// Returns the open file; closing the fd releases the advisory lock
/// (POSIX). A second daemon on the same socket bounces with `EWOULDBLOCK`
/// and surfaces a clear "daemon already running" error.
fn acquire_single_instance(lock_path: &Path) -> anyhow::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    use rustix::fs::{FlockOperation, flock};

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .map_err(|e| anyhow::anyhow!("open lockfile {}: {e}", lock_path.display()))?;
    if let Err(errno) = flock(&file, FlockOperation::NonBlockingLockExclusive) {
        return Err(anyhow::anyhow!(
            "another hallouminate daemon already holds {} ({})",
            lock_path.display(),
            std::io::Error::from(errno)
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn lock_path_appends_dot_lock_suffix() {
        let sock = PathBuf::from("/tmp/hallouminate/daemon.sock");
        assert_eq!(
            lock_path_for(&sock),
            PathBuf::from("/tmp/hallouminate/daemon.sock.lock"),
        );
    }

    // A missing socket is the normal first-boot case: pre-bind cleanup must
    // treat `NotFound` as success, never an error, so the boot path proceeds
    // straight to `bind`.
    #[tokio::test]
    async fn remove_stale_socket_tolerates_missing_file() {
        let dir = std::env::temp_dir().join(format!("hallouminate-test-{}", std::process::id()));
        let missing = dir.join("never-existed.sock");
        assert!(!missing.exists());
        // Returns without panicking; the `NotFound` branch is the silent path.
        remove_stale_socket(&missing).await;
        assert!(!missing.exists());
    }

    // When a prior daemon left a socket behind, pre-bind cleanup must actually
    // unlink it — otherwise the later `bind` fails with EADDRINUSE.
    #[tokio::test]
    async fn remove_stale_socket_unlinks_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "hallouminate-test-{}-{}",
            std::process::id(),
            "stale"
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let stale = dir.join("daemon.sock");
        std::fs::write(&stale, b"").expect("create stale socket stand-in");
        assert!(stale.exists());
        remove_stale_socket(&stale).await;
        assert!(!stale.exists(), "stale socket must be removed before bind");
        std::fs::remove_dir_all(&dir).ok();
    }
}
