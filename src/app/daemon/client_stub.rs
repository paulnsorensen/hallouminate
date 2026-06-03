//! Non-unix stub for the daemon RPC client (`client.rs`).
//!
//! The daemon's IPC transport is built on Unix domain sockets; the Windows
//! named-pipes port is #48 stage 2. Until then `DaemonClient` keeps the unix
//! client's public shape, but every connect / call path returns the
//! unsupported error so `cli.rs` dispatch and the `mod.rs` re-exports compile
//! on non-unix targets.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use super::ipc::{DaemonRequest, DaemonResponse, ErrorKind};

/// Mirrors the unix `DaemonClient` so the public API is byte-identical. Never
/// constructed on this platform — every constructor returns the unsupported
/// error — but the `socket` field keeps `socket_path`'s signature honest.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    #[allow(dead_code)]
    socket: PathBuf,
}

pub async fn daemon_client() -> anyhow::Result<DaemonClient> {
    Err(daemon_unsupported())
}

pub async fn client_for(_socket: Option<&Path>) -> anyhow::Result<DaemonClient> {
    Err(daemon_unsupported())
}

/// Byte-identical with the unix client's helper: the documented daemon-down
/// hint that every client entry point surfaces.
pub fn daemon_client_unavailable(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("daemon unavailable: {reason} (start it with `hallouminate daemon`)")
}

/// Canonical "daemon unsupported on this platform" error shared by every
/// non-unix daemon stub. Routes through `daemon_client_unavailable` so the
/// message shape matches the unix client's daemon-down hint.
pub(super) fn daemon_unsupported() -> anyhow::Error {
    daemon_client_unavailable(
        "the local daemon is unsupported on this platform \
         (the Windows named-pipes transport is tracked in #48 stage 2)",
    )
}

pub async fn connect_at(_socket: &Path) -> anyhow::Result<DaemonClient> {
    Err(daemon_unsupported())
}

impl DaemonClient {
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub async fn call_raw(&self, _req: DaemonRequest) -> anyhow::Result<DaemonResponse> {
        Err(daemon_unsupported())
    }

    pub async fn call<T: DeserializeOwned>(&self, _req: DaemonRequest) -> anyhow::Result<T> {
        Err(daemon_unsupported())
    }
}

/// Typed daemon error, mirroring the unix client so MCP/CLI consumers can
/// downcast identically on both platforms.
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
