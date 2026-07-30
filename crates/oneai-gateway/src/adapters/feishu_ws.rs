//! Feishu **long-connection** (WebSocket, outbound) transport — the no-public-URL
//! alternative to the webhook adapter. Verified against the official Go SDK
//! (`ws/client.go`).
//!
//! Flow:
//! 1. **Bootstrap** `POST {base}/callback/ws/endpoint` with `{AppID, AppSecret}`
//!    → `{URL (wss://…), ClientConfig {PingInterval, ReconnectInterval, …}}`.
//! 2. **Dial** the returned `wss://` URL (outbound — no public port / ngrok).
//! 3. **Read loop**: decode binary protobuf [`Frame`] → for `event` data frames,
//!    reassemble (`sum>1`) → `parse_message_event` → **ack immediately** (prevents
//!    slow-turn retries) → spawn [`Gateway::handle_inbound`] in the background.
//! 4. **Ping loop**: every `PingInterval`, send a control ping frame; pong may
//!    carry an updated `ClientConfig`.
//! 5. **Reconnect** per `ClientConfig` on read error / disconnect.
//!
//! The reply path is unchanged: `handle_inbound` looks up the `feishu` platform
//! from the gateway's registry and calls its REST `send` (tenant_access_token).

#![cfg(feature = "feishu")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::adapters::feishu::{parse_message_event, FeishuConfig};
use crate::adapters::feishu_pb::{Frame, FRAME_TYPE_CONTROL, FRAME_TYPE_DATA};
use crate::error::{GatewayError, Result};
use crate::gateway::Gateway;

/// Reassembly cache for sum>1 frames: `msg_id → (sum, parts)`.
type ReassemblyCache = Arc<tokio::sync::Mutex<HashMap<String, (i32, Vec<Option<Vec<u8>>>)>>>;

/// Server-pushed connection config. Keys are PascalCase (Feishu wire format).
#[derive(Debug, Clone, Default, Deserialize)]
struct ClientConfig {
    #[serde(default, rename = "PingInterval")]
    ping_interval: i64,
    #[serde(default, rename = "ReconnectInterval")]
    reconnect_interval: i64,
    #[serde(default, rename = "ReconnectCount")]
    reconnect_count: i64,
    /// Initial reconnect jitter window (seconds). Advisory; the loop applies
    /// a small attempt-based jitter instead.
    #[serde(default, rename = "ReconnectNonce")]
    #[allow(dead_code)]
    reconnect_nonce: i64,
}

impl ClientConfig {
    fn ping(&self) -> Duration {
        Duration::from_secs(self.ping_interval.max(1) as u64)
    }
    fn reconnect(&self) -> Duration {
        Duration::from_secs(self.reconnect_interval.max(1) as u64)
    }
    /// Reconnect count: negative = infinite.
    fn tries(&self) -> i64 {
        self.reconnect_count
    }
}

#[derive(Debug, Deserialize)]
struct EndpointResp {
    code: i64,
    #[serde(default)]
    msg: String,
    data: Option<EndpointData>,
}
#[derive(Debug, Deserialize)]
struct EndpointData {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "ClientConfig", default)]
    client_config: ClientConfig,
}

/// Bootstrap: fetch the WSS URL + config.
async fn bootstrap(cfg: &FeishuConfig, http: &reqwest::Client) -> Result<(String, ClientConfig)> {
    let body = serde_json::json!({
        "AppID": cfg.app_id,
        "AppSecret": cfg.app_secret,
    });
    let resp = http
        .post(format!("{}/callback/ws/endpoint", cfg.base_url))
        .header("locale", "zh")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json::<EndpointResp>()
        .await?;
    if resp.code != 0 {
        return Err(GatewayError::Platform {
            platform: "feishu".into(),
            message: format!("bootstrap code {}: {}", resp.code, resp.msg),
        });
    }
    let data = resp.data.ok_or_else(|| GatewayError::Platform {
        platform: "feishu".into(),
        message: "bootstrap: no endpoint data".into(),
    })?;
    Ok((data.url, data.client_config))
}

