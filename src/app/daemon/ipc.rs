//! IPC types shared between the daemon server and its CLI/MCP clients.
//!
//! Wire format is JSON-lines over a Unix domain socket: one request,
//! one response, then the connection closes. Keeps server-side dispatch
//! trivially correct around per-corpus locks and the global write-lane
//! semaphore without needing an in-band correlation id.
//!
//! # Wire compatibility (v1)
//!
//! The daemon and every client (CLI, MCP) ship from the *same* `hallouminate`
//! binary. The response payloads in this module embed domain types
//! ([`IndexReport`], [`GroundResponse`], [`FileEntry`]) wholesale and carry
//! **no protocol version envelope** and no `#[serde(deny_unknown_fields)]`
//! — a single binary owns both sides of the socket, so a field added to a
//! domain type lands on both sides in the same release. **Cross-version IPC
//! (a client from one release talking to a daemon from another) is not a
//! supported configuration in v1.** If a future contributor wants to ship a
//! standalone client (e.g. a third-party Python client, an out-of-process
//! agent) they must first add an explicit `version: u32` to the request /
//! response envelopes and a negotiation handshake; do not assume the
//! current shape is forward-compatible by accident.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::app::cli::IndexReport;
use crate::domain::corpus::sandbox::FileEntry;
use crate::domain::ground::GroundResponse;

/// Top-level request envelope. One variant per CLI/MCP operation the daemon
/// owns. Stateless operations (`Ping`, `ListCorpora`, `ListFiles`,
/// `ReadMarkdown`, `Ground`) skip the write lane; mutating operations
/// (`Index`, `AddMarkdown`, `DeleteMarkdown`) take the corpus lock and the
/// write-lane permit in that order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Liveness check; the server responds with `Pong`.
    Ping,
    /// `ground` semantic search.
    Ground(GroundRequest),
    /// `index` corpus rebuild.
    Index(IndexRequest),
    /// List configured corpora (explicit + repository-derived).
    ListCorpora,
    /// List files visible in a corpus.
    ListFiles(ListFilesRequest),
    /// Write a markdown file to a corpus root and refresh its index rows.
    AddMarkdown(AddMarkdownRequest),
    /// Read verbatim markdown content from a corpus root.
    ReadMarkdown(ReadMarkdownRequest),
    /// Unlink a markdown file from a corpus root and prune its index rows.
    DeleteMarkdown(DeleteMarkdownRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroundRequest {
    pub query: String,
    pub corpus: Option<String>,
    pub top_files: Option<usize>,
    pub chunks_per_file: Option<usize>,
    pub limit: Option<usize>,
    pub snippet_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRequest {
    pub corpus: Option<String>,
    pub paths_from: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListFilesRequest {
    pub corpus: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMarkdownRequest {
    pub corpus: String,
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadMarkdownRequest {
    pub corpus: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteMarkdownRequest {
    pub corpus: String,
    pub path: String,
}

/// Daemon response envelope. `Ok` carries an opaque JSON payload — each
/// request variant documents its own response shape. `Err` distinguishes
/// invalid-input failures (the MCP transport maps these to JSON-RPC -32602)
/// from internal faults (-32603).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DaemonResponse {
    Ok { result: serde_json::Value },
    Err { kind: ErrorKind, message: String },
}

impl DaemonResponse {
    pub fn ok<T: Serialize>(value: &T) -> Self {
        DaemonResponse::Ok {
            result: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        }
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        DaemonResponse::Err {
            kind: ErrorKind::InvalidParams,
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        DaemonResponse::Err {
            kind: ErrorKind::Internal,
            message: msg.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidParams,
    Internal,
}

// ── Response payload structs ───────────────────────────────────────────
//
// One per request variant. CLI / MCP clients deserialize the daemon's
// `Ok` payload into these typed shapes via `DaemonClient::call::<T>()`;
// dispatch.rs constructs them and serializes through `DaemonResponse::ok`.

/// `ListCorpora` payload entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorpusEntry {
    pub name: String,
    pub paths: Vec<String>,
}

/// `Ground` payload. Carries both the rendered outline (matches the MCP
/// `ground` text content) and the full structured response so different
/// transports can pick the shape they need without paying for a second
/// search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroundResult {
    pub outline: String,
    pub response: GroundResponse,
}

/// `AddMarkdown` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMarkdownResult {
    pub corpus: String,
    pub path: String,
    pub absolute_path: String,
    pub indexed: IndexReport,
}

/// `ReadMarkdown` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadMarkdownResult {
    pub corpus: String,
    pub path: String,
    pub absolute_path: String,
    pub content: String,
    pub bytes: u64,
}

/// `DeleteMarkdown` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteMarkdownResult {
    pub corpus: String,
    pub path: String,
    pub absolute_path: String,
    pub file_ref: String,
}

/// `ListFiles` payload alias — daemon emits an array of [`FileEntry`].
pub type ListFilesResult = Vec<FileEntry>;

/// `ListCorpora` payload alias — daemon emits an array of [`CorpusEntry`].
pub type ListCorporaResult = Vec<CorpusEntry>;
