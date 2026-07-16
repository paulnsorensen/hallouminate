//! Daemon RPC client.
//!
//! `DaemonClient::connect` resolves the socket path from
//! `daemon_socket_path()` (test-overridable via `HALLOUMINATE_SOCKET`) and
//! returns a clear `daemon unavailable` error when the socket is missing or
//! unreachable. Callers that fall back to a non-daemon path do so
//! explicitly; the client never auto-starts a daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::bootstrap::ensure_daemon_running;
use super::ipc::{DaemonRequest, DaemonRequestPayload, DaemonResponse, ErrorKind};
use super::socket::daemon_socket_path;

/// Client handle: just remembers which socket path to dial. Stateless
/// otherwise — every `call` opens a fresh connection.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
}

/// Connect to the daemon. Returns `Err` with a clear "daemon unavailable"
/// message when the socket is missing, unreadable, or the connect fails.
pub async fn daemon_client() -> anyhow::Result<DaemonClient> {
    let socket = daemon_socket_path();
    connect_at(&socket).await
}

/// Connect to the daemon at an explicit socket path when set, otherwise
/// resolve via `daemon_socket_path()` (which honors `HALLOUMINATE_SOCKET`).
/// One canonical entry point for CLI / MCP callers so the `--socket` flag
/// path and the env-var / default path go through the same client builder.
pub async fn client_for(socket: Option<&Path>) -> anyhow::Result<DaemonClient> {
    client_for_with(socket, ensure_daemon_running).await
}

/// `client_for` with an injectable respawn step — the test seam behind it,
/// mirroring [`super::lifecycle::restart_with`]. Production passes
/// `ensure_daemon_running` (which no-ops under `HALLOUMINATE_SOCKET`). Only the
/// default-socket path (`None`) self-heals: on connect failure it runs
/// `respawn` once and retries the connect; a second failure returns the loud
/// "daemon unavailable" error. Explicit-socket callers (`Some(path)`) —
/// `lifecycle::status`/`stop` and test harnesses — never spawn: `stop` must not
/// resurrect what it stopped (ADR-002).
pub async fn client_for_with<F, Fut>(
    socket: Option<&Path>,
    respawn: F,
) -> anyhow::Result<DaemonClient>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    match socket {
        Some(path) => connect_at(path).await,
        None => match daemon_client().await {
            Ok(c) => Ok(c),
            Err(_) => {
                respawn().await?;
                daemon_client().await
            }
        },
    }
}

/// Wrap an arbitrary error as a "daemon unavailable" `anyhow::Error` whose
/// message mirrors `connect_at`'s shape — for callers that need to surface
/// the daemon-down hint from a path that already produced its own error.
/// Kept as a small helper rather than open-coded so the documented hint
/// ("start it with `hallouminate daemon`") never drifts between call sites.
pub fn daemon_client_unavailable(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("daemon unavailable: {reason} (start it with `hallouminate daemon`)")
}

/// Test entry point: dial a specific socket path. Production code goes
/// through `daemon_client()` or `client_for()`.
pub async fn connect_at(socket: &Path) -> anyhow::Result<DaemonClient> {
    // Probe the socket with a quick connect to confirm a daemon is alive,
    // surfacing the failure here instead of inside the first `call`.
    UnixStream::connect(socket).await.with_context(|| {
        format!(
            "daemon unavailable: cannot connect to {} \
             (start it with `hallouminate daemon`)",
            socket.display()
        )
    })?;
    Ok(DaemonClient {
        socket: socket.to_path_buf(),
    })
}

impl DaemonClient {
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Send one request, parse one response. The daemon protocol is
    /// one-shot per connection, so each call opens a new socket.
    pub async fn call_raw(&self, req: DaemonRequest) -> anyhow::Result<DaemonResponse> {
        let mut stream = UnixStream::connect(&self.socket).await.map_err(|e| {
            daemon_client_unavailable(format!("connect to {} failed: {e}", self.socket.display()))
        })?;
        let mut text = serde_json::to_string(&req)?;
        text.push('\n');
        // Wrap mid-call I/O errors with the same `daemon unavailable` hint
        // the initial connect uses. Without this, a daemon that dies after
        // the connect succeeds (write fails, read returns EOF, response
        // truncates) surfaces as a bare I/O / JSON error and MCP/CLI
        // callers lose the actionable "start it with `hallouminate daemon`"
        // recovery suffix.
        stream.write_all(text.as_bytes()).await.map_err(|e| {
            daemon_client_unavailable(format!("write to {} failed: {e}", self.socket.display()))
        })?;
        stream.flush().await.map_err(|e| {
            daemon_client_unavailable(format!("flush {} failed: {e}", self.socket.display()))
        })?;
        let (read_half, _) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(|e| {
            daemon_client_unavailable(format!("read from {} failed: {e}", self.socket.display()))
        })?;
        if n == 0 {
            return Err(daemon_client_unavailable(format!(
                "daemon at {} closed the connection before responding",
                self.socket.display(),
            )));
        }
        let response: DaemonResponse = serde_json::from_str(line.trim_end()).map_err(|e| {
            daemon_client_unavailable(format!(
                "invalid daemon response from {}: {e} (response: {line:?})",
                self.socket.display(),
            ))
        })?;
        Ok(response)
    }

