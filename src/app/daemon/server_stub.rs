//! Non-unix stub for the daemon accept loop (`server.rs`).
//!
//! Keeps the public API byte-identical with the unix build (`DaemonArgs`,
//! `run_daemon`, `serve`, `spawn_signal_handlers`) so `cli.rs` dispatch and
//! the `mod.rs` re-exports compile on non-unix targets. The accept loop, the
//! flock single-instance lock, and the signal wiring are all Unix-socket
//! bound; their Windows named-pipes port is #48 stage 2. Every entry point
//! here fails loudly with the unsupported error.

use std::path::{Path, PathBuf};

use super::client::daemon_unsupported;
use super::state::DaemonState;

#[derive(Debug, Default, Clone)]
pub struct DaemonArgs {
    pub config: Option<PathBuf>,
}

pub async fn run_daemon(_args: DaemonArgs) -> anyhow::Result<()> {
    Err(daemon_unsupported())
}

pub async fn serve(_state: &DaemonState, _socket_path: &Path) -> anyhow::Result<()> {
    Err(daemon_unsupported())
}

pub fn spawn_signal_handlers(_state: &DaemonState) {}
