//! Platform-specific local byte streams for the daemon protocol.

#[cfg(unix)]
pub(crate) use tokio::net::{UnixListener as LocalListener, UnixStream as LocalStream};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{LocalListener, LocalStream};
