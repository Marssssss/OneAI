//! HTTP transport (feature `http`) — same-origin SPA + JSON-RPC WebSocket.
//!
//! `oneai web` serves the prebuilt web UI static assets AND the `/ws`
//! JSON-RPC endpoint on a single port, so `npx oneai web` is a one-command
//! launch (no separate Vite dev server / app-server process for end users).
//! Mirrors `transport::serve_ws` but layers axum (static `ServeDir` +
//! `WebSocketUpgrade`) on top of the same `serve_connection` adapter seam —
//! zero duplicated JSON-RPC handling.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header::CACHE_CONTROL, HeaderValue},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tower::Layer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use oneai_bus::{EngineBus, InProcessBus};

use crate::adapter::serve_connection;
use crate::dispatcher::Dispatcher;
use crate::{
    SharedAppProbe, SharedConversationStore, SharedFeedbackStore, SharedHostAllowlistRpc,
    SharedScenarioStore,
};

/// Per-connection shared handles — the same six the ws transport threads
/// through `serve_ws_stream`, captured into the axum router state so the
/// upgrade handler can hand them to `serve_ws_axum`.
struct WebState {
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    feedback_store: SharedFeedbackStore,
    host_allowlist_rpc: SharedHostAllowlistRpc,
    probe: SharedAppProbe,
}

/// Channel buffer — matches `transport::CHANNEL_BUFFER` so the axum bridge
/// has the same backpressure posture as the plain ws transport.
const CHANNEL_BUFFER: usize = 256;

/// Bind the HTTP server: `GET /ws` → JSON-RPC WebSocket upgrade; everything
/// else → the SPA static dir (with `index.html` fallback for client-side
/// history routing). `static_dir = None` serves only `/ws` (no SPA).
///
/// Returns the bound address (the caller asked for `:0` to get an ephemeral
/// port) + a `JoinHandle` that completes when the server stops. The shared
/// `Dispatcher` yield-consumer is spawned here (mirrors `serve_all` lib.rs).
#[allow(clippy::too_many_arguments)]
pub async fn serve_web(
    addr: SocketAddr,
    static_dir: Option<PathBuf>,
    bus: Arc<InProcessBus>,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    feedback_store: SharedFeedbackStore,
    host_allowlist_rpc: SharedHostAllowlistRpc,
    probe: SharedAppProbe,
) -> std::io::Result<(JoinHandle<()>, SocketAddr)> {
    // One process-wide dispatcher (subscribe before spawning so the receiver
    // exists before any yield is emitted — same ordering as `serve_all`).
    let dispatcher = Dispatcher::default();
    let yield_rx = bus.subscribe_yields();
    tokio::spawn(dispatcher.clone().run(yield_rx));

    let state = Arc::new(WebState {
        bus,
        dispatcher,
        scenario_store,
        session_store,
        feedback_store,
        host_allowlist_rpc,
        probe,
    });

    let mut router = Router::new().route("/ws", get(ws_handler));
    if let Some(dir) = static_dir {
        // SPA history fallback: a path with no matching file returns
        // index.html so the client router owns deep links.
        let spa = ServeDir::new(dir.clone()).fallback(ServeFile::new(dir.join("index.html")));
        // `ServeDir` sends no Cache-Control, so a browser heuristically caches
        // `index.html` and pins the old hashed JS bundle after a `npm run build`
        // (the hash changes but the stale entry file still references the old
        // asset). Force revalidation on every request — `index.html` is tiny,
        // and the hashed `/assets/*` still benefit from ETag/Last-Modified.
        let spa =
            SetResponseHeaderLayer::overriding(CACHE_CONTROL, HeaderValue::from_static("no-cache"))
                .layer(spa);
        router = router.fallback_service(spa);
    }
    let app = router.with_state(state);

    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(%bound, "app-server: http+ws listener bound");

    Ok((
        tokio::spawn(async move {
            // `axum::serve` runs the accept loop until an error / the task is
            // aborted. The CLI wraps this in a `tokio::select!` with Ctrl-C.
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!(error = %e, "app-server: http serve ended");
            }
        }),
        bound,
    ))
}

/// `/ws` upgrade — delegates to `serve_ws_axum` with the shared state.
async fn ws_handler(ws: WebSocketUpgrade, State(st): State<Arc<WebState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        serve_ws_axum(
            socket,
            st.bus.clone(),
            st.dispatcher.clone(),
            st.scenario_store.clone(),
            st.session_store.clone(),
            st.feedback_store.clone(),
            st.host_allowlist_rpc.clone(),
            st.probe.clone(),
        )
    })
}

