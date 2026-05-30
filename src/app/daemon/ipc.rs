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
use crate::domain::corpus::sandbox::{FileEntry, TreeNode};
use crate::domain::ground::GroundResponse;

/// Top-level request envelope. Carries a `cwd: PathBuf` plus a
/// [`DaemonRequestPayload`] discriminating one of the request variants.
///
/// `cwd` is the client's working directory at request time — the daemon
/// walks it on every request to discover the active repo-layer config
/// (`.hallouminate/config.toml`) and merge it with the boot baseline. See
/// `.cheese/specs/repo-config-discovery.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRequest {
    pub cwd: PathBuf,
    pub payload: DaemonRequestPayload,
}

/// The discriminated request body. One variant per CLI/MCP operation the
/// daemon owns. Stateless operations (`Ping`, `ListCorpora`, `ListFiles`,
/// `ReadMarkdown`, `Ground`) skip the write lane; mutating operations
/// (`Index`, `AddMarkdown`, `DeleteMarkdown`) take the corpus lock and the
/// write-lane permit in that order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DaemonRequestPayload {
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
    /// List files visible in a corpus, grouped into a directory tree.
    ListTree(ListTreeRequest),
    /// Write a markdown file to a corpus root and refresh its index rows.
    AddMarkdown(AddMarkdownRequest),
    /// Read verbatim markdown content from a corpus root.
    ReadMarkdown(ReadMarkdownRequest),
    /// Unlink a markdown file from a corpus root and prune its index rows.
    DeleteMarkdown(DeleteMarkdownRequest),
    /// Copy a single markdown entry from `source_corpus` into the corpus
    /// marked `global = true`, reindexing the global corpus row synchronously.
    GlobalizeMarkdown(GlobalizeMarkdownRequest),
    /// Ask the daemon to shut down gracefully: cancel the accept loop, drop
    /// the flock guard, and remove the socket file. The server acks with
    /// `"stopping"` before tearing down.
    Shutdown,
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
pub struct ListTreeRequest {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalizeMarkdownRequest {
    /// Corpus the entry currently lives in.
    pub source_corpus: String,
    /// Relative path of the entry within `source_corpus`.
    pub path: String,
    /// Destination path within the global corpus. Defaults to `path`.
    #[serde(default)]
    pub dest_path: Option<String>,
    /// Replace an existing destination entry. Defaults to false.
    #[serde(default)]
    pub overwrite: bool,
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
    /// Wrap a serializable payload in an `Ok` response.
    ///
    /// On serialization failure the result is an [`Internal`](ErrorKind::Internal)
    /// error rather than a silent `Ok { result: Null }`: a `null` payload
    /// reads as an empty success across the CLI/MCP transport, so swallowing
    /// the error would mask the fault. The `Err` variant is a valid `Self`,
    /// so the signature is unchanged.
    pub fn ok<T: Serialize>(value: &T) -> Self {
        match serde_json::to_value(value) {
            Ok(result) => DaemonResponse::Ok { result },
            Err(e) => DaemonResponse::internal(format!("serialize response: {e}")),
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

/// `GlobalizeMarkdown` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalizeMarkdownResult {
    pub source_corpus: String,
    pub source_path: String,
    pub global_corpus: String,
    pub dest_path: String,
    pub absolute_path: String,
    pub indexed: IndexReport,
}

/// `ListFiles` payload alias — daemon emits an array of [`FileEntry`].
pub type ListFilesResult = Vec<FileEntry>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTreeResult {
    pub corpus: String,
    pub root: TreeNode,
}

/// `ListCorpora` payload alias — daemon emits an array of [`CorpusEntry`].
pub type ListCorporaResult = Vec<CorpusEntry>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload whose `Serialize` impl always fails, standing in for any
    /// domain type that errors mid-serialization (e.g. a map with non-string
    /// keys nested deep in a response).
    struct FailsToSerialize;

    impl Serialize for FailsToSerialize {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("boom"))
        }
    }

    #[test]
    fn ok_maps_serialize_failure_to_internal_error_not_null_success() {
        // WHY: a serialization failure that degrades to `Ok { result: Null }`
        // reads as an empty success on the CLI/MCP transport, hiding the
        // fault from the caller. It must surface as an Internal error.
        let resp = DaemonResponse::ok(&FailsToSerialize);
        match resp {
            DaemonResponse::Err { kind, message } => {
                assert_eq!(kind, ErrorKind::Internal);
                assert!(
                    message.starts_with("serialize response:"),
                    "message should name the serialize failure, got: {message:?}"
                );
            }
            DaemonResponse::Ok { result } => {
                panic!("serialize failure must not produce Ok, got result: {result:?}");
            }
        }
    }

    #[test]
    fn ok_wraps_serializable_payload_verbatim() {
        // WHY: the failure path must not regress the happy path — a value
        // that serializes cleanly still lands in `Ok { result }`.
        let resp = DaemonResponse::ok(&"pong");
        match resp {
            DaemonResponse::Ok { result } => {
                assert_eq!(result, serde_json::Value::String("pong".to_string()));
            }
            DaemonResponse::Err { kind, message } => {
                panic!("clean payload must serialize, got {kind:?}: {message}");
            }
        }
    }

    #[test]
    fn serialize_failure_response_is_err_envelope_on_the_wire() {
        // WHY: the defect is transport-level — a client deserializing the
        // response must see an error, not the old `{"status":"ok",
        // "result":null}` it would misread as empty success. Pin the wire
        // shape a remote client actually decodes, so a regression to the
        // null-success envelope is caught here.
        let wire = serde_json::to_value(DaemonResponse::ok(&FailsToSerialize))
            .expect("the Err envelope itself serializes cleanly");
        assert_eq!(wire["status"], "err");
        assert_eq!(wire["kind"], "internal");
        assert!(
            wire.get("result").is_none(),
            "an error envelope must not carry a `result` field, got: {wire}"
        );
        assert!(
            wire["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("serialize response:")),
            "error message must name the serialize failure, got: {wire}"
        );
    }
}
