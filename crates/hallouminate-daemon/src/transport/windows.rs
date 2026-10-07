use std::io;
use std::os::windows::ffi::OsStrExt;
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

use crate::windows_security::{OwnerSecurity, UserSid, current_user_sid, process_user_sid};

fn pipe_name(path: &Path) -> io::Result<String> {
    let path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            hallouminate_domain::common::best_effort_canonical(path)
        }
        Err(error) => return Err(error),
    };
    let mut hash = blake3::Hasher::new();
    for unit in path.as_os_str().encode_wide() {
        hash.update(&unit.to_le_bytes());
    }
    hash.update(current_user_sid()?.to_string().as_bytes());
    Ok(format!(
        r"\\.\pipe\hallouminate-{}",
        hash.finalize().to_hex()
    ))
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
    pending: Mutex<NamedPipeServer>,
}

impl LocalListener {
    /// Binds the local endpoint identified by a filesystem path.
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        crate::windows_security::restrict_path(path)?;
        let name = pipe_name(path)?;
        let pending = create_server(&name, Instance::First)?;
        Ok(Self {
            name,
            pending: Mutex::new(pending),
        })
    }

    /// Accepts the next connection while keeping another pipe instance available.
    pub async fn accept(&self) -> io::Result<(LocalStream, ())> {
        let mut pending = self.pending.lock().await;
        pending.connect().await?;
        let next = create_server(&self.name, Instance::Additional)?;
        let connected = std::mem::replace(&mut *pending, next);
        Ok((LocalStream::Server(connected), ()))
    }
}

/// Carries bytes between local daemon clients and servers.
pub enum LocalStream {
    /// A connection opened by a client.
    Client(NamedPipeClient),
    /// A connection accepted by a server.
    Server(NamedPipeServer),
}

impl LocalStream {
    /// Connects to a local endpoint and verifies the server's Windows account.
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let name = pipe_name(path.as_ref())?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match ClientOptions::new().open(&name) {
                Ok(client) => {
                    let stream = Self::Client(client);
                    if stream.peer_user()? != current_user_sid()? {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "daemon belongs to another Windows account",
                        ));
                    }
                    return Ok(stream);
                }
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn peer_user(&self) -> io::Result<UserSid> {
        let mut pid = 0;
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
                    current_user_sid().expect("SID")
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
