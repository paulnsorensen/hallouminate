//! Cross-platform local-socket transport for the daemon control channel.
//!
//! One bind/connect/accept seam over `interprocess::local_socket`, with the
//! Windows arm dropping to `os::windows::named_pipe` so the pipe's DACL can be
//! locked down at creation time. The two platforms diverge in exactly two
//! places — the *bind* (named pipes need an owner-only security descriptor that
//! `local_socket::ListenerOptions` does not expose) and stale cleanup (a unix
//! socket is a real file to unlink; a named pipe is not) — and share everything
//! else: the JSON-line framing, the connection handler, and the client.
//!
//! ## Endpoint model
//!
//! The lockfile stays a real filesystem path on both platforms (it anchors the
//! single-instance lock in `server.rs`). On unix the socket is that same kind of
//! path; on Windows the pipe name is *derived* from it — a stable `\\.\pipe\`
//! name keyed off the path so two daemons targeting different sockets get
//! different pipes, and the `HALLOUMINATE_SOCKET` test override still selects a
//! per-test endpoint.
//!
//! ## Connection handler
//!
//! `handle_connection` is generic over `AsyncRead + AsyncWrite + Unpin`, so the
//! unix `local_socket::tokio::Stream` and the Windows
//! `DuplexPipeStream<pipe_mode::Bytes>` flow through the same one-shot
//! read-line → dispatch → write-line path. The framing is byte-identical to the
//! pre-port `tokio::net::UnixStream` transport.

use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use super::dispatch::dispatch;
use super::ipc::{DaemonRequest, DaemonResponse};
use super::state::DaemonState;

/// A resolved daemon endpoint. On both platforms it carries the lockfile-anchor
/// path; on Windows the derived pipe name is computed from it at bind/connect
/// time. Keeping the `PathBuf` (rather than an `interprocess` `Name<'_>`) avoids
/// a self-referential borrow — `Name<'_>` borrows its backing string, so it is
/// rebuilt at each use from this owned path.
#[derive(Debug, Clone)]
pub struct Endpoint {
    path: PathBuf,
}

/// Resolve the transport endpoint from the daemon socket path. The path comes
/// from `daemon_socket_path()` (honoring `HALLOUMINATE_SOCKET`); on Windows the
/// pipe name is derived from it, on unix it *is* the socket file.
pub fn daemon_endpoint(socket_path: &Path) -> Endpoint {
    Endpoint {
        path: socket_path.to_path_buf(),
    }
}

/// The Windows named-pipe path derived from the socket path: `\\.\pipe\` plus a
/// stable, filesystem-safe encoding of the full socket path. Encoding the whole
/// path (not just the file name) keeps two daemons on different sockets — and
/// the per-test `HALLOUMINATE_SOCKET` overrides — on distinct pipes.
#[cfg(windows)]
fn pipe_path(socket_path: &Path) -> String {
    // Pipe names cannot contain backslashes (the namespace separator) and are
    // case-insensitive; map the path's non-alphanumeric bytes to `_` so any
    // socket path yields one legal, collision-resistant pipe leaf.
    let raw = socket_path.to_string_lossy();
    let leaf: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!(r"\\.\pipe\hallouminate-{leaf}")
}

/// Owner-only DACL for the named pipe, set at creation (no TOCTOU window).
///
/// A default named-pipe security descriptor grants READ to Everyone and the
/// Anonymous account (MS Learn, "Named Pipe Security and Access Rights"), which
/// would expose this JSON-RPC control channel to any local principal. This SDDL
/// instead grants GENERIC_ALL to only the creator (`CO`), SYSTEM (`SY`), and the
/// built-in Administrators group (`BA`), with a protected DACL (`P`) so no
/// inherited ACE re-widens it. `CO` (Creator Owner, `S-1-3-0`) — not `OW`
/// (Owner Rights, `S-1-3-4`) — is the SID that resolves to the creating
/// principal at pipe-creation time.
#[cfg(windows)]
const OWNER_ONLY_SDDL: &str = "D:P(A;;GA;;;CO)(A;;GA;;;SY)(A;;GA;;;BA)";

/// Bind the listener and serve connections until `shutdown` is cancelled.
///
/// The accept loop is cfg-split because the two backends have no common
/// listener trait, but each arm spawns the same generic [`handle_connection`]
/// per accepted stream. Returns once the shutdown token fires (the IPC
/// `Shutdown` request cancels it) or on an unrecoverable bind error.
pub async fn serve_connections(
    state: &DaemonState,
    endpoint: &Endpoint,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        serve_unix(state, endpoint, shutdown).await
    }
    #[cfg(not(unix))]
    {
        serve_windows(state, endpoint, shutdown).await
    }
}

