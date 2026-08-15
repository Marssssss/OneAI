//! IPC transport — an OS-abstracted bidirectional byte stream + listener.
//!
//! The supervisor daemon listens on a local IPC endpoint and native-app
//! clients connect to it. The concrete [`IpcListener`] / [`IpcStream`] types
//! select the backend at compile time:
//!
//! - **Unix**: a `tokio::net::UnixListener` / `UnixStream` bound at
//!   `~/.oneai/server.sock`.
//! - **Windows**: a `tokio::net::windows::named_pipe` server/client at
//!   `\\.\pipe\oneai-supervisor`.
//! - **In-memory**: a `tokio::io::duplex` pair, for portable in-proc tests
//!   (and the "in-proc supervised tokio task" mode — no real socket needed).
//!
//! Framing (one `\n`-terminated JSON line per message) lives in [`crate::protocol`];
//! this module only moves bytes.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A connected bidirectional IPC stream.
pub struct IpcStream(IpcStreamInner);

enum IpcStreamInner {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    PipeClient(tokio::net::windows::named_pipe::NamedPipeClient),
    #[cfg(windows)]
    PipeServer(tokio::net::windows::named_pipe::NamedPipeServer),
    Mem(tokio::io::DuplexStream),
}

// `match` over the cfg-gated enum needs an arm for every variant that exists
// in the current build; the `#[cfg]`-gated arms below cover exactly those.
macro_rules! dispatch_read {
    ($self:expr, $cx:expr, $buf:expr) => {
        match &mut $self.0 {
            #[cfg(unix)]
            IpcStreamInner::Unix(s) => Pin::new(s).poll_read($cx, $buf),
            #[cfg(windows)]
            IpcStreamInner::PipeClient(s) => Pin::new(s).poll_read($cx, $buf),
            #[cfg(windows)]
            IpcStreamInner::PipeServer(s) => Pin::new(s).poll_read($cx, $buf),
            IpcStreamInner::Mem(s) => Pin::new(s).poll_read($cx, $buf),
        }
    };
}

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        dispatch_read!(self.get_mut(), cx, buf)
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().0 {
            #[cfg(unix)]
            IpcStreamInner::Unix(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            IpcStreamInner::PipeClient(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            IpcStreamInner::PipeServer(s) => Pin::new(s).poll_write(cx, buf),
            IpcStreamInner::Mem(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().0 {
            #[cfg(unix)]
            IpcStreamInner::Unix(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            IpcStreamInner::PipeClient(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            IpcStreamInner::PipeServer(s) => Pin::new(s).poll_flush(cx),
            IpcStreamInner::Mem(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().0 {
            #[cfg(unix)]
            IpcStreamInner::Unix(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            IpcStreamInner::PipeClient(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            IpcStreamInner::PipeServer(s) => Pin::new(s).poll_shutdown(cx),
            IpcStreamInner::Mem(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl IpcStream {
    /// Wrap a Unix domain stream.
    #[cfg(unix)]
    pub fn from_unix(stream: tokio::net::UnixStream) -> Self {
        Self(IpcStreamInner::Unix(stream))
    }

    /// Wrap a Windows named-pipe client (the connecting side).
    #[cfg(windows)]
    pub fn from_pipe_client(client: tokio::net::windows::named_pipe::NamedPipeClient) -> Self {
        Self(IpcStreamInner::PipeClient(client))
    }

    /// Wrap a Windows named-pipe server (the accepted side).
    #[cfg(windows)]
    pub fn from_pipe_server(server: tokio::net::windows::named_pipe::NamedPipeServer) -> Self {
        Self(IpcStreamInner::PipeServer(server))
    }

    /// Wrap an in-memory duplex stream.
    pub fn from_duplex(stream: tokio::io::DuplexStream) -> Self {
        Self(IpcStreamInner::Mem(stream))
    }
}

/// An IPC listener — accepts inbound [`IpcStream`] connections.
pub struct IpcListener(IpcListenerInner);

enum IpcListenerInner {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    Pipe {
        name: String,
        /// A pre-created server instance awaiting its first client. tokio's
        /// named-pipe model is one instance per connection, so on each
        /// `accept` we connect this instance, then create a fresh one
        /// (`first_pipe_instance(false)`) for the next round.
        next: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    },
    Mem(tokio::sync::mpsc::Receiver<IpcStream>),
}

impl IpcListener {
    /// Bind a real OS IPC endpoint at `path`.
    ///
    /// On Unix this is a Unix domain socket (any stale socket file is
    /// unlinked first so restarts rebind cleanly). On Windows it is a named
    /// pipe at `\\.\pipe\<name>`.
    pub async fn bind(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(path);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let listener = tokio::net::UnixListener::bind(path)?;
            Ok(Self(IpcListenerInner::Unix(listener)))
        }
        #[cfg(windows)]
        {
            let name = to_pipe_name(path);
            let first = tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)?;
            Ok(Self(IpcListenerInner::Pipe {
                name,
                next: Some(first),
            }))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no IPC backend on this platform",
            ))
        }
    }

    /// Accept the next inbound connection.
    pub async fn accept(&mut self) -> io::Result<IpcStream> {
        match &mut self.0 {
            #[cfg(unix)]
            IpcListenerInner::Unix(l) => {
                let (stream, _) = l.accept().await?;
                Ok(IpcStream::from_unix(stream))
            }
            #[cfg(windows)]
            IpcListenerInner::Pipe { name, next } => {
                // Take the pre-created instance, wait for a client, then
                // create the next instance for the following accept.
                let mut server = next
                    .take()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "listener closed"))?;
                server.connect().await?;
                *next = Some(
                    tokio::net::windows::named_pipe::ServerOptions::new()
                        .first_pipe_instance(false)
                        .create(name)?,
                );
                Ok(IpcStream::from_pipe_server(server))
            }
            IpcListenerInner::Mem(rx) => rx
                .recv()
                .await
                .ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionReset, "listener closed")),
        }
    }
}

/// Connect a real OS IPC endpoint at `path`.
pub async fn connect(path: &Path) -> io::Result<IpcStream> {
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(path).await?;
        Ok(IpcStream::from_unix(stream))
    }
    #[cfg(windows)]
    {
        let name = to_pipe_name(path);
        // tokio's named_pipe module has no `Client::connect` (that type doesn't
        // exist) — the client is built via `ClientOptions::new().open(&name)`
        // (note: `.open`, not `.create` — `create` is `ServerOptions`'s method;
        // `open` returns a `NamedPipeClient`, which `from_pipe_client`
        // wraps). The handle connects on first I/O; `open` is synchronous.
        let client = tokio::net::windows::named_pipe::ClientOptions::new().open(&name)?;
        Ok(IpcStream::from_pipe_client(client))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no IPC backend on this platform",
        ))
    }
}

#[cfg(windows)]
fn to_pipe_name(path: &Path) -> String {
    // Flatten any path into a valid \\.\pipe\<name>. Backslashes and spaces
    // are not allowed in the name segment.
    let flat: String = path
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ' ' => '_',
            other => other,
        })
        .collect();
    format!("\\\\.\\pipe\\{}", flat)
}

