//! Daemon accept loop.
//!
//! `run_daemon` binds the configured endpoint, takes a single-instance lock via
//! `std::fs::File::try_lock`, and dispatches one request per connection. The
//! protocol is intentionally minimal: read one JSON line, write one JSON line,
//! close. Per-corpus serialization and the global write-lane live in
//! `dispatch::dispatch`; the accept loop (in `transport`) is only responsible
//! for surfacing framing/IO errors.
//!
//! The transport itself (bind/accept/connect, plus the owner-only Windows pipe
//! DACL) lives in `transport.rs`; this module owns the lock, the socket-dir
//! permissions, the signal wiring, and the boot ordering.

use std::path::{Path, PathBuf};

use crate::app::config::{self, Config};

use super::socket::daemon_socket_path;
use super::state::DaemonState;
use super::transport::{self, daemon_endpoint};

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
    let endpoint = daemon_endpoint(socket_path);
    transport::remove_stale(&endpoint).await;
    let watcher = super::watch::spawn_corpus_watcher(&state);
    spawn_signal_handlers(&state);
    let result = transport::serve_connections(&state, &endpoint, state.shutdown_token()).await;
    drop(watcher);
    cleanup(lock, &endpoint).await;
    result
}

/// Wire SIGINT and SIGTERM (unix) / Ctrl-C, Ctrl-Break, Ctrl-Close (windows)
/// onto the daemon's shutdown token so a `kill` (or a console signal) drains the
/// accept loop and runs the same lock-drop + socket-removal cleanup as the IPC
/// `Shutdown` request, rather than dying on default signal disposition and
/// leaving a stale socket.
///
/// On unix the SIGTERM stream is registered **synchronously** (before the
/// function returns), so on return the process's default-terminate disposition
/// is already overridden — a `kill -TERM` after this returns reaches the token,
/// not the default killer. This synchronous postcondition is what the SIGTERM
/// integration test relies on to raise the signal without a spawn race.
///
/// Production stop on every platform is the IPC `Shutdown` request (already
/// cross-platform); these signal handlers are the foreground convenience.
#[cfg(unix)]
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

/// Windows signal wiring: Ctrl-C, Ctrl-Break, and the console-close control
/// event all drain the accept loop through the shutdown token. There is no
/// SIGTERM-equivalent default-disposition race to win here (the production stop
/// path is the IPC `Shutdown` request regardless), so this is registered
/// asynchronously inside the spawned task.
#[cfg(not(unix))]
pub fn spawn_signal_handlers(state: &DaemonState) {
    let token = state.shutdown_token().clone();
    tokio::spawn(async move {
        let mut ctrl_break = match tokio::signal::windows::ctrl_break() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "hallouminate::daemon", error = %e, "failed to install Ctrl-Break handler");
                return;
            }
        };
        let mut ctrl_close = match tokio::signal::windows::ctrl_close() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "hallouminate::daemon", error = %e, "failed to install Ctrl-Close handler");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!(target: "hallouminate::daemon", "received Ctrl-C; shutting down");
            }
            _ = ctrl_break.recv() => {
                tracing::info!(target: "hallouminate::daemon", "received Ctrl-Break; shutting down");
            }
            _ = ctrl_close.recv() => {
                tracing::info!(target: "hallouminate::daemon", "received Ctrl-Close; shutting down");
            }
        }
        token.cancel();
    });
}

/// Release the single-instance lock and remove the endpoint so the next boot
/// binds cleanly. Dropping the `File` releases the advisory lock; we remove the
/// socket after so a client racing a reconnect sees it gone rather than a
/// dead-but-present file. On Windows `remove_stale` is a no-op (no on-disk
/// pipe).
async fn cleanup(lock: std::fs::File, endpoint: &transport::Endpoint) {
    transport::remove_stale(endpoint).await;
    drop(lock);
}

async fn prepare_socket_dir(socket_path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| anyhow::anyhow!("create socket parent dir {}: {e}", parent.display()))?;
        set_dir_owner_only(parent).await;
    }
    Ok(())
}