#[cfg(unix)]
async fn serve_unix(
    state: &DaemonState,
    endpoint: &Endpoint,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    use interprocess::local_socket::ListenerOptions;
    use interprocess::local_socket::traits::tokio::Listener as _;
    use interprocess::local_socket::{GenericFilePath, ToFsName};

    let name = endpoint
        .path
        .as_path()
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| anyhow::anyhow!("resolve socket name {}: {e}", endpoint.path.display()))?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .map_err(|e| {
            tracing::error!(
                target: "hallouminate::daemon",
                socket = %endpoint.path.display(),
                error = %e,
                "failed to bind daemon socket",
            );
            anyhow::anyhow!("bind {}: {e}", endpoint.path.display())
        })?;
    // Tighten the socket file to owner-only (0o600) after bind — the unix
    // equivalent of the Windows named-pipe DACL. We chmod the bound file
    // directly rather than via `ListenerOptions::mode()`: interprocess's
    // `.mode()` returns `Unsupported` on macOS, and chmod-after-bind is what the
    // pre-port transport did anyway. Logged-but-ignored on failure so a tmpfs
    // backend that refuses chmod doesn't crash the daemon (the parent dir's
    // 0o700 from `server.rs` is the primary owner-only boundary).
    set_socket_owner_only(&endpoint.path).await;
    tracing::info!(
        target: "hallouminate::daemon",
        socket = %endpoint.path.display(),
        "daemon listening"
    );
    loop {
        let stream = tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!(target: "hallouminate::daemon", "shutdown requested; stopping accept loop");
                break;
            }
            accepted = listener.accept() => match accepted {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "hallouminate::daemon", error = %e, "accept error");
                    continue;
                }
            },
        };
        spawn_handler(state.clone(), stream);
    }
    Ok(())
}

#[cfg(not(unix))]
async fn serve_windows(
    state: &DaemonState,
    endpoint: &Endpoint,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    // `PipeListener::accept` is an inherent method — no trait import needed
    // (there is no `PipeListenerExt` in this module; the only trait here is the
    // deprecated `PipeListenerOptionsExt`, which we deliberately avoid).
    use interprocess::os::windows::named_pipe::{PipeListenerOptions, pipe_mode};
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::U16CString;

    let path = pipe_path(&endpoint.path);
    let sddl = U16CString::from_str(OWNER_ONLY_SDDL)
        .map_err(|e| anyhow::anyhow!("encode owner-only SDDL: {e}"))?;
    let sd = SecurityDescriptor::deserialize(&sddl)
        .map_err(|e| anyhow::anyhow!("build owner-only security descriptor: {e}"))?;

    let listener = PipeListenerOptions::new()
        .path(Path::new(&path))
        .security_descriptor(Some(sd))
        .create_tokio_duplex::<pipe_mode::Bytes>()
        .map_err(|e| {
            tracing::error!(
                target: "hallouminate::daemon",
                pipe = %path,
                error = %e,
                "failed to bind daemon named pipe",
            );
            anyhow::anyhow!("bind {path}: {e}")
        })?;
    tracing::info!(
        target: "hallouminate::daemon",
        pipe = %path,
        "daemon listening"
    );
    loop {
        let stream = tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!(target: "hallouminate::daemon", "shutdown requested; stopping accept loop");
                break;
            }
            accepted = listener.accept() => match accepted {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "hallouminate::daemon", error = %e, "accept error");
                    continue;
                }
            },
        };
        spawn_handler(state.clone(), stream);
    }
    Ok(())
}

/// Spawn the detached one-shot handler for an accepted connection. Detached on
/// purpose: cancelling the shutdown token stops accepting *new* connections but
/// lets in-flight request/response pairs finish (matching the pre-port drain
/// semantics).
fn spawn_handler<S>(state: DaemonState, stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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

/// One-shot request/response over a single connection: read one JSON line,
/// dispatch, write one JSON line, close. Generic over the stream type so the
/// unix socket and the Windows named pipe share one path.
async fn handle_connection<S>(state: DaemonState, stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
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

/// A boxed connected client stream — uniform across platforms so `client.rs`,
/// `bootstrap.rs`, and `lifecycle.rs` hold one type regardless of backend.
pub type ClientStream = Box<dyn ClientConn>;

/// The object-safe bound a connected client stream satisfies. Both the unix
/// `local_socket::tokio::Stream` and the Windows `DuplexPipeStream` implement
/// `AsyncRead + AsyncWrite`; this trait erases the concrete type.
pub trait ClientConn: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ClientConn for T {}

/// Open one client connection to the daemon endpoint, boxed to a uniform
/// `ClientStream`. Cfg-split because the DACL forces the Windows server onto
/// `os::windows::named_pipe`, so the client connects by pipe path
/// (`DuplexPipeStream::connect_by_path`) rather than through the `local_socket`
/// `Stream::connect` the unix arm uses. Both yield an `AsyncRead + AsyncWrite`.
pub async fn connect(endpoint: &Endpoint) -> std::io::Result<ClientStream> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::tokio::Stream;
        use interprocess::local_socket::traits::tokio::Stream as _;
        use interprocess::local_socket::{GenericFilePath, ToFsName};

        let name = endpoint.path.as_path().to_fs_name::<GenericFilePath>()?;
        let stream = Stream::connect(name).await?;
        Ok(Box::new(stream))
    }
    #[cfg(not(unix))]
    {
        use interprocess::os::windows::named_pipe::pipe_mode;
        use interprocess::os::windows::named_pipe::tokio::DuplexPipeStream;

        let path = pipe_path(&endpoint.path);
        let stream =
            DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path(Path::new(&path)).await?;
        Ok(Box::new(stream))
    }
}

