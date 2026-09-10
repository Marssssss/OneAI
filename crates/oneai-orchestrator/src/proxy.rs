//! WS reverse proxy (D3): frontend ↔ orchestrator ↔ session container.
//!
//! Pure passthrough — JSON-RPC payloads are never parsed. The axum 0.8
//! client side and the tokio-tungstenite 0.26 upstream side share the same
//! tungstenite wire types, so message conversion is lossless.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as AxumMessage, WebSocket as AxumWebSocket};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as TtMessage;

use crate::fsm::SessionEntry;

// ─── Message conversion (unit-tested per variant) ─────────────────────────────

/// Client (axum) → upstream (tungstenite). All axum variants map 1:1; the
/// two crates' `Utf8Bytes` types are distinct, so text goes via `&str`.
pub fn axum_to_tungstenite(m: AxumMessage) -> TtMessage {
    match m {
        AxumMessage::Text(t) => TtMessage::Text(t.as_str().into()),
        AxumMessage::Binary(b) => TtMessage::Binary(b),
        AxumMessage::Ping(p) => TtMessage::Ping(p),
        AxumMessage::Pong(p) => TtMessage::Pong(p),
        AxumMessage::Close(f) => {
            TtMessage::Close(
                f.map(|cf| tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: cf.code.into(),
                    reason: cf.reason.as_str().into(),
                }),
            )
        }
    }
}

/// Upstream (tungstenite) → client (axum). `Frame` (raw, never surfaced in
/// practice) is dropped → `None`.
pub fn tungstenite_to_axum(m: TtMessage) -> Option<AxumMessage> {
    match m {
        TtMessage::Text(t) => Some(AxumMessage::Text(t.as_str().into())),
        TtMessage::Binary(b) => Some(AxumMessage::Binary(b)),
        TtMessage::Ping(p) => Some(AxumMessage::Ping(p)),
        TtMessage::Pong(p) => Some(AxumMessage::Pong(p)),
        TtMessage::Close(f) => Some(AxumMessage::Close(f.map(|cf| {
            axum::extract::ws::CloseFrame {
                code: cf.code.into(),
                reason: cf.reason.as_str().into(),
            }
        }))),
        TtMessage::Frame(_) => None,
    }
}

// ─── Connection guard ─────────────────────────────────────────────────────────

/// Increments `active_conns` on construction, decrements on drop (the idle
/// sweep vetoes hibernation while any connection is attached).
pub struct ConnGuard {
    entry: Arc<SessionEntry>,
}

impl ConnGuard {
    /// Attach: bump the entry's active-connection count.
    pub fn new(entry: Arc<SessionEntry>) -> Self {
        entry.active_conns.fetch_add(1, Ordering::Relaxed);
        entry.touch();
        Self { entry }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.entry.active_conns.fetch_sub(1, Ordering::Relaxed);
        self.entry.touch();
    }
}

// ─── Bidirectional pump ───────────────────────────────────────────────────────

/// Upstream connect attempts before giving up (the engine may still be
/// binding its port right after container start).
const UPSTREAM_CONNECT_ATTEMPTS: u32 = 5;
/// Backoff between upstream connect attempts.
const UPSTREAM_CONNECT_BACKOFF: Duration = Duration::from_millis(300);

/// Run the bidirectional proxy between an upgraded client socket and the
/// session container's ws endpoint. Returns when either side closes or
/// errors; both sides are torn down together.
pub async fn proxy_pump(mut client: AxumWebSocket, upstream_url: String, entry: Arc<SessionEntry>) {
    let _guard = ConnGuard::new(entry.clone());

    let upstream = match connect_upstream(&upstream_url).await {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!(
                session = %entry.spec.session_id,
                upstream = %upstream_url,
                error = %e,
                "upstream ws connect failed; closing client"
            );
            let _ = client.close().await;
            return;
        }
    };

    let (mut client_sink, mut client_stream) = client.split();
    let (mut upstream_sink, mut upstream_stream) = upstream.split();

    let session_id = entry.spec.session_id.clone();
    let entry_c2u = entry.clone();
    // Forwarder A: client → upstream.
    let c2u = tokio::spawn(async move {
        while let Some(msg) = client_stream.next().await {
            let Ok(msg) = msg else { break };
            entry_c2u.touch();
            if upstream_sink.send(axum_to_tungstenite(msg)).await.is_err() {
                break;
            }
        }
        let _ = upstream_sink.send(TtMessage::Close(None)).await;
    });

    let entry_u2c = entry.clone();
    // Forwarder B: upstream → client.
    let u2c = tokio::spawn(async move {
        while let Some(msg) = upstream_stream.next().await {
            let Ok(msg) = msg else { break };
            entry_u2c.touch();
            let Some(msg) = tungstenite_to_axum(msg) else {
                continue; // raw Frame — drop
            };
            if client_sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = client_sink.send(AxumMessage::Close(None)).await;
    });

    // Either direction ending tears down both (the spawned halves finish
    // once their sinks close).
    tokio::select! {
        _ = c2u => {},
        _ = u2c => {},
    }
    tracing::debug!(session = %session_id, "ws proxy pump finished");
}

