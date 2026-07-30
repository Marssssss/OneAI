//! axum webhook receiving surface — `POST /gateway/{platform}` (+ WeChat GET
//! handshake). Shared base for Phase 3.2 (cron `/cron/fire`) and 3.5 (A2A
//! server axum).
//!
//! Per-platform event parsing (Feishu JSON, WeChat XML) lives in the
//! feature-gated adapters; the web layer dispatches by URL path segment to a
//! registered [`WebhookHandler`], **acks the platform immediately** (so the
//! platform's HTTP timeout isn't blown by a slow agent turn), and spawns
//! [`Gateway::handle_inbound`] for any parsed [`MessageEvent`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, warn};

use crate::error::Result;
use crate::event::MessageEvent;
use crate::gateway::Gateway;

/// What a [`WebhookHandler`] returns: the HTTP ack body to send back to the
/// platform *now*, and an optional [`MessageEvent`] to drive a turn in the
/// background (None = handshake/verification, no turn).
#[derive(Debug, Default)]
pub struct WebhookAck {
    pub status: u16,
    pub body: String,
    pub event: Option<MessageEvent>,
}

impl WebhookAck {
    /// Empty 200 OK ack, no turn.
    pub fn ok() -> Self {
        Self {
            status: 200,
            body: String::new(),
            event: None,
        }
    }
}

/// Per-platform inbound webhook parser. Registered per platform name; the web
/// layer dispatches the URL path segment to the matching handler.
#[async_trait]
pub trait WebhookHandler: Send + Sync {
    /// Platform name this handler parses for (matches the URL segment).
    fn platform(&self) -> &str;

    /// Parse one inbound webhook request.
    ///
    /// `query` is the raw query string (WeChat GET handshake carries
    /// `signature`/`timestamp`/`nonce`/`echostr` there). Implementations verify
    /// the platform signature here and return the ack body the platform expects
    /// (Feishu echoes `challenge`; WeChat GET echoes `echostr`).
    async fn parse(&self, headers: &HeaderMap, body: &[u8], query: &str) -> Result<WebhookAck>;
}

/// Shared state for the webhook router.
#[derive(Clone)]
pub struct WebhookState {
    pub gateway: Arc<Gateway>,
    pub handlers: Arc<HashMap<String, Arc<dyn WebhookHandler>>>,
}

impl WebhookState {
    pub fn new(gateway: Arc<Gateway>) -> Self {
        Self {
            gateway,
            handlers: Arc::new(HashMap::new()),
        }
    }

    /// Builder: register a per-platform webhook handler.
    pub fn with(mut self, handler: Arc<dyn WebhookHandler>) -> Self {
        let mut map = (*self.handlers).clone();
        map.insert(handler.platform().to_string(), handler);
        self.handlers = Arc::new(map);
        self
    }
}

/// Build the axum router. `POST /gateway/{platform}` for event pushes,
/// `GET /gateway/{platform}` for handshake (WeChat URL verification).
pub fn build_router(state: WebhookState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    Router::new()
        .route("/gateway/{platform}", post(post_inbound).get(get_handshake))
        .layer(cors)
        .with_state(state)
}

/// Configuration for the webhook server.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub addr: SocketAddr,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([0, 0, 0, 0], 9090)),
        }
    }
}

/// Start the webhook HTTP server. Blocks until the server stops.
pub async fn serve(config: WebConfig, state: WebhookState) -> Result<()> {
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    tracing::info!("OneAI gateway webhook listening on http://{}", config.addr);
    axum::serve(listener, router).await?;
    Ok(())
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn post_inbound(
    State(state): State<WebhookState>,
    Path(platform): Path<String>,
    headers: HeaderMap,
    raw_query: axum::extract::RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let query = raw_query.0.unwrap_or_default();
    let handler = match state.handlers.get(&platform) {
        Some(h) => h.clone(),
        None => {
            warn!(platform = %platform, "no webhook handler registered");
            return (StatusCode::NOT_FOUND, "no handler").into_response();
        }
    };

    let ack = match handler.parse(&headers, &body, &query).await {
        Ok(a) => a,
        Err(e) => {
            warn!(platform = %platform, error = %e, "webhook parse error");
            return (StatusCode::BAD_REQUEST, format!("parse error: {e}")).into_response();
        }
    };

    // If the parsed event carries a turn, drive it in the background — the
    // platform already got its ack (below), don't block the HTTP response on
    // a potentially long agent turn.
    if let Some(event) = ack.event {
        let gw = state.gateway.clone();
        tokio::spawn(async move {
            if let Err(e) = gw.handle_inbound(event).await {
                warn!(error = %e, "handle_inbound failed");
            }
        });
    }

    ack_response(ack.status, ack.body)
}

async fn get_handshake(
    State(state): State<WebhookState>,
    Path(platform): Path<String>,
    headers: HeaderMap,
    raw_query: axum::extract::RawQuery,
) -> Response {
    let query = raw_query.0.unwrap_or_default();
    let handler = match state.handlers.get(&platform) {
        Some(h) => h.clone(),
        None => return (StatusCode::NOT_FOUND, "no handler").into_response(),
    };
    // Handshake GET has no body; pass empty bytes.
    let ack = match handler.parse(&headers, &[], &query).await {
        Ok(a) => a,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("handshake error: {e}")).into_response()
        }
    };
    debug!(platform = %platform, "handshake ack");
    ack_response(ack.status, ack.body)
}

