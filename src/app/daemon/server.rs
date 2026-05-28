//! Daemon accept loop.
//!
//! `run_daemon` binds the configured socket, takes a single-instance lock
//! via `flock`, and dispatches one request per connection. The protocol is
//! intentionally minimal: read one JSON line, write one JSON line, close.
//! Per-corpus serialization and the global write-lane live in
//! `dispatch::dispatch`; the accept loop is only responsible for surfacing
//! framing/IO errors.

use std::path::{Path, PathBuf};

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
    let _ = tokio::fs::remove_file(socket_path).await;
    let watcher = super::watch::spawn_corpus_watcher(&state);
    spawn_signal_handlers(&state);
    let result = serve_on_listener(&state, socket_path).await;
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

/// Release the single-instance flock and remove the socket file so the next
/// boot binds cleanly. Dropping the `File` releases the advisory lock (POSIX);
/// we remove the socket after so a client racing a reconnect sees the socket
/// gone rather than a dead-but-present file.
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

/// Public for tests: drive the accept loop against an already-opened
/// `DaemonState` and a known socket path. The accept loop breaks when
/// `state.shutdown_token()` is cancelled — the IPC `Shutdown` request
/// cancels that token, so `serve` returns once shutdown is requested (or
/// on an unrecoverable bind error). After the loop breaks, the caller runs
/// cleanup: dropping the single-instance flock and removing the socket.
pub async fn serve(state: &DaemonState, socket_path: &Path) -> anyhow::Result<()> {
    prepare_socket_dir(socket_path).await?;
    let lock_path = lock_path_for(socket_path);
    let lock = acquire_single_instance(&lock_path)?;
    // Stale socket cleanup. If a previous daemon crashed without removing
    // its socket, the next bind would fail with EADDRINUSE. Holding the
    // flock above guarantees only one daemon is alive, so removing the
    // socket here is safe.
    let _ = tokio::fs::remove_file(socket_path).await;
    let watcher = super::watch::spawn_corpus_watcher(state);
    let result = serve_on_listener(state, socket_path).await;
    drop(watcher);
    cleanup(lock, socket_path).await;
    result
}

async fn serve_on_listener(state: &DaemonState, socket_path: &Path) -> anyhow::Result<()> {
    let listener = UnixListener::bind(socket_path)
        .map_err(|e| anyhow::anyhow!("bind {}: {e}", socket_path.display()))?;
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
    eprintln!("hallouminate daemon listening on {}", socket_path.display());

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
        tokio::spawn(async move {
            if let Err(e) = handle_connection(state, stream).await {
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

async fn handle_connection(state: DaemonState, stream: UnixStream) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }
    let response = match serde_json::from_str::<DaemonRequest>(line.trim_end()) {
        Ok(req) => dispatch(&state, req).await,
        Err(e) => DaemonResponse::invalid_params(format!("invalid request: {e}")),
    };
    let mut text = serde_json::to_string(&response)?;
    text.push('\n');
    write_half.write_all(text.as_bytes()).await?;
    write_half.flush().await?;
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
}