/// Tighten the socket parent dir to owner-only (0o700) on unix. Without this,
/// another local user on a shared machine could traverse the parent dir,
/// connect to the socket, and issue mutating requests — the daemon has no
/// peer-credential auth on the wire.
///
/// On Windows the owner-only guarantee lives on the named-pipe DACL set at
/// creation (`transport.rs`, Decision D), not on a directory mode — the pipe
/// is not an on-disk file under this directory. This arm is a no-op + debug log
/// so the delegation is auditable.
#[cfg(unix)]
async fn set_dir_owner_only(parent: &Path) {
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

#[cfg(not(unix))]
async fn set_dir_owner_only(parent: &Path) {
    tracing::debug!(
        target: "hallouminate::daemon",
        parent = %parent.display(),
        "socket-dir chmod is a no-op on this platform; owner-only access is \
         enforced by the named-pipe DACL set at creation",
    );
}

/// Public for tests: drive the accept loop against an already-opened
/// `DaemonState` and a known socket path. The accept loop breaks when
/// `state.shutdown_token()` is cancelled — the IPC `Shutdown` request
/// cancels that token, so `serve` returns once shutdown is requested (or
/// on an unrecoverable bind error). After the loop breaks, the caller runs
/// cleanup: dropping the single-instance lock and removing the socket.
pub async fn serve(state: &DaemonState, socket_path: &Path) -> anyhow::Result<()> {
    prepare_socket_dir(socket_path).await?;
    let lock_path = lock_path_for(socket_path);
    let lock = acquire_single_instance(&lock_path)?;
    let endpoint = daemon_endpoint(socket_path);
    // Stale endpoint cleanup. If a previous daemon crashed without removing its
    // socket, the next bind would fail with EADDRINUSE on unix. Holding the lock
    // above guarantees only one daemon is alive, so removing it here is safe.
    transport::remove_stale(&endpoint).await;
    let watcher = super::watch::spawn_corpus_watcher(state);
    let result = transport::serve_connections(state, &endpoint, state.shutdown_token()).await;
    drop(watcher);
    cleanup(lock, &endpoint).await;
    result
}

fn lock_path_for(socket_path: &Path) -> PathBuf {
    let mut s = socket_path.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// Take a non-blocking exclusive lock on the lockfile next to the socket.
/// Returns the open file; closing it (dropping the `File`) releases the lock.
/// A second daemon on the same socket bounces with a clear "daemon already
/// running" error.
///
/// `std::fs::File::try_lock` (stable since 1.89) maps to `flock(LOCK_EX |
/// LOCK_NB)` on unix and `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK |
/// LOCKFILE_FAIL_IMMEDIATELY)` on Windows — an exact, cross-platform,
/// zero-dependency match for the prior rustix `flock` call. The `.read(true)
/// .write(true)` open satisfies std's "not append-only" Windows requirement.
fn acquire_single_instance(lock_path: &Path) -> anyhow::Result<std::fs::File> {
    use std::fs::{OpenOptions, TryLockError};

    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    // Owner-only lockfile on unix; on Windows the lockfile is a plain file under
    // the user's runtime dir and the named-pipe DACL is the access boundary.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(lock_path)
        .map_err(|e| anyhow::anyhow!("open lockfile {}: {e}", lock_path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(anyhow::anyhow!(
            "another hallouminate daemon already holds {}",
            lock_path.display(),
        )),
        Err(TryLockError::Error(e)) => Err(anyhow::anyhow!("lock {}: {e}", lock_path.display(),)),
    }
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

    // A missing endpoint is the normal first-boot case: pre-bind cleanup must
    // not error, so the boot path proceeds straight to `bind`.
    #[tokio::test]
    async fn remove_stale_tolerates_missing_endpoint() {
        let dir = std::env::temp_dir().join(format!("hallouminate-test-{}", std::process::id()));
        let missing = dir.join("never-existed.sock");
        let endpoint = daemon_endpoint(&missing);
        // Returns without panicking; the missing/no-op branch is the silent path.
        transport::remove_stale(&endpoint).await;
    }

    // The single-instance lock must be exclusive: a second `try_lock` on the
    // same path while the first file is held bounces with WouldBlock, which is
    // exactly the "daemon already running" signal.
    #[test]
    fn acquire_single_instance_is_exclusive() {
        let dir = std::env::temp_dir().join(format!(
            "hallouminate-lock-{}-{}",
            std::process::id(),
            "excl"
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let lock_path = dir.join("daemon.sock.lock");
        let first = acquire_single_instance(&lock_path).expect("first lock acquires");
        let second = acquire_single_instance(&lock_path);
        assert!(
            second.is_err(),
            "a second daemon must not acquire the lock the first holds"
        );
        assert!(
            second.unwrap_err().to_string().contains("already holds"),
            "the bounce must name the already-running daemon"
        );
        drop(first);
        std::fs::remove_dir_all(&dir).ok();
    }
}