// ─── In-memory transport (portable tests + in-proc direct mode) ─────────────

/// A handle that "connects" to an in-memory listener — each call yields a
/// fresh paired stream whose other end the listener's `accept` returns.
pub struct MemListenerHandle {
    tx: tokio::sync::mpsc::Sender<IpcStream>,
}

impl MemListenerHandle {
    /// Create a fresh client stream; the server end will be accepted by the
    /// paired listener.
    pub fn connect(&self) -> IpcStream {
        let (client, server) = tokio::io::duplex(8 * 1024);
        let _ = self.tx.try_send(IpcStream::from_duplex(server));
        IpcStream::from_duplex(client)
    }
}

/// Create a paired in-memory listener + handle (no socket, works everywhere).
pub fn mem_listener(buffer: usize) -> (MemListenerHandle, IpcListener) {
    let (tx, rx) = tokio::sync::mpsc::channel(buffer);
    (
        MemListenerHandle { tx },
        IpcListener(IpcListenerInner::Mem(rx)),
    )
}

// ─── Path helpers ───────────────────────────────────────────────────────────

/// The `~/.oneai` root (falls back to `/tmp` like `oneai-skill`'s discovery).
fn oneai_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".oneai")
}

/// Default directory holding `instances.json` (the instance registry).
pub fn default_server_dir() -> PathBuf {
    oneai_root().join("server")
}

/// Default IPC socket path.
pub fn default_socket_path() -> PathBuf {
    #[cfg(unix)]
    {
        oneai_root().join("server.sock")
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\oneai-supervisor")
    }
    #[cfg(not(any(unix, windows)))]
    {
        oneai_root().join("server.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mem_roundtrip() {
        let (handle, mut listener) = mem_listener(4);
        let client = handle.connect();
        let server = tokio::spawn(async move { listener.accept().await.unwrap() });
        let mut client = client;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        client.write_all(b"hello\n").await.unwrap();
        let mut s = server.await.unwrap();
        let mut buf = [0u8; 6];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uds_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        let mut listener = IpcListener::bind(&sock).await.unwrap();
        let connect_task = tokio::spawn(async move {
            let mut c = connect(&sock).await.unwrap();
            use tokio::io::AsyncWriteExt;
            c.write_all(b"ping\n").await.unwrap();
            c
        });
        let mut server = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0u8; 5];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping\n");
        server.write_all(b"pong\n").await.unwrap();
        let mut c = connect_task.await.unwrap();
        let mut b2 = [0u8; 5];
        c.read_exact(&mut b2).await.unwrap();
        assert_eq!(&b2, b"pong\n");
    }

    #[test]
    fn default_paths_exist() {
        let dir = default_server_dir();
        assert!(dir.to_string_lossy().contains(".oneai"));
        let _sock = default_socket_path();
    }
}
