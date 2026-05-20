//! Local daemon: the single owner of the configured LanceDB ground
//! directory, repository registry, per-corpus mutation locks, and the
//! global write-lane.
//!
//! See `.cheese/specs/repository-daemon-wikis.md` for the design.
//! Operational summary:
//!
//! - One daemon per user-local socket (`HALLOUMINATE_SOCKET`,
//!   `$XDG_RUNTIME_DIR/hallouminate/daemon.sock`, or
//!   `~/.cache/hallouminate/daemon.sock`).
//! - Single-instance enforced via `flock` on `<socket>.lock`.
//! - CLI / MCP transports become clients of the daemon for stateful
//!   operations and fail loudly when the daemon is unreachable instead of
//!   silently auto-starting (which would recreate the multi-process race).
//!
//! Lock order across the dispatcher is documented in `state.rs`.

mod client;
mod dispatch;
mod ipc;
mod server;
mod socket;
mod state;

pub use client::{
    DaemonClient, DaemonRpcError, client_for, connect_at, daemon_client, daemon_client_unavailable,
};
pub use ipc::{
    AddMarkdownRequest, AddMarkdownResult, CorpusEntry, DaemonRequest, DaemonRequestPayload,
    DaemonResponse, DeleteMarkdownRequest, DeleteMarkdownResult, ErrorKind, GroundRequest,
    GroundResult, IndexRequest, ListCorporaResult, ListFilesRequest, ListFilesResult,
    ReadMarkdownRequest, ReadMarkdownResult,
};
pub use server::{DaemonArgs, run_daemon, serve};
pub use socket::daemon_socket_path;
pub use state::DaemonState;
