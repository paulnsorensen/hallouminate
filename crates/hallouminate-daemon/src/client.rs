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
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use super::bootstrap::ensure_daemon_running;
use super::framing::{MAX_IPC_LINE_BYTES, ipc_lines};
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
    let socket = daemon_socket_path()?;
    connect_at(&socket)
        .await
        .map_err(|_| daemon_client_unavailable(socket.display()))
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
/// default path (`None`) self-heals: on connection failure it runs `respawn`
/// once and retries the same socket. Explicit-socket callers (`Some(path)`)
/// never spawn.
pub async fn client_for_with<F, Fut>(
    socket: Option<&Path>,
    respawn: F,
) -> anyhow::Result<DaemonClient>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    match socket {
        Some(path) => {
            let connected = connect_at(path).await;
            if let Err(error) = &connected {
                tracing::warn!(
                    target: "hallouminate::client",
                    socket = %path.display(),
                    error = %format_args!("{error:#}"),
                    "daemon unreachable",
                );
            }
            connected
        }
        None => {
            let socket = daemon_socket_path()?;
            connect_or_spawn(&socket, respawn).await
        }
    }
}

/// Connect to `socket`; on failure run `respawn` once and retry the same
/// socket.
async fn connect_or_spawn<F, Fut>(socket: &Path, respawn: F) -> anyhow::Result<DaemonClient>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let error = match connect_at(socket).await {
        Ok(client) => return Ok(client),
        Err(error) => error,
    };
    tracing::info!(
        target: "hallouminate::client",
        socket = %socket.display(),
        error = %format_args!("{error:#}"),
        "daemon unreachable; spawning one",
    );
    if let Err(error) = respawn().await {
        tracing::warn!(
            target: "hallouminate::client",
            socket = %socket.display(),
            error = %format_args!("{error:#}"),
            "daemon spawn failed",
        );
        return Err(error);
    }
    let connected = connect_at(socket).await;
    if let Err(error) = &connected {
        tracing::warn!(
            target: "hallouminate::client",
            socket = %socket.display(),
            error = %format_args!("{error:#}"),
            "daemon unreachable after spawn",
        );
    }
    connected
}

/// Wrap an arbitrary error as a "daemon unavailable" `anyhow::Error` whose
/// message mirrors `connect_at`'s shape — for callers that need to surface
/// the daemon-down hint from a path that already produced its own error.
/// Kept as a small helper rather than open-coded so the documented hint
/// ("start it with `hallouminate daemon`") never drifts between call sites.
pub fn daemon_client_unavailable(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("daemon unavailable: {reason} (start it with `hallouminate daemon`)")
}

