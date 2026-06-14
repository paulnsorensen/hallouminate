//! Local daemon: the single owner of the configured LanceDB ground
//! directory, repository registry, per-corpus mutation locks, and the
//! global write-lane.
//!
//! See `.cheese/specs/repository-daemon-wikis.md` for the design.
//! Operational summary:
//!
//! - One daemon per user-local endpoint (`HALLOUMINATE_SOCKET`,
//!   `$XDG_RUNTIME_DIR/hallouminate/daemon.sock`, or
//!   `~/.cache/hallouminate/daemon.sock`). On unix this is a Unix domain
//!   socket; on Windows the transport derives an owner-only named pipe from
//!   the same path (`transport.rs`).
//! - Single-instance enforced via `std::fs::File::try_lock` on `<socket>.lock`
//!   (cross-platform: `flock` on unix, `LockFileEx` on Windows).
//! - Interactive CLI subcommands (`ground`, `index`, …) become clients of
//!   the daemon and fail loudly when it is unreachable rather than silently
//!   auto-starting — the user sees a clear hint to run `hallouminate daemon`.
//! - The non-interactive MCP `serve` transport calls `ensure_daemon_running`
//!   to spawn a detached daemon when one is not already up. The flock keeps
//!   concurrent spawns safe: only one daemon wins the lock, the rest exit.
//!
//! Lock order across the dispatcher is documented in `state.rs`.

mod bootstrap;
mod client;
mod dispatch;
mod ipc;
mod lifecycle;
mod server;
mod socket;
mod state;
mod transport;
mod watch;

pub use bootstrap::ensure_daemon_running;
pub use client::{
    DaemonClient, DaemonRpcError, client_for, connect_at, daemon_client, daemon_client_unavailable,
};
pub use ipc::{
    AddMarkdownRequest, AddMarkdownResult, CorpusEntry, DaemonRequest, DaemonRequestPayload,
    DaemonResponse, DeleteMarkdownRequest, DeleteMarkdownResult, ErrorKind, GroundRequest,
    GroundResult, IndexRequest, ListCorporaResult, ListFilesRequest, ListFilesResult,
    ListTreeRequest, ListTreeResult, ReadMarkdownRequest, ReadMarkdownResult,
};
pub use lifecycle::{DaemonStatus, restart, restart_with, status, stop};
pub use server::{DaemonArgs, run_daemon, serve, spawn_signal_handlers};
pub use socket::daemon_socket_path;
pub use state::DaemonState;