/// Liveness probe: a daemon is live iff a client connect succeeds. Replaces the
/// unix-only `socket.exists()` file check, which has no meaning for a named pipe
/// (there is no on-disk file) and is racy even on unix (a stale socket file can
/// exist with no listener).
pub async fn is_live(endpoint: &Endpoint) -> bool {
    connect(endpoint).await.is_ok()
}

/// Remove a stale endpoint before binding. On unix this unlinks a leftover
/// socket file (a crashed daemon leaves one, and the next `bind` would fail
/// with `EADDRINUSE`). On Windows a named pipe has no on-disk artifact — the
/// last handle closing frees the name — so this is a no-op + audit log.
pub async fn remove_stale(endpoint: &Endpoint) {
    #[cfg(unix)]
    {
        if let Err(e) = tokio::fs::remove_file(&endpoint.path).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                target: "hallouminate::daemon",
                socket = %endpoint.path.display(),
                error = %e,
                "failed to remove stale socket before bind; bind may fail with address-in-use",
            );
        }
    }
    #[cfg(not(unix))]
    {
        // Named pipes are freed when the last handle closes; there is nothing to
        // unlink. Logged so the platform delegation is auditable.
        tracing::debug!(
            target: "hallouminate::daemon",
            endpoint = %endpoint.path.display(),
            "remove_stale is a no-op on named pipes (no on-disk artifact)",
        );
    }
}

/// Tighten the bound unix socket file to owner-only (0o600). Best-effort: a
/// failure is logged, not fatal — the socket parent dir's 0o700 (set in
/// `server.rs`) is the primary access boundary, and some backends refuse chmod.
#[cfg(unix)]
async fn set_socket_owner_only(socket_path: &Path) {
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
}

/// Send one request and read one response over a fresh connection. Shared by
/// every client call site (the daemon protocol is one-shot per connection).
pub async fn round_trip(
    mut stream: ClientStream,
    req: &DaemonRequest,
) -> std::io::Result<Option<String>> {
    let mut text = serde_json::to_string(req)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    text.push('\n');
    stream.write_all(text.as_bytes()).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_carries_the_socket_path() {
        let p = Path::new("/tmp/hallouminate/daemon.sock");
        let ep = daemon_endpoint(p);
        assert_eq!(ep.path, p);
    }

    // Bind a real listener, connect through `connect`, and assert the unix
    // socket file lands at 0o600. Guards the transport's core contract (the
    // accept/connect path works) and the owner-only chmod that replaced
    // interprocess's macOS-unsupported `.mode()`.
    #[cfg(unix)]
    #[tokio::test]
    async fn unix_bind_sets_socket_to_owner_only_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;

        use interprocess::local_socket::ListenerOptions;
        use interprocess::local_socket::traits::tokio::Listener as _;
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        use tokio::io::AsyncReadExt;

        let dir = std::env::temp_dir().join(format!("ipc-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("daemon.sock");
        let endpoint = daemon_endpoint(&sock);

        let name = sock.as_path().to_fs_name::<GenericFilePath>().unwrap();
        let listener = ListenerOptions::new()
            .name(name)
            .create_tokio()
            .expect("bind must succeed");
        set_socket_owner_only(&sock).await;

        // 0o600: owner read/write only — the unix owner-only guarantee.
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be owner-only after bind");

        // A client connects and the listener accepts — the transport's spine.
        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            conn.read_to_end(&mut buf).await.ok();
        });
        let client = connect(&endpoint).await.expect("client connect");
        drop(client); // closing the client EOFs the server read
        let _ = server.await;

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn pipe_path_is_prefixed_and_sanitized() {
        // Backslashes and the drive colon must not survive into the pipe leaf —
        // they would break the `\\.\pipe\` namespace. Every non-alphanumeric
        // byte maps to `_`, and the result is prefixed.
        let p = Path::new(r"C:\Users\me\AppData\daemon.sock");
        let got = pipe_path(p);
        assert!(got.starts_with(r"\\.\pipe\hallouminate-"));
        let leaf = got.trim_start_matches(r"\\.\pipe\hallouminate-");
        assert!(
            leaf.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "pipe leaf must be namespace-safe: {leaf}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn distinct_socket_paths_yield_distinct_pipes() {
        // Two daemons on different sockets (or two HALLOUMINATE_SOCKET test
        // overrides) must not collide on one pipe name.
        let a = pipe_path(Path::new(r"C:\a\daemon.sock"));
        let b = pipe_path(Path::new(r"C:\b\daemon.sock"));
        assert_ne!(a, b);
    }
}
