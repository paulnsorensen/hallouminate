//! Platform-specific local byte streams for the daemon protocol.

#[cfg(unix)]
pub use tokio::net::{UnixListener as LocalListener, UnixStream as LocalStream};

#[cfg(windows)]
mod windows;
#[cfg(all(windows, test))]
pub(crate) use windows::pipe_name;
#[cfg(windows)]
pub use windows::{LocalListener, LocalStream};