/// Connect to the upstream container ws endpoint with bounded retries.
async fn connect_upstream(
    url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    let mut last_err = None;
    for attempt in 0..UPSTREAM_CONNECT_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(UPSTREAM_CONNECT_BACKOFF).await;
        }
        match tokio_tungstenite::connect_async(url).await {
            Ok((ws, _resp)) => return Ok(ws),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("at least one attempt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_roundtrip() {
        let m = AxumMessage::Text("hello".into());
        let t = axum_to_tungstenite(m);
        assert!(matches!(&t, TtMessage::Text(s) if s.as_str() == "hello"));
        match tungstenite_to_axum(t).unwrap() {
            AxumMessage::Text(s) => assert_eq!(s.as_str(), "hello"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn binary_roundtrip() {
        let m = AxumMessage::Binary(vec![1, 2, 3].into());
        let t = axum_to_tungstenite(m);
        assert!(matches!(&t, TtMessage::Binary(b) if b.as_ref() == [1, 2, 3]));
        match tungstenite_to_axum(t).unwrap() {
            AxumMessage::Binary(b) => assert_eq!(b.as_ref(), [1, 2, 3]),
            _ => panic!("expected Binary"),
        }
    }

    #[test]
    fn ping_pong_roundtrip() {
        let t = axum_to_tungstenite(AxumMessage::Ping(vec![9].into()));
        assert!(matches!(&t, TtMessage::Ping(p) if p.as_ref() == [9]));
        assert!(matches!(
            tungstenite_to_axum(t).unwrap(),
            AxumMessage::Ping(ref p) if p.as_ref() == [9]
        ));
        let t = axum_to_tungstenite(AxumMessage::Pong(vec![7].into()));
        assert!(matches!(&t, TtMessage::Pong(p) if p.as_ref() == [7]));
        assert!(matches!(
            tungstenite_to_axum(t).unwrap(),
            AxumMessage::Pong(ref p) if p.as_ref() == [7]
        ));
    }

    #[test]
    fn close_roundtrip_with_code_and_reason() {
        let m = AxumMessage::Close(Some(axum::extract::ws::CloseFrame {
            code: 1001,
            reason: "going away".into(),
        }));
        let t = axum_to_tungstenite(m);
        match &t {
            TtMessage::Close(Some(cf)) => {
                assert_eq!(u16::from(cf.code), 1001);
                assert_eq!(cf.reason.as_str(), "going away");
            }
            _ => panic!("expected Close"),
        }
        match tungstenite_to_axum(t).unwrap() {
            AxumMessage::Close(Some(cf)) => {
                assert_eq!(cf.code, 1001);
                assert_eq!(cf.reason.as_str(), "going away");
            }
            _ => panic!("expected Close"),
        }
    }

    #[test]
    fn close_none_roundtrip() {
        let t = axum_to_tungstenite(AxumMessage::Close(None));
        assert!(matches!(t, TtMessage::Close(None)));
        assert!(matches!(
            tungstenite_to_axum(TtMessage::Close(None)).unwrap(),
            AxumMessage::Close(None)
        ));
    }

    // Note: `TtMessage::Frame` cannot be constructed in tests — tungstenite
    // keeps `Frame` private; the drop arm in `tungstenite_to_axum` is
    // defensive only.

    #[tokio::test]
    async fn conn_guard_increments_and_decrements() {
        use crate::registry::tests::test_spec;
        let entry = Arc::new(SessionEntry::new_creating(test_spec("g")));
        assert_eq!(entry.active_conns.load(Ordering::Relaxed), 0);
        {
            let _g1 = ConnGuard::new(entry.clone());
            let _g2 = ConnGuard::new(entry.clone());
            assert_eq!(entry.active_conns.load(Ordering::Relaxed), 2);
        }
        assert_eq!(entry.active_conns.load(Ordering::Relaxed), 0);
    }
}