fn ack_response(status: u16, body: String) -> Response {
    let is_json = body.trim_start().starts_with('{');
    let mut resp = body.into_response();
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    if is_json {
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
    }
    resp
}

// ─── Loopback webhook handler (for the e2e smoke test + local dev) ───────────

/// A trivial loopback webhook handler: the POST body is a JSON [`MessageEvent`]
/// (or `{"text":..,"channel":{..},"sender":{..}}`). No signature. The GET
/// handshake just 200s. Used by the e2e test and `oneai gateway serve` when no
/// real platform adapter is configured.
pub struct LoopbackWebhookHandler;

#[async_trait]
impl WebhookHandler for LoopbackWebhookHandler {
    fn platform(&self) -> &str {
        "loopback"
    }

    async fn parse(&self, _headers: &HeaderMap, body: &[u8], _query: &str) -> Result<WebhookAck> {
        if body.is_empty() {
            return Ok(WebhookAck::ok());
        }
        let event: MessageEvent = serde_json::from_slice(body)?;
        Ok(WebhookAck {
            status: 200,
            body: "{\"ok\":true}".to_string(),
            event: Some(event),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ChannelId, MessageEvent, Sender};
    use crate::gateway::Gateway;
    use crate::platform::{MessagePlatform, PlatformRegistry};
    use crate::profile::ProfileRoute;
    use crate::runner::{GatewayRunner, TurnOutcome};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct EchoRunner;
    #[async_trait]
    impl GatewayRunner for EchoRunner {
        async fn run_turn(&self, _sid: &str, task: &str) -> TurnOutcome {
            TurnOutcome::Done {
                final_answer: format!("echo: {task}"),
                completed: true,
                iterations: 1,
            }
        }
    }

    // Capture platform that records sends.
    struct Cap(Mutex<Vec<String>>);
    #[async_trait]
    impl MessagePlatform for Cap {
        fn name(&self) -> &str {
            "loopback"
        }
        async fn send(&self, _ch: &ChannelId, text: &str) -> Result<()> {
            self.0.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    async fn spawn_server() -> (SocketAddr, Arc<Cap>, tokio::task::JoinHandle<()>) {
        let cap = Arc::new(Cap(Mutex::new(Vec::new())));
        let mut reg = PlatformRegistry::new();
        reg.register(cap.clone());
        let gw = Arc::new(Gateway::new(
            Arc::new(EchoRunner),
            reg,
            crate::directory::ChannelDirectory::in_memory(),
            ProfileRoute::new("coding"),
        ));
        let state = WebhookState::new(gw).with(Arc::new(LoopbackWebhookHandler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let router = build_router(state);
            axum::serve(listener, router).await.unwrap();
        });
        (addr, cap, handle)
    }

    #[tokio::test]
    async fn e2e_post_drives_turn_and_replies_via_send() {
        let (addr, cap, _h) = spawn_server().await;
        let ev = MessageEvent::new(
            ChannelId::new("loopback", "c1"),
            Sender::anonymous("u1"),
            "hi",
        );
        let body = serde_json::to_vec(&ev).unwrap();
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/gateway/loopback"))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // The turn runs in a spawned task — poll for the reply to land.
        for _ in 0..50 {
            if !cap.0.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(cap.0.lock().unwrap().as_slice(), ["echo: hi"]);
    }
}
