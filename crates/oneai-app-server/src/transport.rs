//! Transports — bridge concrete byte streams (stdio / IPC / WebSocket) to the
//! adapter's raw-JSON-`String` channels.
//!
//! Each transport does exactly two things, symmetric to the adapter's
//! [`serve_connection`](crate::adapter::serve_connection) contract:
//!
//! - **inbound**: read framed messages from the concrete stream, send each as a
//!   `String` on `inbound_tx`.
//! - **outbound**: receive `String`s from `outbound_rx`, write each framed to
//!   the concrete stream.
//!
//! Framing: newline-terminated JSON for stdio/IPC (one `\n` per message, same
//! as `oneai-bus`'s wire codec); one WebSocket text frame per message for ws.
//! The adapter is framing-agnostic — it only sees parsed JSON-RPC messages.
//!
//! Every transport spawns [`serve_connection`](crate::adapter::serve_connection)
//! per accepted connection with a fresh channel pair and the shared
//! [`Dispatcher`](crate::dispatcher::Dispatcher).

use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use oneai_bus::InProcessBus;
use oneai_supervisor::{IpcListener, IpcStream};

use crate::adapter::serve_connection;
use crate::dispatcher::Dispatcher;
use crate::{SharedAppProbe, SharedConversationStore, SharedScenarioStore};

/// Channel buffer for the per-connection inbound/outbound JSON queues. Turns a
/// slow frontend into back-pressure rather than unbounded memory.
const CHANNEL_BUFFER: usize = 256;

/// Run the stdio transport: exactly one connection (the spawning process's
/// stdin/stdout — an IDE LSP-style spawn). Returns the serve task handle; the
/// two pump tasks are detached and abort when the serve task ends.
pub fn serve_stdio(
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    probe: SharedAppProbe,
) -> JoinHandle<()> {
    let (inbound_tx, inbound_rx) = mpsc::channel(CHANNEL_BUFFER);
    let (outbound_tx, outbound_rx) = mpsc::channel(CHANNEL_BUFFER);

    // Inbound: stdin → lines → inbound_tx.
    spawn_stdin_reader(inbound_tx);
    // Outbound: outbound_rx → stdout lines.
    spawn_stdout_writer(outbound_rx);

    tokio::spawn(serve_connection(
        bus,
        dispatcher,
        scenario_store,
        session_store,
        probe,
        inbound_rx,
        outbound_tx,
    ))
}

/// Run the IPC transport: bind a `oneai-supervisor` `IpcListener` (Unix domain
/// socket / Windows named pipe) and accept connections, each bridged to a
/// fresh `serve_connection`.
pub async fn serve_ipc(
    path: &Path,
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    probe: SharedAppProbe,
) -> std::io::Result<JoinHandle<()>> {
    let mut listener = IpcListener::bind(path).await?;
    tracing::info!(path = %path.display(), "app-server: ipc listener bound");
    Ok(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    let bus = bus.clone();
                    let dispatcher = dispatcher.clone();
                    let scenario_store = scenario_store.clone();
                    let session_store = session_store.clone();
                    let probe = probe.clone();
                    tokio::spawn(async move {
                        serve_line_stream(
                            stream,
                            bus,
                            dispatcher,
                            scenario_store,
                            session_store,
                            probe,
                        )
                        .await;
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "app-server: ipc accept failed; retrying");
                    continue;
                }
            }
        }
    }))
}

/// Bridge one newline-JSON byte stream (an `IpcStream`) to `serve_connection`.
async fn serve_line_stream(
    stream: IpcStream,
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    probe: SharedAppProbe,
) {
    let (read, mut write) = tokio::io::split(stream);
    let (inbound_tx, inbound_rx) = mpsc::channel::<String>(CHANNEL_BUFFER);
    let (outbound_tx, outbound_rx) = mpsc::channel::<String>(CHANNEL_BUFFER);

    // Inbound: read lines → inbound_tx.
    let in_tx = inbound_tx.clone();
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => return, // clean EOF
                Ok(_) => {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    if trimmed.is_empty() {
                        continue;
                    }
                    if in_tx.send(trimmed.to_string()).await.is_err() {
                        return; // serve_connection gone
                    }
                }
                Err(_) => return,
            }
        }
    });

    // Outbound: outbound_rx → write lines.
    let writer = tokio::spawn(async move {
        let mut rx = outbound_rx;
        while let Some(line) = rx.recv().await {
            // Each message is one JSON object; frame with a trailing newline.
            if write.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            if write.write_all(b"\n").await.is_err() {
                return;
            }
            let _ = write.flush().await;
        }
    });

    serve_connection(
        bus,
        dispatcher,
        scenario_store,
        session_store,
        probe,
        inbound_rx,
        outbound_tx,
    )
    .await;
    reader.abort();
    writer.abort();
}

