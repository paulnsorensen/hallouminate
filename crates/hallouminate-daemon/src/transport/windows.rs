use std::io;
use std::io::Write as _;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::Mutex;
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};

use crate::windows_security::{
    OwnerSecurity, UserSid, create_owner_only_file, current_user_sid, process_user_sid,
    random_nonce,
};

/// Longest wait for a busy pipe when the caller sets no deadline.
const BUSY_RETRY_BUDGET: Duration = Duration::from_secs(5);
const BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(10);
/// Pause before `accept` reports a failed pipe-instance creation again.
const INSTANCE_RETRY_DELAY: Duration = Duration::from_millis(50);
const NONCE_HEX_LEN: usize = 64;

fn pipe_name_for_nonce(nonce: &str) -> String {
    format!(r"\\.\pipe\hallouminate-{nonce}")
}

/// Reads the pipe name that the daemon recorded in the owner-only marker file.
///
/// The name carries a random nonce, so another account cannot compute it or
/// create the pipe first. A missing or incomplete marker reports `NotFound`.
pub(crate) fn pipe_name(path: &Path) -> io::Result<String> {
    let nonce = std::fs::read_to_string(path)?;
    if nonce.len() != NONCE_HEX_LEN || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "daemon endpoint marker is not ready",
        ));
    }
    Ok(pipe_name_for_nonce(&nonce))
}

/// Reports whether `error` means that every pipe instance is in use.
pub fn is_pipe_busy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
}

enum Instance {
    First,
    Additional,
}

