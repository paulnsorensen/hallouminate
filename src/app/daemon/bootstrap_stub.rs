//! Non-unix stub for daemon auto-spawn (`bootstrap.rs`).
//!
//! `ensure_daemon_running` spawns and adopts a detached Unix-socket daemon on
//! the MCP `serve` path; there is no Windows transport until the named-pipes
//! port (#48 stage 2). The stub fails loudly with the unsupported error so the
//! `mod.rs` re-export compiles on non-unix targets.

use super::client::daemon_unsupported;

pub async fn ensure_daemon_running() -> anyhow::Result<()> {
    Err(daemon_unsupported())
}