/// Run the WebSocket transport (feature `ws`): bind a TCP listener and accept
/// browser/JS WebSocket connections, each bridged to `serve_connection`.
/// Returns the listener task handle + the bound address (so callers/tests can
/// discover an ephemeral port).
#[cfg(feature = "ws")]
pub async fn serve_ws(
    addr: std::net::SocketAddr,
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    probe: SharedAppProbe,
) -> std::io::Result<(JoinHandle<()>, std::net::SocketAddr)> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(%bound, "app-server: ws listener bound");
    Ok((
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let bus = bus.clone();
                        let dispatcher = dispatcher.clone();
                        let scenario_store = scenario_store.clone();
                        let session_store = session_store.clone();
                        let probe = probe.clone();
                        tokio::spawn(async move {
                            match tokio_tungstenite::accept_async(stream).await {
                                Ok(ws) => {
                                    serve_ws_stream(
                                        ws,
                                        bus,
                                        dispatcher,
                                        scenario_store,
                                        session_store,
                                        probe,
                                    )
                                    .await
                                }
                                Err(e) => {
                                    tracing::warn!(%peer, error = %e, "app-server: ws handshake failed")
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "app-server: ws accept failed; retrying");
                        continue;
                    }
                }
            }
        }),
        bound,
    ))
}

/// Bridge one WebSocket to `serve_connection`. Inbound = WS text frames →
/// inbound_tx; outbound = outbound_rx → WS text frames.
#[cfg(feature = "ws")]
async fn serve_ws_stream(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    probe: SharedAppProbe,
) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws_sink, mut ws_stream) = ws.split();
    let (inbound_tx, inbound_rx) = mpsc::channel::<String>(CHANNEL_BUFFER);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<String>(CHANNEL_BUFFER);

    let reader = tokio::spawn(async move {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let s = text.to_string();
                    if inbound_tx.send(s).await.is_err() {
                        return;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => return,
                // Ignore pings/binaries; tungstenite auto-responds to Ping.
                _ => {}
            }
        }
    });

    let writer = tokio::spawn(async move {
        while let Some(line) = outbound_rx.recv().await {
            if ws_sink.send(Message::Text(line.into())).await.is_err() {
                return;
            }
        }
    });

    serve_connection(
        bus,
        dispatcher,
        scenario_store,
        session_store,
        probe,
        inbound_rx,
        outbound_tx,
    )
    .await;
    reader.abort();
    writer.abort();
}

// ── native-messaging pumps ──────────────────────────────────────────────────
//
// Chrome/Firefox native messaging wire format: each message is a 4-byte
// little-endian length prefix followed by that many bytes of UTF-8 JSON.
// Unlike stdio's newline framing, the length-prefix lets JSON contain newlines
// and is what `chrome.runtime.connectNative` / `port.postMessage` speak on the
// host side. The browser itself handles the framing on *its* side; the host
// (us) must read/write the 4B-LE prefix on stdin/stdout.
//
// CRITICAL: in this mode stdout is the message stream — nothing else may be
// written to it. `cmd_app_server` routes all banner/info output to stderr
// (LSP convention) so stdio AND native-messaging both keep stdout clean.

/// Chrome caps native-messaging messages at 1 MiB; we accept up to 4 MiB as a
/// sanity ceiling to avoid OOM on a corrupt length prefix, then give up.
const MAX_NM_MESSAGE: u32 = 4 * 1024 * 1024;

/// Run the native-messaging transport: stdin/stdout framed with a 4-byte
/// little-endian length prefix. Exactly one connection (the spawning browser
/// process's stdin/stdout). Returns the serve task handle; the two pump tasks
/// are detached and abort when the serve task ends.
pub fn serve_native_messaging(
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    probe: SharedAppProbe,
) -> JoinHandle<()> {
    let (inbound_tx, inbound_rx) = mpsc::channel(CHANNEL_BUFFER);
    let (outbound_tx, outbound_rx) = mpsc::channel(CHANNEL_BUFFER);

    spawn_nm_reader(inbound_tx);
    spawn_nm_writer(outbound_rx);

    tokio::spawn(serve_connection(
        bus,
        dispatcher,
        scenario_store,
        session_store,
        probe,
        inbound_rx,
        outbound_tx,
    ))
}

/// Read one native-messaging-framed message (4B-LE len + JSON) from `r`.
/// Returns `None` on clean EOF; `None` on any error (the pump treats both as
/// "host closed" — native messaging has no retry semantics on a malformed
/// stream). Extracted from the pump so framing is unit-testable without IO.
async fn nm_read_message<R>(r: &mut R) -> Option<String>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    // EOF before any byte = parent gone (clean); error/short read likewise.
    r.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf);
    if len == 0 {
        return Some(String::new());
    }
    if len > MAX_NM_MESSAGE {
        // Corrupt length prefix — refuse to allocate. Drop the stream.
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await.ok()?;
    // The browser sends UTF-8 JSON; a non-UTF-8 stream is malformed — skip
    // rather than kill the whole host connection.
    String::from_utf8(buf).ok()
}