fn create_server(name: &str, instance: Instance) -> io::Result<NamedPipeServer> {
    let descriptor = OwnerSecurity::new()?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options.reject_remote_clients(true);
    match instance {
        Instance::First => {
            options.first_pipe_instance(true);
        }
        Instance::Additional => {
            options.first_pipe_instance(false);
        }
    }
    // SAFETY: `attributes` and the descriptor behind it outlive the call.
    unsafe {
        options.create_with_security_attributes_raw(
            name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
}

/// Accepts connections to an account-restricted local endpoint.
pub struct LocalListener {
    name: String,
    /// The instance that waits for the next client. `None` after a failed
    /// replacement, until the next `accept` creates it again.
    pending: Mutex<Option<NamedPipeServer>>,
}

impl LocalListener {
    /// Creates the pipe and records its random name in an owner-only marker file at `path`.
    ///
    /// Fails when `path` exists, like a Unix socket bind.
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let nonce = random_nonce()?;
        let name = pipe_name_for_nonce(&nonce);
        let mut marker = create_owner_only_file(path)?;
        let pending = create_server(&name, Instance::First).and_then(|pending| {
            marker.write_all(nonce.as_bytes())?;
            Ok(pending)
        });
        drop(marker);
        match pending {
            Ok(pending) => Ok(Self {
                name,
                pending: Mutex::new(Some(pending)),
            }),
            Err(error) => {
                let _ = std::fs::remove_file(path);
                Err(error)
            }
        }
    }

    /// Accepts the next connection while keeping another pipe instance available.
    pub async fn accept(&self) -> io::Result<(LocalStream, ())> {
        let mut slot = self.pending.lock().await;
        if slot.is_none() {
            match create_server(&self.name, Instance::Additional) {
                Ok(server) => *slot = Some(server),
                Err(error) => {
                    tokio::time::sleep(INSTANCE_RETRY_DELAY).await;
                    return Err(error);
                }
            }
        }
        let Some(pending) = slot.as_mut() else {
            return Err(io::Error::other("pipe instance missing after creation"));
        };
        pending.connect().await?;
        let Some(connected) = slot.take() else {
            return Err(io::Error::other("pipe instance missing after connect"));
        };
        *slot = create_server(&self.name, Instance::Additional)
            .inspect_err(|error| {
                tracing::warn!(
                    target: "hallouminate::daemon",
                    error = %error,
                    "failed to create the next pipe instance; retrying on the next accept",
                );
            })
            .ok();
        Ok((LocalStream::Server(connected), ()))
    }
}

/// A connected named-pipe endpoint.
///
/// Named pipes have no half-close, so `shutdown` does not send end-of-file.
/// A request must end with a line feed.
pub enum LocalStream {
    /// A connection opened by a client.
    Client(NamedPipeClient),
    /// A connection accepted by a server.
    Server(NamedPipeServer),
}

impl LocalStream {
    /// Connects to the daemon, waiting a bounded time while every pipe instance is busy.
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::connect_until(path, tokio::time::Instant::now() + BUSY_RETRY_BUDGET).await
    }

    /// Connects to the daemon, retrying a busy pipe until `deadline`.
    pub(crate) async fn connect_until(
        path: impl AsRef<Path>,
        deadline: tokio::time::Instant,
    ) -> io::Result<Self> {
        let name = pipe_name(path.as_ref())?;
        loop {
            match ClientOptions::new().open(&name) {
                Ok(client) => {
                    let stream = Self::Client(client);
                    if stream.peer_user()? != *current_user_sid()? {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "daemon belongs to another Windows account",
                        ));
                    }
                    return Ok(stream);
                }
                Err(error) if is_pipe_busy(&error) && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(BUSY_RETRY_INTERVAL).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn peer_user(&self) -> io::Result<UserSid> {
        let mut pid = 0;
        // SAFETY: each handle comes from a live pipe that `self` owns, and
        // `pid` is a valid out pointer.
        let success = match self {
            Self::Client(client) => unsafe {
                GetNamedPipeServerProcessId(client.as_raw_handle(), &mut pid)
            },
            Self::Server(server) => unsafe {
                GetNamedPipeClientProcessId(server.as_raw_handle(), &mut pid)
            },
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        process_user_sid(pid)
    }

    /// Splits a connection into independently owned read and write halves.
    pub fn into_split(self) -> (ReadHalf<Self>, WriteHalf<Self>) {
        tokio::io::split(self)
    }
}

impl AsyncRead for LocalStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(stream) => Pin::new(stream).poll_read(cx, buffer),
            Self::Server(stream) => Pin::new(stream).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for LocalStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Client(stream) => Pin::new(stream).poll_write(cx, buffer),
            Self::Server(stream) => Pin::new(stream).poll_write(cx, buffer),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(stream) => Pin::new(stream).poll_flush(cx),
            Self::Server(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Server(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn pipe_exchanges_frames_and_verifies_account_identity() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let endpoint = directory.path().join("daemon.sock");
        let listener = LocalListener::bind(&endpoint).expect("bind pipe");
        let server = tokio::spawn(async move {
            for expected in 0..4_u8 {
                let (mut stream, ()) = listener.accept().await.expect("accept client");
                assert_eq!(
                    stream.peer_user().expect("client SID"),
                    *current_user_sid().expect("SID")
                );
                assert_eq!(stream.read_u8().await.expect("request"), expected);
                stream.write_u8(expected + 1).await.expect("response");
            }
        });
        for expected in 0..4_u8 {
            let mut stream = LocalStream::connect(&endpoint).await.expect("connect");
            stream.write_u8(expected).await.expect("request");
            assert_eq!(stream.read_u8().await.expect("response"), expected + 1);
        }
        server.await.expect("server task");
        assert!(LocalStream::connect(&endpoint).await.is_err());
    }

    #[tokio::test]
    async fn pipe_name_comes_from_a_random_marker_nonce() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first.sock");
        let second = directory.path().join("second.sock");
        let _first = LocalListener::bind(&first).expect("bind first pipe");
        let _second = LocalListener::bind(&second).expect("bind second pipe");
        assert_ne!(
            pipe_name(&first).expect("first name"),
            pipe_name(&second).expect("second name")
        );
        std::fs::remove_file(&second).expect("remove marker");
        std::fs::write(&second, b"").expect("write empty marker");
        let error = pipe_name(&second).expect_err("an empty marker has no pipe name");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn cancellation_keeps_a_pipe_available_and_excludes_a_second_listener() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let endpoint = directory.path().join("daemon.sock");
        let listener = LocalListener::bind(&endpoint).expect("bind pipe");
        assert!(LocalListener::bind(&endpoint).is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
        let client = LocalStream::connect(&endpoint)
            .await
            .expect("connect after cancellation");
        let (server, ()) = listener.accept().await.expect("accept after cancellation");
        assert_eq!(
            server.peer_user().expect("peer SID"),
            client.peer_user().expect("server SID")
        );
    }

    #[tokio::test]
    async fn existing_endpoint_aliases_resolve_to_the_same_pipe() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let endpoint = directory.path().join("daemon.sock");
        let listener = LocalListener::bind(&endpoint).expect("bind pipe");
        let canonical = std::fs::canonicalize(&endpoint).expect("canonical endpoint");
        let dotted = directory.path().join(".").join("daemon.sock");
        let uppercase = directory.path().join("DAEMON.SOCK");
        if uppercase.exists() {
            assert_eq!(
                pipe_name(&endpoint).expect("name"),
                pipe_name(&uppercase).expect("case alias")
            );
        }
        assert_eq!(
            pipe_name(&endpoint).expect("name"),
            pipe_name(&canonical).expect("canonical name")
        );
        assert_eq!(
            pipe_name(&endpoint).expect("name"),
            pipe_name(&dotted).expect("dotted name")
        );
        let client = LocalStream::connect(&canonical)
            .await
            .expect("connect alias");
        let (server, ()) = listener.accept().await.expect("accept alias");
        assert_eq!(
            server.peer_user().expect("peer"),
            client.peer_user().expect("server")
        );
    }
}