/// Bridge an axum `WebSocket` to `serve_connection`'s `mpsc<String>` seam —
/// the same shape `transport::serve_ws_stream` builds for a tungstenite
/// stream. Inbound text frames → `inbound_tx`; `outbound_rx` → outbound text
/// frames. `serve_connection` owns all JSON-RPC request/event handling.
#[allow(clippy::too_many_arguments)]
async fn serve_ws_axum(
    socket: WebSocket,
    bus: Arc<InProcessBus>,
    dispatcher: Dispatcher,
    scenario_store: SharedScenarioStore,
    session_store: SharedConversationStore,
    feedback_store: SharedFeedbackStore,
    host_allowlist_rpc: SharedHostAllowlistRpc,
    probe: SharedAppProbe,
) {
    let (mut ws_sink, mut ws_stream) = socket.split();

    let (inbound_tx, inbound_rx) = mpsc::channel::<String>(CHANNEL_BUFFER);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<String>(CHANNEL_BUFFER);

    // Inbound reader: axum text frame → inbound channel. Close/Err ends it.
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
                // Pings are auto-answered by axum; ignore binaries.
                _ => {}
            }
        }
    });

    // Outbound writer: outbound channel → axum text frame.
    let writer = tokio::spawn(async move {
        while let Some(line) = outbound_rx.recv().await {
            if ws_sink.send(Message::from(line)).await.is_err() {
                return;
            }
        }
    });

    serve_connection(
        bus,
        dispatcher,
        scenario_store,
        session_store,
        feedback_store,
        host_allowlist_rpc,
        probe,
        inbound_rx,
        outbound_tx,
    )
    .await;

    reader.abort();
    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        InMemoryConversationStore, InMemoryFeedbackStore, InMemoryHostAllowlistRpc,
        InMemoryScenarioStore, NullAppProbe,
    };
    use oneai_bus::InProcessBus;
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// `session/list` (synchronous — no engine driver needed) over the `/ws`
    /// upgrade of `serve_web`, proving the axum bridge round-trips a real
    /// JSON-RPC request/response through `serve_connection`.
    #[tokio::test]
    async fn serve_web_ws_roundtrips_session_list() {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let bus = Arc::new(InProcessBus::new().0);
        let scenario_store: SharedScenarioStore =
            Arc::new(InMemoryScenarioStore::from_seed(vec![]));
        let session_store: SharedConversationStore = Arc::new(InMemoryConversationStore::new());
        let feedback_store: SharedFeedbackStore = Arc::new(InMemoryFeedbackStore::new());
        let host_allowlist_rpc: SharedHostAllowlistRpc = Arc::new(InMemoryHostAllowlistRpc::new());

        let (_handle, bound) = serve_web(
            "127.0.0.1:0".parse().unwrap(),
            None,
            bus,
            scenario_store,
            session_store,
            feedback_store,
            host_allowlist_rpc,
            Arc::new(NullAppProbe),
        )
        .await
        .expect("serve_web bind");

        let url = format!("ws://{bound}/ws");
        let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .expect("ws connect");
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/list",
            "params": null,
        });
        ws.send(Message::Text(serde_json::to_string(&req).unwrap().into()))
            .await
            .unwrap();

        // Read until the session/list response (id=1, result is an array).
        for _ in 0..50 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let s = t.to_string();
                    let val: Value = match serde_json::from_str(&s) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if val.get("id") == Some(&json!(1)) && val.get("result").is_some() {
                        assert!(
                            val["result"]["sessions"].is_array(),
                            "session list result.sessions is an array, got: {val}"
                        );
                        return;
                    }
                }
                _ => break,
            }
        }
        panic!("did not observe session/list response over serve_web /ws");
    }

    /// `GET /` returns `index.html` from the static dir with HTTP 200 —
    /// proves the SPA assets are served same-origin.
    #[tokio::test]
    async fn serve_web_serves_index_html_at_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = dir.path().join("index.html");
        std::fs::write(&index, "<html><body>oneai web</body></html>").expect("write index");

        let bus = Arc::new(InProcessBus::new().0);
        let (_handle, bound) = serve_web(
            "127.0.0.1:0".parse().unwrap(),
            Some(dir.path().to_path_buf()),
            bus,
            Arc::new(InMemoryScenarioStore::from_seed(vec![])),
            Arc::new(InMemoryConversationStore::new()),
            Arc::new(InMemoryFeedbackStore::new()),
            Arc::new(InMemoryHostAllowlistRpc::new()),
            Arc::new(NullAppProbe),
        )
        .await
        .expect("serve_web bind");

        let mut stream = TcpStream::connect(bound).await.expect("tcp connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("write request");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read response");
        let body = String::from_utf8_lossy(&buf);
        assert!(body.contains("200 OK"), "expected 200, got: {body}");
        assert!(
            body.contains("oneai web"),
            "expected index body, got: {body}"
        );
    }
}
