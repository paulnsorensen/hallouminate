//! Daemon accept loop.
//!
//! `run_daemon` binds the configured socket, takes a single-instance lock
//! via `flock`, and dispatches one request per connection. The protocol is
//! intentionally minimal: read one JSON line, write one JSON line, close.
//! Per-corpus serialization and the global write-lane live in
//! `dispatch::dispatch`; the accept loop is only responsible for surfacing
//! framing/IO errors.

use std::os::fd::AsRawFd;
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
    let cfg = config::load(args.config.as_deref())?;
    let socket_path = daemon_socket_path();
    serve_with_config(cfg, &socket_path).await
}

/// Production wiring that takes the lock first, *then* opens the LanceDB
/// handle and model. Critical for the single-instance invariant: a second
/// daemon launched against the same socket must never briefly co-own the
/// ground directory before failing on the lock — that's exactly the
/// multi-process LanceDB race the daemon exists to prevent.
async fn serve_with_config(cfg: Config, socket_path: &Path) -> anyhow::Result<()> {
    prepare_socket_dir(socket_path).await?;
    let lock_path = lock_path_for(socket_path);
    let _lock = acquire_single_instance(&lock_path)?;
    let state = DaemonState::open(cfg).await?;
    let _ = tokio::fs::remove_file(socket_path).await;
    serve_on_listener(&state, socket_path).await
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
/// `DaemonState` and a known socket path. Tests inject shutdown via an
/// external `tokio::select!` racing this future against an oneshot, which
/// is why `serve` returns only on an unrecoverable IO error in production
/// — the loop has no built-in shutdown signal.
pub async fn serve(state: &DaemonState, socket_path: &Path) -> anyhow::Result<()> {
    prepare_socket_dir(socket_path).await?;
    let lock_path = lock_path_for(socket_path);
    let _lock = acquire_single_instance(&lock_path)?;
    // Stale socket cleanup. If a previous daemon crashed without removing
    // its socket, the next bind would fail with EADDRINUSE. Holding the
    // flock above guarantees only one daemon is alive, so removing the
    // socket here is safe.
    let _ = tokio::fs::remove_file(socket_path).await;
    serve_on_listener(state, socket_path).await
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

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(target: "hallouminate::daemon", error = %e, "accept error");
                continue;
            }
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

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .map_err(|e| anyhow::anyhow!("open lockfile {}: {e}", lock_path.display()))?;
    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(anyhow::anyhow!(
            "another hallouminate daemon already holds {} ({})",
            lock_path.display(),
            err
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
