//! Platform-specific local byte streams for the daemon protocol.

#[cfg(unix)]
pub(crate) use tokio::net::{UnixListener as LocalListener, UnixStream as LocalStream};

#[cfg(windows)]
mod windows;
#[cfg(all(windows, test))]
pub(crate) use windows::pipe_name;
#[cfg(windows)]
pub(crate) use windows::{LocalListener, LocalStream, is_pipe_busy};
