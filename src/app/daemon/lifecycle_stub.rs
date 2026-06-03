//! Non-unix stub for daemon lifecycle ops (`lifecycle.rs`).
//!
//! `status` / `stop` / `restart` drive a running Unix-socket daemon; there is
//! no Windows transport until the named-pipes port (#48 stage 2). `DaemonStatus`
//! keeps the unix enum's shape so `cli.rs` dispatch and the `mod.rs` re-exports
//! compile on non-unix targets; every operation fails loudly with the
//! unsupported error.

use super::client::daemon_unsupported;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    Running,
    NotRunning,
}

pub async fn status() -> anyhow::Result<DaemonStatus> {
    Err(daemon_unsupported())
}

pub async fn stop() -> anyhow::Result<()> {
    Err(daemon_unsupported())
}

pub async fn restart() -> anyhow::Result<()> {
    Err(daemon_unsupported())
}

#[doc(hidden)]
pub async fn restart_with<F, Fut>(_respawn: F) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    Err(daemon_unsupported())
}
