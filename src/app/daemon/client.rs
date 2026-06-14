//! Daemon RPC client.
//!
//! `DaemonClient::connect` resolves the socket path from
//! `daemon_socket_path()` (test-overridable via `HALLOUMINATE_SOCKET`) and
//! returns a clear `daemon unavailable` error when the endpoint is missing or
//! unreachable. Callers that fall back to a non-daemon path do so
//! explicitly; the client never auto-starts a daemon.
//!
//! The transport (unix domain socket / Windows named pipe) is abstracted by
//! `transport.rs`; this module owns the JSON request/response envelope, the
//! daemon-down error vocabulary, and the typed error surface.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use super::ipc::{DaemonRequest, DaemonResponse, ErrorKind};
use super::socket::daemon_socket_path;
use super::transport::{self, daemon_endpoint};

/// Client handle: just remembers which socket path to dial. Stateless
/// otherwise — every `call` opens a fresh connection.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
}

/// Connect to the daemon. Returns `Err` with a clear "daemon unavailable"
/// message when the endpoint is missing, unreadable, or the connect fails.
pub async fn daemon_client() -> anyhow::Result<DaemonClient> {
    let socket = daemon_socket_path();
    connect_at(&socket).await
}

/// Connect to the daemon at an explicit socket path when set, otherwise
/// resolve via `daemon_socket_path()` (which honors `HALLOUMINATE_SOCKET`).
/// One canonical entry point for CLI / MCP callers so the `--socket` flag
/// path and the env-var / default path go through the same client builder.
pub async fn client_for(socket: Option<&Path>) -> anyhow::Result<DaemonClient> {
    match socket {
        Some(path) => connect_at(path).await,
        None => daemon_client().await,
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
    // Probe the endpoint with a quick connect to confirm a daemon is alive,
    // surfacing the failure here instead of inside the first `call`.
    let endpoint = daemon_endpoint(socket);
    transport::connect(&endpoint).await.map_err(|e| {
        daemon_client_unavailable(format!("cannot connect to {}: {e}", socket.display()))
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
    /// one-shot per connection, so each call opens a new connection.
    pub async fn call_raw(&self, req: DaemonRequest) -> anyhow::Result<DaemonResponse> {
        let endpoint = daemon_endpoint(&self.socket);
        let stream = transport::connect(&endpoint).await.map_err(|e| {
            daemon_client_unavailable(format!("connect to {} failed: {e}", self.socket.display()))
        })?;
        // Wrap mid-call I/O errors with the same `daemon unavailable` hint the
        // initial connect uses. Without this, a daemon that dies after the
        // connect succeeds (write fails, read returns EOF, response truncates)
        // surfaces as a bare I/O / JSON error and MCP/CLI callers lose the
        // actionable "start it with `hallouminate daemon`" recovery suffix.
        let line = transport::round_trip(stream, &req).await.map_err(|e| {
            daemon_client_unavailable(format!("rpc to {} failed: {e}", self.socket.display()))
        })?;
        let line = line.ok_or_else(|| {
            daemon_client_unavailable(format!(
                "daemon at {} closed the connection before responding",
                self.socket.display(),
            ))
        })?;
        let response: DaemonResponse = serde_json::from_str(line.trim_end()).map_err(|e| {
            daemon_client_unavailable(format!(
                "invalid daemon response from {}: {e} (response: {line:?})",
                self.socket.display(),
            ))
        })?;
        Ok(response)
    }

    /// Convenience wrapper: send a request and decode the `Ok` payload as
    /// `T`. Daemon-side `Err` variants surface as `anyhow::Error` with the
    /// daemon's message preserved.
    pub async fn call<T: DeserializeOwned>(&self, req: DaemonRequest) -> anyhow::Result<T> {
        match self.call_raw(req).await? {
            DaemonResponse::Ok { result } => serde_json::from_value(result)
                .map_err(|e| anyhow::anyhow!("daemon returned unexpected payload: {e}")),
            DaemonResponse::Err { kind, message } => match kind {
                ErrorKind::InvalidParams => Err(DaemonRpcError::invalid_params(message).into()),
                ErrorKind::Internal => Err(DaemonRpcError::internal(message).into()),
            },
        }
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