    /// [`call_raw`] with a bounded round-trip deadline. `call_raw` itself
    /// has no timeout — a daemon that accepts the connection but never
    /// writes/reads would otherwise hang the caller forever. Lifecycle
    /// commands (`stop`, `status`) that must never wait indefinitely for an
    /// accepted-but-silent socket use this instead of bare `call_raw`.
    pub async fn call_raw_with_timeout(
        &self,
        req: DaemonRequest,
        timeout: Duration,
    ) -> anyhow::Result<DaemonResponse> {
        match tokio::time::timeout(timeout, self.call_raw(req)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(daemon_client_unavailable(format!(
                "no response from {} within {}s",
                self.socket.display(),
                timeout.as_secs(),
            ))),
        }
    }

    /// Convenience wrapper: send a request and decode the `Ok` payload as
    /// `T`. Daemon-side `Err` variants surface as `anyhow::Error` with the
    /// daemon's message preserved. Bounded by [`timeout_for`]'s per-class
    /// deadline — `call_raw` itself never times out, so an unbounded `call`
    /// would hang the caller forever on a wedged daemon (issue #216).
    pub async fn call<T: DeserializeOwned>(&self, req: DaemonRequest) -> anyhow::Result<T> {
        let timeout = timeout_for(&req.payload);
        match self.call_raw_with_timeout(req, timeout).await? {
            DaemonResponse::Ok { result } => serde_json::from_value(result)
                .map_err(|e| anyhow::anyhow!("daemon returned unexpected payload: {e}")),
            DaemonResponse::Err { kind, message } => match kind {
                ErrorKind::InvalidParams => Err(DaemonRpcError::invalid_params(message).into()),
                ErrorKind::Internal => Err(DaemonRpcError::internal(message).into()),
            },
        }
    }
}

/// Per-request-class RPC deadline for [`DaemonClient::call`]. Reads
/// (listings, single-file markdown ops, corpus stats) are cheap lookups;
/// `ground` embeds the query and searches, so it gets more room; `index`
/// rebuilds a corpus and mutating writes (`add_markdown`, `delete_markdown`)
/// are long-running (many batches of disk I/O plus embedding work), so both
/// get the longest class. `Ping` and `Shutdown` aren't routed through
/// `call` in practice (lifecycle.rs calls `call_raw_with_timeout` directly
/// with its own short deadlines) but are classified for match exhaustiveness.
fn timeout_for(payload: &DaemonRequestPayload) -> Duration {
    match payload {
        DaemonRequestPayload::Ground(_) => Duration::from_secs(120),
        DaemonRequestPayload::Index(_)
        | DaemonRequestPayload::AddMarkdown(_)
        | DaemonRequestPayload::DeleteMarkdown(_) => Duration::from_secs(15 * 60),
        DaemonRequestPayload::Ping
        | DaemonRequestPayload::ListCorpora
        | DaemonRequestPayload::ListFiles(_)
        | DaemonRequestPayload::ListTree(_)
        | DaemonRequestPayload::ReadMarkdown(_)
        | DaemonRequestPayload::Backlinks(_)
        | DaemonRequestPayload::CorpusStats { .. }
        | DaemonRequestPayload::Shutdown => Duration::from_secs(60),
    }
}

/// Typed daemon error so MCP/CLI consumers can downcast and decide how to
/// surface the message (JSON-RPC error code, exit status, etc.).
#[derive(Debug)]
pub struct DaemonRpcError {
    pub kind: ErrorKind,
    pub message: String,
}