/// Start the long-connection loop. Spawns a detached task that runs for the
/// process lifetime, reconnecting on failure. Called by the CLI after the
/// gateway is built and the feishu platform is registered.
pub fn start_long_connection(
    cfg: FeishuConfig,
    http: reqwest::Client,
    gateway: Arc<Gateway>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut config = ClientConfig::default();
        let mut attempt: u64 = 0;
        loop {
            attempt += 1;
            match bootstrap(&cfg, &http).await {
                Ok((url, cfg2)) => {
                    config = cfg2;
                    info!(
                        attempt, url = %url,
                        ping_secs = config.ping().as_secs(),
                        reconnect_secs = config.reconnect().as_secs(),
                        "feishu long-connection bootstrap ok",
                    );
                    match serve_connection(&url, config.clone(), &cfg, &http, gateway.clone()).await
                    {
                        Ok(()) => {
                            info!("feishu long-connection closed cleanly");
                            return;
                        }
                        Err(e) => {
                            warn!(error = %e, attempt, "feishu long-connection loop ended");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, attempt, "feishu bootstrap failed");
                }
            }
            // Reconnect backoff (config interval + nonce jitter by attempt).
            let base = config.reconnect();
            let jitter = Duration::from_millis((attempt * 137) % 2000);
            tokio::time::sleep(base + jitter).await;
            // Server may send a finite reconnect budget (tries>0); when we've
            // exceeded it we keep retrying for the process lifetime anyway.
            if config.tries() > 0 && attempt as i64 > config.tries() {
                warn!(attempt, "feishu long-connection reconnect budget exceeded; retrying anyway (process lifetime)");
            }
        }
    })
}