/// Write one native-messaging-framed message (4B-LE len + JSON) to `w`.
/// Returns false on write failure so the pump can tear down.
async fn nm_write_message<W>(w: &mut W, msg: &str) -> bool
where
    W: AsyncWrite + Unpin,
{
    let len = msg.len() as u32;
    if w.write_all(&len.to_le_bytes()).await.is_err() {
        return false;
    }
    if w.write_all(msg.as_bytes()).await.is_err() {
        return false;
    }
    w.flush().await.is_ok()
}

fn spawn_nm_reader(inbound_tx: mpsc::Sender<String>) {
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        loop {
            match nm_read_message(&mut reader).await {
                Some(msg) => {
                    if msg.is_empty() {
                        continue;
                    }
                    if inbound_tx.send(msg).await.is_err() {
                        return; // serve_connection gone
                    }
                }
                None => return, // stdin EOF or malformed stream
            }
        }
    });
}

fn spawn_nm_writer(outbound_rx: mpsc::Receiver<String>) {
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut rx = outbound_rx;
        while let Some(msg) = rx.recv().await {
            if !nm_write_message(&mut stdout, &msg).await {
                return;
            }
        }
    });
}

// ── stdio pumps ───────────────────────────────────────────────────────────────

fn spawn_stdin_reader(inbound_tx: mpsc::Sender<String>) {
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => return, // stdin EOF (parent process closed)
                Ok(_) => {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    if trimmed.is_empty() {
                        continue;
                    }
                    if inbound_tx.send(trimmed.to_string()).await.is_err() {
                        return; // serve_connection gone
                    }
                }
                Err(_) => return,
            }
        }
    });
}

fn spawn_stdout_writer(outbound_rx: mpsc::Receiver<String>) {
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut rx = outbound_rx;
        while let Some(line) = rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            if stdout.write_all(b"\n").await.is_err() {
                return;
            }
            let _ = stdout.flush().await;
        }
    });
}

#[cfg(test)]
mod nm_tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt};

    #[tokio::test]
    async fn nm_frame_round_trip_single() {
        // Writer frames, reader decodes — one message, payload with a newline
        // (which newline framing could not survive, motivating the length prefix).
        let (mut client, mut server) = duplex(8 * 1024);
        let payload =
            r#"{"jsonrpc":"2.0","method":"turn/run","params":{"content":[{"text":"a\nb"}]}}"#;
        assert!(nm_write_message(&mut client, payload).await, "write");

        let got = nm_read_message(&mut server).await;
        assert_eq!(got.as_deref(), Some(payload));
    }

    #[tokio::test]
    async fn nm_frame_round_trip_multiple_back_to_back() {
        // Two messages written before any read — the reader must peel both
        // using the length prefix (no delimiter between them).
        let (mut client, mut server) = duplex(8 * 1024);
        nm_write_message(&mut client, r#"{"id":1}"#).await;
        nm_write_message(&mut client, r#"{"id":2,"x":true}"#).await;

        assert_eq!(
            nm_read_message(&mut server).await.as_deref(),
            Some(r#"{"id":1}"#)
        );
        assert_eq!(
            nm_read_message(&mut server).await.as_deref(),
            Some(r#"{"id":2,"x":true}"#)
        );
    }

    #[tokio::test]
    async fn nm_length_prefix_is_little_endian() {
        // Verify the on-wire bytes: 4-byte LE length then payload. A Chrome
        // host must produce exactly this — the browser parses it on the other
        // end. Hand-construct the expected bytes and check the writer matches.
        let (mut client, mut server) = duplex(8 * 1024);
        let msg = "hello";
        nm_write_message(&mut client, msg).await;
        let mut buf = [0u8; 5 + 4];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf[0..4], &(msg.len() as u32).to_le_bytes());
        assert_eq!(&buf[4..], msg.as_bytes());
    }

    #[tokio::test]
    async fn nm_read_clean_eof_returns_none() {
        // No bytes at all = parent gone. Must not hang or error — the pump
        // treats None as "host closed" and exits.
        let (_client, mut server) = duplex(8 * 1024);
        drop(_client); // close the write end → read EOF
        assert_eq!(nm_read_message(&mut server).await, None);
    }

    #[tokio::test]
    async fn nm_oversized_length_prefix_is_refused() {
        // A bogus length prefix beyond MAX_NM_MESSAGE must not cause an
        // unbounded allocation — refuse (None) instead.
        let (mut client, mut server) = duplex(8 * 1024);
        client
            .write_all(&(MAX_NM_MESSAGE + 1).to_le_bytes())
            .await
            .unwrap();
        // Don't send a body — the reader should bail on the length alone.
        assert_eq!(nm_read_message(&mut server).await, None);
    }
}