/// Dials one explicit socket path and confirms that a daemon accepts the
/// connection. Every discovery entry point resolves its path first, then
/// connects through this function.
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
    /// one-shot per connection, so each call opens a new socket. A
    /// transport failure logs one WARN event; callers that tolerate an
    /// absent daemon use [`Self::call_raw_quiet`] instead.
    pub async fn call_raw(&self, req: DaemonRequest) -> anyhow::Result<DaemonResponse> {
        let op = req.payload.op_name();
        let result = self.call_raw_quiet(req).await;
        if let Err(error) = &result {
            self.log_call_failure(op, error);
        }
        result
    }

    /// [`Self::call_raw`] without the failure log, for probes whose callers
    /// treat an unreachable daemon as a normal answer.
    pub(crate) async fn call_raw_quiet(
        &self,
        req: DaemonRequest,
    ) -> anyhow::Result<DaemonResponse> {
        let mut text = serde_json::to_string(&req)?;
        text.push('\n');
        if let DaemonRequestPayload::AddMarkdownBatch { .. } = &req.payload
            && text.len() > MAX_IPC_LINE_BYTES
        {
            return Err(DaemonRpcError::invalid_params(
                "serialized markdown batch exceeds the 4 MiB IPC frame limit",
            )
            .into());
        }
        // Wrap mid-call I/O errors with the same `daemon unavailable` hint
        // the initial connect uses. Without this, a daemon that dies after
        // the connect succeeds (write fails, read returns EOF, response
        // truncates) surfaces as a bare I/O / JSON error and MCP/CLI
        // callers lose the actionable "start it with `hallouminate daemon`"
        // recovery suffix.
        let mut stream = UnixStream::connect(&self.socket).await.map_err(|e| {
            daemon_client_unavailable(format!("connect to {} failed: {e}", self.socket.display()))
        })?;
        stream.write_all(text.as_bytes()).await.map_err(|e| {
            daemon_client_unavailable(format!("write to {} failed: {e}", self.socket.display()))
        })?;
        stream.flush().await.map_err(|e| {
            daemon_client_unavailable(format!("flush {} failed: {e}", self.socket.display()))
        })?;
        let (read_half, _) = stream.into_split();
        let mut lines = ipc_lines(read_half);
        let Some(line) = lines.next().await else {
            return Err(daemon_client_unavailable(format!(
                "daemon at {} closed the connection before responding",
                self.socket.display(),
            )));
        };
        let line = line.map_err(|e| {
            daemon_client_unavailable(format!("read from {} failed: {e}", self.socket.display()))
        })?;
        serde_json::from_str(&line).map_err(|e| {
            daemon_client_unavailable(format!(
                "invalid daemon response from {}: {e} (response: {line:?})",
                self.socket.display(),
            ))
        })
    }

    /// Logs one WARN event for a failed call. A local `InvalidParams`
    /// rejection is the caller's mistake, not a daemon failure.
    fn log_call_failure(&self, op: &'static str, error: &anyhow::Error) {
        if let Some(rpc) = error.downcast_ref::<DaemonRpcError>()
            && rpc.kind == ErrorKind::InvalidParams
        {
            return;
        }
        tracing::warn!(
            target: "hallouminate::client",
            op,
            socket = %self.socket.display(),
            error = %format_args!("{error:#}"),
            "daemon call failed",
        );
    }

    /// [`call_raw`] with a bounded round-trip deadline. `call_raw` itself
    /// has no timeout — a daemon that accepts the connection but never
    /// writes/reads would otherwise hang the caller forever. Lifecycle
    /// commands (`stop`, `status`) that must never wait indefinitely for an
    /// accepted-but-silent socket use [`Self::call_raw_with_timeout_quiet`].
    ///
    /// Deadline expiry is typed [`DaemonRpcError`] with
    /// [`ErrorKind::Retryable`] (#216): the daemon accepted the connection,
    /// so it is likely busy rather than down, and the caller can retry.
    /// Batch deadlines leave completion uncertain; callers must inspect files before retrying.
    /// Transport failures inside `call_raw` (connect/write/read/EOF) keep
    /// the untyped "daemon unavailable" shape with its restart hint —
    /// retrying against a dead daemon cannot succeed. Any failure logs one
    /// WARN event.
    pub async fn call_raw_with_timeout(
        &self,
        req: DaemonRequest,
        timeout: Duration,
    ) -> anyhow::Result<DaemonResponse> {
        let op = req.payload.op_name();
        let result = self.call_raw_with_timeout_quiet(req, timeout).await;
        if let Err(error) = &result {
            self.log_call_failure(op, error);
        }
        result
    }

    /// [`Self::call_raw_with_timeout`] without the failure log, for
    /// lifecycle commands that treat an unreachable daemon as a normal answer.
    pub(crate) async fn call_raw_with_timeout_quiet(
        &self,
        req: DaemonRequest,
        timeout: Duration,
    ) -> anyhow::Result<DaemonResponse> {
        let mut timeout_hint = "the daemon may be busy — retry";
        if let DaemonRequestPayload::AddMarkdownBatch { .. } = &req.payload {
            timeout_hint = "batch completion is uncertain; inspect files before retrying. No automatic retry or rollback occurred";
        }
        match tokio::time::timeout(timeout, self.call_raw_quiet(req)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(DaemonRpcError::retryable(format!(
                "no response from {} within {}s; {timeout_hint}",
                self.socket.display(),
                timeout.as_secs(),
            ))
            .into()),
        }
    }

    /// Convenience wrapper: send a request and decode the `Ok` payload as
    /// `T`. Daemon-side `Err` variants surface as `anyhow::Error` with the
    /// daemon's message preserved. Bounded by [`timeout_for`]'s per-class
    /// deadline — `call_raw` itself never times out, so an unbounded `call`
    /// would hang the caller forever on a wedged daemon (issue #216).
    pub async fn call<T: DeserializeOwned>(&self, req: DaemonRequest) -> anyhow::Result<T> {
        let op = req.payload.op_name();
        let timeout = timeout_for(&req.payload);
        match self.call_raw_with_timeout(req, timeout).await? {
            DaemonResponse::Ok { result } => serde_json::from_value(result).map_err(|e| {
                tracing::warn!(
                    target: "hallouminate::client",
                    op,
                    error = %e,
                    "daemon returned an unexpected payload",
                );
                anyhow::anyhow!("daemon returned unexpected payload: {e}")
            }),
            DaemonResponse::Err { kind, message } => {
                tracing::debug!(
                    target: "hallouminate::client",
                    op,
                    kind = ?kind,
                    "daemon returned an error: {message}",
                );
                match kind {
                    ErrorKind::InvalidParams => Err(DaemonRpcError::invalid_params(message).into()),
                    ErrorKind::Internal => Err(DaemonRpcError::internal(message).into()),
                    ErrorKind::Retryable => Err(DaemonRpcError::retryable(message).into()),
                }
            }
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
        | DaemonRequestPayload::AddMarkdownBatch { .. }
        | DaemonRequestPayload::DeleteMarkdown(_) => Duration::from_secs(15 * 60),
        DaemonRequestPayload::Ping
        | DaemonRequestPayload::ListCorpora
        | DaemonRequestPayload::ListFiles(_)
        | DaemonRequestPayload::ListTree(_)
        | DaemonRequestPayload::ReadMarkdown(_)
        | DaemonRequestPayload::Backlinks(_)
        | DaemonRequestPayload::CorpusStats { .. }
        | DaemonRequestPayload::Status
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
    pub fn retryable(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Retryable,
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
    use tokio::io::{AsyncBufReadExt, BufReader};

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

    fn client_events(
        capture: &crate::test_support::EventCapture,
        level: tracing::Level,
    ) -> Vec<crate::test_support::CapturedEvent> {
        let mut events = Vec::new();
        for event in capture.events_for("hallouminate::client") {
            if event.level == level {
                events.push(event);
            }
        }
        events
    }

    #[tokio::test]
    async fn unreachable_explicit_socket_logs_one_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("never.sock");
        let (capture, _guard) = crate::test_support::EventCapture::install();
        client_for(Some(&missing))
            .await
            .expect_err("connect to a missing socket must fail");
        let warnings = client_events(&capture, tracing::Level::WARN);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let socket = warnings[0].fields.get("socket").expect("socket field");
        assert_eq!(socket, &missing.display().to_string());
    }

    #[tokio::test]
    async fn failed_spawn_logs_one_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("daemon.sock");
        let (capture, _guard) = crate::test_support::EventCapture::install();
        connect_or_spawn(&socket, || async { anyhow::bail!("spawn refused") })
            .await
            .expect_err("a failed spawn must fail the connect");
        let warnings = client_events(&capture, tracing::Level::WARN);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let error = warnings[0].fields.get("error").expect("error field");
        assert!(error.contains("spawn refused"), "{error}");
    }

    async fn hangup_client() -> (tempfile::TempDir, DaemonClient) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock_path = tmp.path().join("hangup.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");
        let client = connect_at(&sock_path).await.expect("connect");
        tokio::spawn(async move {
            loop {
                let (stream, _addr) = listener.accept().await.expect("accept");
                drop(stream);
            }
        });
        (tmp, client)
    }

    #[tokio::test]
    async fn transport_failure_logs_one_warning_at_the_public_boundary() {
        let (_tmp, client) = hangup_client().await;
        let (capture, _guard) = crate::test_support::EventCapture::install();
        client
            .call_raw(ping_request())
            .await
            .expect_err("a hung-up peer must fail");
        let warnings = client_events(&capture, tracing::Level::WARN);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[tokio::test]
    async fn quiet_calls_log_no_warning_on_transport_failure() {
        let (_tmp, client) = hangup_client().await;
        let (capture, _guard) = crate::test_support::EventCapture::install();
        client
            .call_raw_quiet(ping_request())
            .await
            .expect_err("a hung-up peer must fail");
        client
            .call_raw_with_timeout_quiet(ping_request(), Duration::from_secs(5))
            .await
            .expect_err("a hung-up peer must fail");
        let warnings = client_events(&capture, tracing::Level::WARN);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[tokio::test]
    async fn rpc_timeout_logs_one_warning_with_the_op() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock_path = tmp.path().join("silent.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");
        tokio::spawn(async move {
            let (_stream, _addr) = listener.accept().await.expect("accept");
            std::future::pending::<()>().await;
        });
        let client = connect_at(&sock_path).await.expect("connect");
        let (capture, _guard) = crate::test_support::EventCapture::install();
        client
            .call_raw_with_timeout(
                DaemonRequest {
                    cwd: PathBuf::from("."),
                    payload: DaemonRequestPayload::Ping,
                },
                Duration::from_millis(100),
            )
            .await
            .expect_err("a silent server must time out");
        let warnings = client_events(&capture, tracing::Level::WARN);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            warnings[0].fields.get("op").map(String::as_str),
            Some("ping")
        );
    }

    #[tokio::test]
    async fn daemon_error_is_logged_at_debug_when_mapped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock_path = tmp.path().join("err.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");
        tokio::spawn(async move {
            let (stream, _addr) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = stream.into_split();
            let mut request = String::new();
            BufReader::new(read_half)
                .read_line(&mut request)
                .await
                .expect("read request");
            let mut line = serde_json::to_string(&DaemonResponse::internal("store exploded"))
                .expect("serialize response");
            line.push('\n');
            write_half
                .write_all(line.as_bytes())
                .await
                .expect("write response");
        });
        let client = DaemonClient { socket: sock_path };
        let (capture, _guard) = crate::test_support::EventCapture::install();
        let err = client
            .call::<serde_json::Value>(DaemonRequest {
                cwd: PathBuf::from("."),
                payload: DaemonRequestPayload::Status,
            })
            .await
            .expect_err("a daemon error must fail the call");
        let rpc = err.downcast_ref::<DaemonRpcError>().expect("typed error");
        assert_eq!(rpc.kind, ErrorKind::Internal);
        let debugs = client_events(&capture, tracing::Level::DEBUG);
        assert_eq!(debugs.len(), 1, "{debugs:?}");
        let message = debugs[0].fields.get("message").expect("message field");
        assert!(message.contains("store exploded"), "{message}");
        assert!(client_events(&capture, tracing::Level::WARN).is_empty());
    }

    #[tokio::test]
    async fn connect_or_spawn_prefers_live_socket_over_spawning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind socket");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                drop(stream);
            }
        });

        let respawn_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&respawn_calls);
        let client = connect_or_spawn(&socket, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { anyhow::Ok(()) }
        })
        .await
        .expect("must connect to the live socket instead of spawning");

        assert_eq!(client.socket_path(), socket.as_path());
        assert_eq!(
            respawn_calls.load(Ordering::SeqCst),
            0,
            "must not spawn a daemon when the socket already answers",
        );
    }

    #[tokio::test]
    async fn connect_or_spawn_spawns_once_when_socket_is_dead() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("daemon.sock");
        let respawn_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&respawn_calls);
        let result = connect_or_spawn(&socket, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { anyhow::Ok(()) }
        })
        .await;
        result.expect_err("a still-missing socket after respawn must fail");
        assert_eq!(respawn_calls.load(Ordering::SeqCst), 1);
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
                footnote_mode: Default::default(),
                match_mode: Default::default(),
                group_by: Default::default(),
                output: Default::default(),
            })),
            ground_class,
        );

        assert_eq!(
            timeout_for(&DaemonRequestPayload::Index(IndexRequest {
                corpus: None,
                paths_from: None,
                strict: false,
                background: false,
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

    #[tokio::test]
    async fn rpc_timeout_is_typed_retryable() {
        // #216: when an RPC exceeds its deadline the caller must get a typed
        // `DaemonRpcError { kind: Retryable }` — not an opaque "daemon
        // unavailable" — so MCP/CLI callers regain control with an error
        // they can pattern on and retry. The daemon accepted the connection,
        // so "start it with `hallouminate daemon`" would be the wrong hint.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock_path = tmp.path().join("silent.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");
        tokio::spawn(async move {
            let (_stream, _addr) = listener.accept().await.expect("accept");
            std::future::pending::<()>().await;
        });

        let client = connect_at(&sock_path).await.expect("connect");
        let err = client
            .call_raw_with_timeout(
                DaemonRequest {
                    cwd: PathBuf::from("."),
                    payload: DaemonRequestPayload::Ping,
                },
                Duration::from_millis(100),
            )
            .await
            .expect_err("a silent server must time out");
        let rpc = err
            .downcast_ref::<DaemonRpcError>()
            .expect("timeout must surface as a typed DaemonRpcError");
        assert_eq!(rpc.kind, ErrorKind::Retryable);
        assert!(
            rpc.message.contains("retry"),
            "CLI callers see only the message, so it must say the error is \
             retryable: {}",
            rpc.message,
        );
    }

    #[tokio::test]
    async fn transport_eof_is_not_typed_retryable() {
        // Conservative classification (#216): only a deadline expiry is
        // typed Retryable. A daemon that closes the connection before
        // responding is a transport failure — it keeps the untyped
        // "daemon unavailable" shape (restart hint), because retrying
        // against a dead daemon cannot succeed.
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock_path = tmp.path().join("eof.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).expect("bind");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                drop(stream);
            }
        });

        let client = connect_at(&sock_path).await.expect("connect");
        let err = client
            .call_raw_with_timeout(
                DaemonRequest {
                    cwd: PathBuf::from("."),
                    payload: DaemonRequestPayload::Ping,
                },
                Duration::from_secs(5),
            )
            .await
            .expect_err("an immediate EOF must fail");
        assert!(
            err.downcast_ref::<DaemonRpcError>().is_none(),
            "transport EOF must not be classified retryable: {err:#}",
        );
    }

    use super::super::framing::MAX_IPC_LINE_BYTES;

    fn ping_request() -> DaemonRequest {
        DaemonRequest {
            cwd: PathBuf::from("."),
            payload: DaemonRequestPayload::Ping,
        }
    }

    enum LineEnding {
        Lf,
        Eof,
    }

    fn response_bytes(total_bytes: usize, ending: LineEnding) -> Vec<u8> {
        let empty = serde_json::to_vec(&DaemonResponse::Ok {
            result: serde_json::Value::String(String::new()),
        })
        .expect("serialize empty response");
        let newline_bytes = match ending {
            LineEnding::Lf => 1,
            LineEnding::Eof => 0,
        };
        let payload_bytes = total_bytes
            .checked_sub(empty.len() + newline_bytes)
            .expect("response size must fit JSON envelope");
        let mut bytes = serde_json::to_vec(&DaemonResponse::Ok {
            result: serde_json::Value::String("a".repeat(payload_bytes)),
        })
        .expect("serialize response");
        match ending {
            LineEnding::Lf => bytes.push(b'\n'),
            LineEnding::Eof => {}
        }
        assert_eq!(bytes.len(), total_bytes);
        bytes
    }

    async fn call_with_response(bytes: Vec<u8>) -> anyhow::Result<DaemonResponse> {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("response.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let (probe, _) = listener.accept().await.expect("accept probe");
            drop(probe);
            let (stream, _) = listener.accept().await.expect("accept request");
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut request = String::new();
            reader.read_line(&mut request).await.expect("read request");
            let _ = write_half.write_all(&bytes).await;
        });
        let client = connect_at(&socket).await.expect("connect client");
        let result = client.call_raw(ping_request()).await;
        server.await.expect("response server");
        result
    }

    #[tokio::test]
    async fn response_below_byte_cap_is_accepted() {
        let bytes = response_bytes(MAX_IPC_LINE_BYTES - 1, LineEnding::Lf);
        call_with_response(bytes)
            .await
            .expect("response below cap must decode");
    }

    #[tokio::test]
    async fn response_at_newline_inclusive_byte_cap_is_accepted() {
        let bytes = response_bytes(MAX_IPC_LINE_BYTES, LineEnding::Lf);
        call_with_response(bytes)
            .await
            .expect("response at cap must decode");
    }

    #[tokio::test]
    async fn response_above_newline_inclusive_byte_cap_is_rejected() {
        let bytes = response_bytes(MAX_IPC_LINE_BYTES + 1, LineEnding::Lf);
        let error = call_with_response(bytes)
            .await
            .expect_err("response above cap must fail");
        assert!(error.to_string().contains("max line length exceeded"));
    }

    #[tokio::test]
    async fn response_without_lf_is_decoded_at_eof() {
        let bytes = response_bytes(128, LineEnding::Eof);
        call_with_response(bytes)
            .await
            .expect("complete response at EOF must decode");
    }

    #[tokio::test]
    async fn invalid_utf8_response_is_rejected() {
        let error = call_with_response(vec![0xff, b'\n'])
            .await
            .expect_err("invalid UTF-8 must fail");
        assert!(error.to_string().contains("Unable to decode input as UTF8"));
    }

    #[tokio::test]
    async fn empty_response_is_rejected_as_transport_eof() {
        let error = call_with_response(Vec::new())
            .await
            .expect_err("empty response must fail");
        assert!(
            error
                .to_string()
                .contains("closed the connection before responding")
        );
    }

    #[tokio::test]
    async fn truncated_response_is_rejected_as_invalid_json() {
        let error = call_with_response(br#"{"Ok":"#.to_vec())
            .await
            .expect_err("truncated response must fail");
        assert!(error.to_string().contains("invalid daemon response"));
    }

    fn markdown_batch_request(content: String) -> DaemonRequest {
        DaemonRequest {
            cwd: PathBuf::from("/"),
            payload: DaemonRequestPayload::AddMarkdownBatch {
                corpus: "wiki".into(),
                items: vec![crate::ipc::AddMarkdownItem {
                    path: "once.md".into(),
                    content,
                    overwrite: false,
                    under_heading: None,
                    position: crate::ipc::Position::default(),
                    replace_lines: None,
                    replace_match: None,
                }],
            },
        }
    }

    #[tokio::test]
    async fn markdown_batch_frame_limit_includes_the_newline_before_connect() {
        let base = serde_json::to_vec(&markdown_batch_request(String::new()))
            .expect("serialize")
            .len();
        let request = markdown_batch_request(" ".repeat(MAX_IPC_LINE_BYTES - base - 1));
        assert_eq!(
            serde_json::to_vec(&request).expect("serialize").len() + 1,
            MAX_IPC_LINE_BYTES
        );
        let tmp = tempfile::tempdir().expect("temp");
        let socket = tmp.path().join("frame.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read, mut write) = stream.into_split();
            let mut lines = ipc_lines(read);
            let line = lines.next().await.expect("line").expect("valid frame");
            assert_eq!(line.len() + 1, MAX_IPC_LINE_BYTES);
            write
                .write_all(b"{\"status\":\"ok\",\"result\":\"accepted\"}\n")
                .await
                .expect("response");
        });
        let client = DaemonClient { socket };
        let response = client.call_raw(request).await.expect("exact cap accepted");
        let DaemonResponse::Ok { result } = response else {
            panic!("cap rejected");
        };
        assert_eq!(result, "accepted");
        server.await.expect("server");

        let request = markdown_batch_request(" ".repeat(MAX_IPC_LINE_BYTES - base));
        let client = DaemonClient {
            socket: tmp.path().join("missing.sock"),
        };
        let error = client
            .call_raw(request)
            .await
            .expect_err("one byte over cap");
        let error = error
            .downcast_ref::<DaemonRpcError>()
            .expect("input error before connection");
        assert_eq!(error.kind, ErrorKind::InvalidParams);
        assert!(error.message.contains("4 MiB"), "{}", error.message);
    }

    #[tokio::test]
    async fn markdown_batch_timeout_reports_uncertainty_without_replay() {
        let tmp = tempfile::tempdir().expect("temp");
        let socket = tmp.path().join("timeout.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut lines = ipc_lines(stream);
            let line = lines.next().await.expect("request").expect("frame");
            let request: DaemonRequest = serde_json::from_str(&line).expect("request");
            assert_eq!(timeout_for(&request.payload), Duration::from_secs(15 * 60));
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err(),
                "no replay"
            );
        });
        let client = DaemonClient { socket };
        let error = client
            .call_raw_with_timeout(
                markdown_batch_request(String::new()),
                Duration::from_millis(50),
            )
            .await
            .expect_err("timeout");
        let error = error
            .downcast_ref::<DaemonRpcError>()
            .expect("typed timeout");
        assert_eq!(error.kind, ErrorKind::Retryable);
        assert!(
            error.message.contains("completion is uncertain"),
            "{}",
            error.message
        );
        server.await.expect("server");
    }
}