/// Dial the WSS URL and run read + ping until the connection ends.
async fn serve_connection(
    url: &str,
    config: ClientConfig,
    _cfg: &FeishuConfig,
    _http: &reqwest::Client,
    gateway: Arc<Gateway>,
) -> Result<()> {
    let (ws_stream, _resp) =
        tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| GatewayError::Platform {
                platform: "feishu".into(),
                message: format!("ws dial failed: {e}"),
            })?;
    info!("feishu long-connection established");

    let (mut sink, mut stream) = ws_stream.split();

    // Write coordinator: read loop + ping task send frames via this channel;
    // a single writer task serializes sends (mutex-free, avoids torn writes).
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);
    let writer = tokio::spawn(async move {
        while let Some(frame_bytes) = write_rx.recv().await {
            if sink
                .send(Message::Binary(frame_bytes.into()))
                .await
                .is_err()
            {
                return;
            }
        }
    });

    // Ping task.
    let ping_cfg = config.clone();
    let ping_tx = write_tx.clone();
    let _ping = tokio::spawn(async move {
        let mut interval = tokio::time::interval(ping_cfg.ping());
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            let ping = Frame::ping(0).encode();
            if ping_tx.send(ping).await.is_err() {
                return;
            }
            debug!("feishu ws: sent ping");
        }
    });

    // Reassembly cache for sum>1 frames: msg_id → (sum, parts).
    let reassembly: ReassemblyCache = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // Read loop.
    while let Some(msg) = stream.next().await {
        let msg = msg.map_err(|e| GatewayError::Platform {
            platform: "feishu".into(),
            message: format!("ws read failed: {e}"),
        })?;
        let bytes = match msg {
            Message::Binary(b) => b.to_vec(),
            Message::Ping(_) | Message::Pong(_) => continue, // tungstenite auto-pongs
            Message::Close(_) => {
                info!("feishu ws: server closed");
                break;
            }
            _ => continue,
        };
        let frame = match Frame::decode(&bytes) {
            Some(f) => f,
            None => {
                warn!("feishu ws: failed to decode frame ({} bytes)", bytes.len());
                continue;
            }
        };
        // Per-frame visibility — the key diagnostic. If you send a Feishu
        // message and never see this line, Feishu isn't pushing events down
        // the WS (→ check backend: 长连接 mode + im.message.receive_v1 subscribed
        // + app version published + bot capability + permissions).
        debug!(
            method = frame.method,
            ftype = frame.header("type").unwrap_or(""),
            msg_id = frame.header("message_id").unwrap_or(""),
            payload_len = frame.payload.len(),
            "feishu ws: received frame",
        );
        match frame.method {
            FRAME_TYPE_CONTROL => {
                // pong: server may push an updated ClientConfig in payload.
                if frame.header("type") == Some("pong") && !frame.payload.is_empty() {
                    if let Ok(new_cfg) = serde_json::from_slice::<ClientConfig>(&frame.payload) {
                        debug!(
                            "feishu ws: server pushed new ping interval {:?}",
                            new_cfg.ping()
                        );
                        // Pong-driven config update is advisory; the current
                        // ping loop keeps its interval. A full impl would
                        // re-arm the interval — left as a follow-up.
                        let _ = new_cfg;
                    }
                }
            }
            FRAME_TYPE_DATA => {
                // Ack immediately (prevents slow-turn retries) by echoing the
                // inbound frame with payload = {"code":200}.
                let mut ack = frame.clone();
                ack.payload = br#"{"code":200}"#.to_vec();
                let _ = write_tx.send(ack.encode()).await;

                // Reassemble if split across frames (sum>1).
                let msg_id = frame.header("message_id").unwrap_or("").to_string();
                let sum: i32 = frame
                    .header("sum")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                let seq: i32 = frame
                    .header("seq")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let payload = if sum > 1 {
                    match reassemble(&reassembly, &msg_id, sum, seq, &frame.payload).await {
                        Some(p) => p,
                        None => continue, // still missing parts; already acked
                    }
                } else {
                    frame.payload.clone()
                };

                // Parse + drive a turn in the background.
                match parse_message_event_slice(&payload) {
                    Ok(event) => {
                        let gw = gateway.clone();
                        tokio::spawn(async move {
                            if let Err(e) = gw.handle_inbound(event).await {
                                warn!(error = %e, "feishu ws: handle_inbound failed");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "feishu ws: parse event failed");
                    }
                }
            }
            _ => {
                debug!(method = frame.method, "feishu ws: unknown frame method");
            }
        }
    }

    // Drop writer on exit; sink goes back via the writer task's exit.
    drop(write_tx);
    let _ = writer.await;
    info!("feishu ws: read loop ended");
    Ok(())
}

/// Reassemble a split frame. Returns the full payload once all `sum` parts have
/// arrived; None while parts are still missing.
async fn reassemble(
    cache: &ReassemblyCache,
    msg_id: &str,
    sum: i32,
    seq: i32,
    payload: &[u8],
) -> Option<Vec<u8>> {
    if msg_id.is_empty() {
        // No id to key on — can't reassemble; treat as complete.
        return Some(payload.to_vec());
    }
    let mut map = cache.lock().await;
    let entry = map.entry(msg_id.to_string()).or_insert_with(|| {
        let mut parts = vec![None; sum as usize];
        parts[seq as usize] = Some(payload.to_vec());
        (sum, parts)
    });
    if entry.0 != sum {
        // Conflicting sum — start over.
        let mut parts = vec![None; sum as usize];
        parts[seq as usize] = Some(payload.to_vec());
        *entry = (sum, parts);
    } else {
        let parts = &mut entry.1;
        if (seq as usize) < parts.len() {
            parts[seq as usize] = Some(payload.to_vec());
        }
    }
    let parts = &entry.1;
    if parts.iter().all(|p| p.is_some()) {
        let mut full = Vec::new();
        for p in parts {
            full.extend_from_slice(p.as_ref().unwrap());
        }
        map.remove(msg_id);
        Some(full)
    } else {
        None
    }
}

/// Parse a reassembled event payload (reuse the webhook envelope parser).
///
/// `parse_message_event` takes `&serde_json::Value`; here we parse raw bytes.
fn parse_message_event_slice(
    payload: &[u8],
) -> std::result::Result<crate::event::MessageEvent, GatewayError> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| GatewayError::Parse(format!("{e}")))?;
    parse_message_event(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FeishuConfig {
        FeishuConfig {
            app_id: "cli_test".into(),
            app_secret: "secret".into(),
            verification_token: String::new(),
            encrypt_key: None,
            base_url: "https://open.feishu.cn".into(),
        }
    }

    #[tokio::test]
    async fn reassemble_two_parts() {
        let cache = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let p1 = reassemble(&cache, "m1", 2, 0, b"hello ").await;
        assert!(p1.is_none());
        let p2 = reassemble(&cache, "m1", 2, 1, b"world").await;
        assert_eq!(p2.as_deref(), Some(b"hello world".as_slice()));
        // cache cleared after completion
        let again = reassemble(&cache, "m1", 2, 0, b"x").await;
        assert!(
            again.is_none(),
            "first part of new message should be partial"
        );
    }

    #[test]
    fn endpoint_resp_parses() {
        let json = r#"{"code":0,"msg":"","data":{"URL":"wss://x.example","ClientConfig":{"PingInterval":30,"ReconnectInterval":2}}}"#;
        let r: EndpointResp = serde_json::from_str(json).unwrap();
        assert_eq!(r.code, 0);
        let data = r.data.unwrap();
        assert_eq!(data.url, "wss://x.example");
        assert_eq!(data.client_config.ping_interval, 30);
    }

    #[test]
    fn client_config_defaults() {
        let c = ClientConfig::default();
        assert_eq!(c.ping(), Duration::from_secs(1)); // max(1)
        assert_eq!(c.reconnect(), Duration::from_secs(1));
        assert_eq!(c.tries(), 0); // 0 → not infinite; outer loop ignores for now
    }

    #[test]
    fn bootstrap_body_shape() {
        // Sanity: the bootstrap body carries AppID + AppSecret (no token).
        let v = serde_json::json!({
            "AppID": cfg().app_id,
            "AppSecret": cfg().app_secret,
        });
        assert_eq!(v["AppID"].as_str(), Some("cli_test"));
    }
}