impl DaemonRpcError {
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidParams,
            message: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for DaemonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DaemonRpcError {}

#[cfg(test)]
mod tests {
    use super::super::ipc::{
        AddMarkdownRequest, BacklinksRequest, DaemonRequestPayload, DeleteMarkdownRequest,
        GroundRequest, IndexRequest, ListFilesRequest, ListTreeRequest, ReadMarkdownRequest,
    };
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn client_for_with_explicit_socket_never_respawns() {
        // AC #4: an explicit-socket caller (Some(path)) must never spawn a
        // daemon, even on connect failure — `stop`/`status` and test harnesses
        // rely on this so `stop` cannot resurrect what it stopped (ADR-002).
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("never.sock");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);
        let result = client_for_with(Some(&missing), || {
            calls_ref.fetch_add(1, Ordering::SeqCst);
            async { anyhow::Ok(()) }
        })
        .await;
        result.expect_err("connect to a missing socket must fail");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "explicit-socket path must never invoke the respawn step",
        );
    }

    #[tokio::test]
    async fn call_raw_with_timeout_returns_err_when_server_never_replies() {
        // The blocker this guards: `call_raw` has no built-in timeout, so a
        // daemon that accepts the connection but never writes a response
        // would hang the caller forever. `call_raw_with_timeout` must bound
        // the whole round trip and return promptly instead of hanging.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock_path = tmp.path().join("silent.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");
        tokio::spawn(async move {
            // Accept and hold the connection open without ever reading or
            // writing — simulates a wedged daemon.
            let (_stream, _addr) = listener.accept().await.expect("accept");
            std::future::pending::<()>().await;
        });

        let client = connect_at(&sock_path).await.expect("connect");
        let started = std::time::Instant::now();
        let result = client
            .call_raw_with_timeout(
                DaemonRequest {
                    cwd: PathBuf::from("."),
                    payload: DaemonRequestPayload::Ping,
                },
                Duration::from_millis(100),
            )
            .await;
        result.expect_err("a silent server must time out, not hang");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "call_raw_with_timeout must not block past its deadline",
        );
    }

    #[test]
    fn timeout_for_classifies_by_request_class() {
        // #216: `call<T>` must bound every RPC class, not just lifecycle
        // status/stop. Reads stay short; `ground` gets more room for
        // embedding + search; `index` and single-file mutations get the
        // longest class because both are long-running bulk operations.
        let read_class = Duration::from_secs(60);
        let ground_class = Duration::from_secs(120);
        let mutation_class = Duration::from_secs(15 * 60);

        assert_eq!(timeout_for(&DaemonRequestPayload::Ping), read_class);
        assert_eq!(timeout_for(&DaemonRequestPayload::ListCorpora), read_class);
        assert_eq!(
            timeout_for(&DaemonRequestPayload::ListFiles(ListFilesRequest {
                corpus: None
            })),
            read_class,
        );
        assert_eq!(
            timeout_for(&DaemonRequestPayload::ListTree(ListTreeRequest {
                corpus: None
            })),
            read_class,
        );
        assert_eq!(
            timeout_for(&DaemonRequestPayload::ReadMarkdown(ReadMarkdownRequest {
                corpus: None,
                path: "x.md".to_string(),
            })),
            read_class,
        );
        assert_eq!(
            timeout_for(&DaemonRequestPayload::Backlinks(BacklinksRequest {
                corpus: None,
                path: "x.md".to_string(),
            })),
            read_class,
        );
        assert_eq!(
            timeout_for(&DaemonRequestPayload::CorpusStats { corpus: None }),
            read_class,
        );
        assert_eq!(timeout_for(&DaemonRequestPayload::Shutdown), read_class);

        assert_eq!(
            timeout_for(&DaemonRequestPayload::Ground(GroundRequest {
                query: "q".to_string(),
                corpus: None,
                top_files: None,
                chunks_per_file: None,
                limit: None,
                snippet_chars: None,
            })),
            ground_class,
        );

        assert_eq!(
            timeout_for(&DaemonRequestPayload::Index(IndexRequest {
                corpus: None,
                paths_from: None,
                strict: false,
            })),
            mutation_class,
        );
        assert_eq!(
            timeout_for(&DaemonRequestPayload::AddMarkdown(
                AddMarkdownRequest::default()
            )),
            mutation_class,
        );
        assert_eq!(
            timeout_for(&DaemonRequestPayload::DeleteMarkdown(
                DeleteMarkdownRequest {
                    corpus: "c".to_string(),
                    path: "x.md".to_string(),
                }
            )),
            mutation_class,
        );
    }
}
