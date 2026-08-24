//! A2A Server Host — serves OneAI agent capabilities via the A2A JSON-RPC
//! protocol over a real axum HTTP server.
//!
//! The A2AServerHost makes a OneAI agent discoverable and reachable by remote
//! A2A agents. It serves:
//! - `GET /.well-known/agent-card` → AgentCard discovery (no auth)
//! - `POST /` → A2A JSON-RPC protocol endpoint (shared-secret Bearer auth)
//!
//! ## What runs the agent
//!
//! `oneai-a2a` sits below `oneai-app`, so it cannot call `run_agent` directly.
//! The [`A2ARunner`] seam (mirroring `oneai_gateway::GatewayRunner`) is the one
//! place the server touches the agent: the CLI injects an App-backed runner
//! via [`A2AServerHost::with_runner`]; the server core calls
//! [`A2ARunner::run_task`] on `tasks/send` and [`A2ARunner::run_task_streaming`]
//! on `tasks/sendSubscribe`. The default [`PlaceholderRunner`] reproduces the
//! pre-3.5 ack so unit tests stay green.
//!
//! ## Auth
//!
//! A shared-secret Bearer token (`Authorization: Bearer <secret>`, env
//! `ONEAI_A2A_SECRET`) gates `POST /`. This mirrors the cron `/cron/fire`
//! receiver posture (`oneai_scheduler::oneshot`) — constant-time compared, and
//! **the server refuses to start without a secret** (evolution-plan §3.2
//! deviated JWT→shared-secret per supply-chain discipline; A2A follows suit).
//! The AgentCard advertises `authentication.schemes = ["bearer"]` truthfully.
//!
//! ## Streaming
//!
//! `tasks/sendSubscribe` returns an SSE stream of `{"type":"status"|"artifact"|"task"}`
//! events (artifact chunks carry assistant tokens as they're produced; a final
//! `task` event carries the terminal Task). This wire format matches the
//! client's `parse_sse_event` so a oneai-a2a client interoperates end-to-end.
//! It deviates from the A2A spec's jsonrpc-enveloped `SendTaskStreamingResponse`
//! shape (documented deviation, same posture as §3.2's JWT deviation).
//!
//! ## Deferred (evolution-plan 戒律 #3 — no consumer = no hook)
//!
//! `tasks/resubscribe`, push-notification delivery, and TaskStore disk
//! persistence are intentionally **not** implemented: the oneai-a2a client
//! calls none of them, `SendTaskParams.push_notification` is always `None`,
//! and `tasks/send` completes synchronously so there's no cross-process
//! `tasks/get` window that would outlive the in-memory TaskStore.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use crate::handler::A2AHandler;
use crate::router::A2ARouter;
use crate::runner::{A2ARunner, A2ASseSink, PlaceholderRunner, TaskOutcome};
use crate::task_store::TaskStore;
use crate::types::{AgentCard, Artifact, Part, SendTaskParams, TaskState};

/// A2A server host — serves a OneAI agent's capabilities via the A2A protocol.
pub struct A2AServerHost {
    /// AgentCard describing this agent's capabilities.
    agent_card: AgentCard,
    /// Task store for managing task lifecycle.
    task_store: Arc<TaskStore>,
    /// Router for dispatching methods to handlers.
    router: Arc<A2ARouter>,
    /// The runner that drives a real agent turn. Defaults to
    /// [`PlaceholderRunner`]; the CLI injects an App-backed runner.
    runner: Arc<dyn A2ARunner>,
}

impl A2AServerHost {
    /// Create a new A2A server host with an AgentCard and TaskStore.
    ///
    /// Defaults to a [`PlaceholderRunner`] (no real AgentLoop) — use
    /// [`A2AServerHost::with_runner`] to inject an App-backed runner.
    pub fn new(agent_card: AgentCard, task_store: Arc<TaskStore>) -> Self {
        let runner: Arc<dyn A2ARunner> = Arc::new(PlaceholderRunner::new(agent_card.skills.len()));
        let handler = Arc::new(A2AHandler::new(agent_card.clone(), task_store.clone()));
        let router = Arc::new(A2ARouter::new(handler));

        Self {
            agent_card,
            task_store,
            router,
            runner,
        }
    }

    /// Builder: inject the runner that drives a real agent turn.
    pub fn with_runner(mut self, runner: Arc<dyn A2ARunner>) -> Self {
        self.runner = runner;
        // Keep the handler's runner in sync so tasks/send drives the real loop.
        let handler = Arc::new(
            A2AHandler::new(self.agent_card.clone(), self.task_store.clone())
                .with_runner(self.runner.clone()),
        );
        self.router = Arc::new(A2ARouter::new(handler));
        self
    }

    /// Create a server host from a DomainPack, auto-generating the AgentCard.
    pub fn from_domain_pack(domain: &oneai_domain::DomainPack, url: &str) -> Self {
        let agent_card = crate::card::agent_card_from_domain_pack(domain, url);
        let task_store = Arc::new(TaskStore::new());
        Self::new(agent_card, task_store)
    }

    /// Process a single JSON-RPC message and return the response (non-streaming).
    pub async fn process_message(&self, message: serde_json::Value) -> serde_json::Value {
        self.router.dispatch(message).await
    }

    pub fn agent_card(&self) -> &AgentCard {
        &self.agent_card
    }

    pub fn task_store(&self) -> &Arc<TaskStore> {
        &self.task_store
    }

    /// The runner currently wired into this host.
    pub fn runner(&self) -> &Arc<dyn A2ARunner> {
        &self.runner
    }

    pub fn well_known_card_json(&self) -> crate::error::Result<String> {
        crate::card::well_known_agent_card(&self.agent_card)
    }
}

// ─── Shared-secret Bearer auth (mirrors oneai_scheduler::oneshot) ──────────────

/// The bearer secret env var name.
pub const A2A_SECRET_ENV: &str = "ONEAI_A2A_SECRET";

/// Read the bearer secret from env. `None` if unset/empty → `serve` refuses
/// to start (external triggering disabled until the operator sets a secret).
pub fn secret_from_env() -> Option<String> {
    std::env::var(A2A_SECRET_ENV).ok().filter(|s| !s.is_empty())
}

/// Constant-time comparison so a bearer mismatch doesn't short-circuit and
/// leak length/timing. `true` iff equal. Mirrors `oneshot.rs::ct_eq`.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Verify the `Authorization: Bearer <secret>` header against `expected`.
/// Mirrors `oneshot.rs::verify_bearer`.
fn verify_bearer(headers: &HeaderMap, expected: &str) -> bool {
    let Ok(Some(value)) = headers
        .get(axum::http::header::AUTHORIZATION)
        .map(|v| v.to_str())
        .transpose()
    else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    ct_eq(token.as_bytes(), expected.as_bytes())
}

// ─── axum HTTP server ───────────────────────────────────────────────────────────

/// Shared state for the A2A router.
#[derive(Clone)]
pub struct A2AWebState {
    pub host: Arc<A2AServerHost>,
    /// Shared-secret Bearer token. Empty ⇒ `serve` refuses to start.
    pub secret: String,
}

impl A2AWebState {
    pub fn new(host: Arc<A2AServerHost>, secret: String) -> Self {
        Self { host, secret }
    }
}

/// Configuration for the A2A server.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub addr: SocketAddr,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([0, 0, 0, 0], 8080)),
        }
    }
}

/// Build the axum router. `GET /.well-known/agent-card` (discovery, no auth),
/// `POST /` (JSON-RPC + streaming, Bearer-gated).
pub fn build_router(state: A2AWebState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    Router::new()
        .route("/.well-known/agent-card", get(get_agent_card))
        .route("/", post(post_jsonrpc))
        .layer(cors)
        .with_state(state)
}

/// Start the A2A HTTP server. Blocks until the server stops. Returns an error
/// if no secret is configured (mirrors the cron `/cron/fire` receiver posture —
/// external triggering is disabled until `ONEAI_A2A_SECRET` is set).
pub async fn serve(config: WebConfig, state: A2AWebState) -> crate::error::Result<()> {
    if state.secret.is_empty() {
        return Err(crate::error::A2AError::Protocol(format!(
            "{A2A_SECRET_ENV} unset — A2A server disabled (set a shared secret to enable)"
        )));
    }
    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .map_err(|e| crate::error::A2AError::Network(format!("bind {}: {}", config.addr, e)))?;
    tracing::info!("OneAI A2A server listening on http://{}", config.addr);
    axum::serve(listener, router)
        .await
        .map_err(|e| crate::error::A2AError::Network(format!("axum serve: {e}")))?;
    Ok(())
}

impl A2AServerHost {
    /// Convenience: bind `0.0.0.0:<port>` and serve. Reads the secret from
    /// `ONEAI_A2A_SECRET`; returns an error if unset.
    pub async fn run(self: Arc<Self>, port: u16) -> crate::error::Result<()> {
        let secret = secret_from_env().ok_or_else(|| {
            crate::error::A2AError::Protocol(format!(
                "{A2A_SECRET_ENV} unset — A2A server disabled"
            ))
        })?;
        let config = WebConfig {
            addr: SocketAddr::from(([0, 0, 0, 0], port)),
        };
        serve(config, A2AWebState::new(self, secret)).await
    }
}

// ─── Handlers ───────────────────────────────────────────────────────────────────

/// `GET /.well-known/agent-card` — discovery, no auth.
async fn get_agent_card(State(state): State<A2AWebState>) -> Response {
    match state.host.well_known_card_json() {
        Ok(json) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

type Response = axum::response::Response;

/// `POST /` — JSON-RPC dispatch + streaming branch, Bearer-gated.
async fn post_jsonrpc(
    State(state): State<A2AWebState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Auth: shared-secret Bearer (constant-time).
    if !verify_bearer(&headers, &state.secret) {
        return jsonrpc_error_response(StatusCode::UNAUTHORIZED, None, -32000, "unauthorized");
    }

    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                None,
                -32700,
                &format!("Parse error: {e}"),
            )
        }
    };

    let id = value.get("id").cloned();
    let method = value
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // gap P0 #4 — W3C Trace Context propagation: lift a valid inbound
    // `traceparent` header into `params.metadata.traceparent` so the handler
    // / runner see it through the existing JSON-RPC plumbing (no signature
    // changes downstream). Invalid headers are dropped per spec.
    if let Some(Ok(tp)) = headers
        .get(oneai_trace::TRACEPARENT_HEADER)
        .map(|v| v.to_str())
    {
        if oneai_trace::parse_traceparent(tp).is_some() {
            let params = value
                .as_object_mut()
                .and_then(|o| o.get_mut("params"))
                .and_then(|p| p.as_object_mut());
            if let Some(params) = params {
                let metadata = params
                    .entry("metadata")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(m) = metadata.as_object_mut() {
                    m.insert("traceparent".to_string(), serde_json::json!(tp));
                }
            }
        }
    }

    // Branch: streaming methods get an SSE response; everything else dispatches
    // through the single-Value router.
    if method == "tasks/sendSubscribe" {
        let params = value
            .get("params")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        return streaming_response(state, id, params).await;
    }

    let resp = state.host.process_message(value).await;
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&resp).unwrap_or_default(),
    )
        .into_response()
}

/// JSON-RPC error response helper (also used for auth/parse failures).
fn jsonrpc_error_response(
    status: StatusCode,
    id: Option<serde_json::Value>,
    code: i64,
    message: &str,
) -> Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
        .into_response()
}

/// `tasks/sendSubscribe` — SSE streaming of agent tokens + final task.
async fn streaming_response(
    state: A2AWebState,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
) -> Response {
    let send_params: SendTaskParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return jsonrpc_error_response(
                StatusCode::OK,
                id,
                -32602,
                &format!("Invalid params: {e}"),
            )
        }
    };

    let task_id = send_params.id.clone();
    let session_id = send_params
        .session_id
        .clone()
        .unwrap_or_else(|| task_id.clone());

    // Extract user text (File/Data parts rejected — text-in/text-out surface).
    let message_text = match crate::handler::A2AHandler::extract_text(&send_params.message) {
        Ok(t) => t,
        Err(msg) => {
            return jsonrpc_error_response(
                StatusCode::OK,
                id,
                -32602,
                &format!("Invalid params: {msg}"),
            )
        }
    };

    // Create + transition to Working.
    if state
        .host
        .task_store
        .create_task(&task_id, send_params.message.clone())
        .await
        .is_err()
    {
        return jsonrpc_error_response(StatusCode::OK, id, -32000, "task creation error");
    }
    if state
        .host
        .task_store
        .transition_task(&task_id, TaskState::Working)
        .await
        .is_err()
    {
        return jsonrpc_error_response(StatusCode::OK, id, -32000, "task transition error");
    }

    // Channel of SSE data payloads (raw JSON strings).
    let (tx, rx) = mpsc::channel::<String>(64);
    let sink = Arc::new(A2AChannelSink::new(tx));

    // Emit the initial "working" status.
    sink.push_status(&TaskState::Working);

    // Drive the runner in the background; on completion it emits the final
    // task event and drops the channel sender (closes the SSE stream).
    let host = state.host.clone();
    let runner = state.host.runner().clone();
    // gap P0 #4 — inbound traceparent lifted into params.metadata by the
    // POST handler; threaded to the runner alongside the SSE sink.
    let traceparent = send_params
        .metadata
        .as_ref()
        .and_then(|m| m.get("traceparent"))
        .and_then(|v| v.as_str())
        .filter(|tp| oneai_trace::parse_traceparent(tp).is_some())
        .map(|tp| tp.to_string());
    tokio::spawn(async move {
        let outcome = runner
            .run_task_with_trace(
                &session_id,
                &message_text,
                traceparent.as_deref(),
                Some(sink.clone()),
            )
            .await;
        // Map the outcome to the terminal Task, emit the final `task` event,
        // then let `sink` drop (closing the SSE stream).
        let terminal = match &outcome {
            TaskOutcome::Done { final_answer, .. } => {
                let artifact = Artifact::text("response", final_answer.clone());
                host.task_store
                    .complete_task(&task_id, Some(artifact))
                    .await
            }
            TaskOutcome::Rejected { reason } | TaskOutcome::Error { message: reason } => {
                host.task_store.fail_task(&task_id, reason).await
            }
        };
        if let Ok(task) = terminal {
            sink.emit_final_task(&task).await;
        }
        // `sink` drops here → the channel sender is gone → SSE stream ends.
    });

    // Bridge the receiver → SSE Event stream.
    let stream = ReceiverStream::new(rx)
        .map(|json_str| Ok::<Event, std::convert::Infallible>(Event::default().data(json_str)));
    Sse::new(stream)
        .keep_alive(KeepAlive::new())
        .into_response()
}

/// A channel-backed [`A2ASseSink`] — turns runner pushes into SSE data
/// payloads on an mpsc channel. The final `task` event is emitted by the
/// spawned streaming task once the runner returns (`emit_final_task`).
struct A2AChannelSink {
    tx: mpsc::Sender<String>,
}

impl A2AChannelSink {
    fn new(tx: mpsc::Sender<String>) -> Self {
        Self { tx }
    }

    /// Emit the terminal Task as a final `task` SSE event (tagged so the
    /// client's `parse_sse_event` recognizes it).
    async fn emit_final_task(&self, terminal_task: &crate::types::Task) {
        let mut task_json = match serde_json::to_value(terminal_task) {
            Ok(v) => v,
            Err(_) => return,
        };
        if let Some(obj) = task_json.as_object_mut() {
            obj.insert("type".to_string(), serde_json::json!("task"));
        }
        let payload = serde_json::to_string(&task_json).unwrap_or_default();
        let _ = self.tx.send(payload).await;
    }
}

impl A2ASseSink for A2AChannelSink {
    fn push_chunk(&self, text: &str) {
        // Wrap the assistant fragment as a streaming artifact chunk.
        let artifact = Artifact::streaming_chunk(
            0,
            vec![Part::Text {
                text: text.to_string(),
            }],
            true,
            false,
        );
        let mut v = match serde_json::to_value(&artifact) {
            Ok(v) => v,
            Err(_) => return,
        };
        if let Some(obj) = v.as_object_mut() {
            obj.insert("type".to_string(), serde_json::json!("artifact"));
        }
        let payload = serde_json::to_string(&v).unwrap_or_default();
        // Sync push — non-blocking; drop on backpressure (token stream).
        let _ = self.tx.try_send(payload);
    }

    fn push_status(&self, state: &TaskState) {
        let payload = serde_json::json!({
            "type": "status",
            "status": { "state": state.to_string() }
        });
        let payload = serde_json::to_string(&payload).unwrap_or_default();
        let _ = self.tx.try_send(payload);
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use oneai_domain::DomainPackBuilder;

    #[test]
    fn test_server_host_creation() {
        let card = AgentCard::new("test-agent", "Test", "https://test.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        assert_eq!(host.agent_card().name, "test-agent");
    }

    #[test]
    fn test_server_host_from_domain_pack() {
        let pack = DomainPackBuilder::new("coding")
            .description("A coding agent")
            .system_prompt("You are a coding assistant.")
            .build();

        let host = A2AServerHost::from_domain_pack(&pack, "https://coding.example.com");

        assert_eq!(host.agent_card().name, "coding");
        assert_eq!(host.agent_card().url, "https://coding.example.com");
    }

    #[tokio::test]
    async fn test_server_host_get_card() {
        let card = AgentCard::new("my-agent", "My agent", "https://my.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        let response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "agent/getCard",
                "params": {}
            }))
            .await;

        let result = response.get("result").unwrap();
        assert_eq!(
            result.get("name").and_then(|n| n.as_str()),
            Some("my-agent")
        );
    }

    #[tokio::test]
    async fn test_server_host_send_task() {
        let card = AgentCard::new("task-agent", "Task test", "https://task.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        let response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tasks/send",
                "params": {
                    "id": "task-001",
                    "message": {
                        "role": "user",
                        "parts": [{"type": "text", "text": "Analyze code"}]
                    }
                }
            }))
            .await;

        let result = response.get("result").unwrap();
        assert_eq!(result.get("id").and_then(|v| v.as_str()), Some("task-001"));
        // Should be Completed
        let status = result.get("status").unwrap();
        assert_eq!(
            status.get("state").and_then(|s| s.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn test_server_host_get_task() {
        let card = AgentCard::new("get-agent", "Get test", "https://get.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store.clone());

        // Create a task manually
        store
            .create_task("task-002", Message::user_text("Manual task"))
            .await
            .unwrap();
        store
            .transition_task("task-002", TaskState::Working)
            .await
            .unwrap();

        let response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tasks/get",
                "params": {
                    "id": "task-002"
                }
            }))
            .await;

        let result = response.get("result").unwrap();
        assert_eq!(result.get("id").and_then(|v| v.as_str()), Some("task-002"));
        assert_eq!(
            result
                .get("status")
                .unwrap()
                .get("state")
                .and_then(|s| s.as_str()),
            Some("working")
        );
    }

    #[tokio::test]
    async fn test_server_host_cancel_task() {
        let card = AgentCard::new("cancel-agent", "Cancel test", "https://cancel.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store.clone());

        // Create a task in Working state
        store
            .create_task("task-003", Message::user_text("Cancel me"))
            .await
            .unwrap();
        store
            .transition_task("task-003", TaskState::Working)
            .await
            .unwrap();

        let response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tasks/cancel",
                "params": {
                    "id": "task-003"
                }
            }))
            .await;

        let result = response.get("result").unwrap();
        let status = result.get("status").unwrap();
        assert_eq!(
            status.get("state").and_then(|s| s.as_str()),
            Some("canceled")
        );
    }

    #[tokio::test]
    async fn test_server_host_unknown_method() {
        let card = AgentCard::new("error-agent", "Error test", "https://error.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        let response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "unknown/method",
                "params": {}
            }))
            .await;

        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32601));
    }

    #[tokio::test]
    async fn test_server_host_full_protocol_flow() {
        let card = AgentCard::new(
            "full-agent",
            "Full protocol test",
            "https://full.example.com",
        );
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        // Step 1: Discover agent
        let card_response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "agent/getCard",
                "params": {}
            }))
            .await;

        let card_result = card_response.get("result").unwrap();
        assert_eq!(
            card_result.get("name").and_then(|n| n.as_str()),
            Some("full-agent")
        );

        // Step 2: Send a task
        let send_response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tasks/send",
                "params": {
                    "id": "task-flow",
                    "message": {
                        "role": "user",
                        "parts": [{"type": "text", "text": "Execute this"}]
                    }
                }
            }))
            .await;

        let task_result = send_response.get("result").unwrap();
        assert_eq!(
            task_result.get("id").and_then(|v| v.as_str()),
            Some("task-flow")
        );

        // Step 3: Get the task status
        let get_response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tasks/get",
                "params": {
                    "id": "task-flow"
                }
            }))
            .await;

        let get_result = get_response.get("result").unwrap();
        assert_eq!(
            get_result.get("id").and_then(|v| v.as_str()),
            Some("task-flow")
        );
        assert_eq!(
            get_result
                .get("status")
                .unwrap()
                .get("state")
                .and_then(|s| s.as_str()),
            Some("completed")
        );
    }

    #[test]
    fn test_well_known_card_json() {
        let card = AgentCard::new("json-agent", "JSON test", "https://json.example.com");
        let store = Arc::new(TaskStore::new());
        let host = A2AServerHost::new(card, store);

        let json = host.well_known_card_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.get("name").and_then(|n| n.as_str()),
            Some("json-agent")
        );
    }

    // ─── Auth tests ───────────────────────────────────────────────────────────────

    #[test]
    fn test_ct_eq_constant_time() {
        assert!(ct_eq(b"abc123", b"abc123"));
        assert!(!ct_eq(b"abc123", b"abc124"));
        assert!(!ct_eq(b"abc", b"abc123")); // length mismatch
        assert!(!ct_eq(b"abc123", b""));
    }

    #[test]
    fn test_verify_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer s3cr3t".parse().unwrap());
        assert!(verify_bearer(&headers, "s3cr3t"));
        assert!(!verify_bearer(&headers, "wrong"));
        // Missing header
        let empty = HeaderMap::new();
        assert!(!verify_bearer(&empty, "s3cr3t"));
        // Wrong scheme
        let mut basic = HeaderMap::new();
        basic.insert("authorization", "Basic s3cr3t".parse().unwrap());
        assert!(!verify_bearer(&basic, "s3cr3t"));
    }

    // ─── axum e2e ─────────────────────────────────────────────────────────────────

    async fn spawn_server(secret: &str) -> (SocketAddr, Arc<TaskStore>) {
        let card = AgentCard::new("e2e-agent", "E2E", "https://e2e.example.com");
        let store = Arc::new(TaskStore::new());
        let host = Arc::new(A2AServerHost::new(card, store.clone()));
        let state = A2AWebState::new(host, secret.to_string());
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (addr, store)
    }

    #[tokio::test]
    async fn e2e_get_card_no_auth() {
        let (addr, _store) = spawn_server("secret").await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{addr}/.well-known/agent-card"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body.get("name").and_then(|n| n.as_str()), Some("e2e-agent"));
    }

    #[tokio::test]
    async fn e2e_post_without_bearer_is_401() {
        let (addr, _store) = spawn_server("secret").await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/"))
            .header("content-type", "application/json")
            .body(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "agent/getCard", "params": {}
                }))
                .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn e2e_post_send_task_with_bearer_completes() {
        let (addr, _store) = spawn_server("secret").await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/"))
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json")
            .body(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tasks/send",
                    "params": {
                        "id": "t1",
                        "message": {"role":"user","parts":[{"type":"text","text":"hi"}]}
                    }
                }))
                .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let result = body.get("result").unwrap();
        assert_eq!(result.get("id").and_then(|v| v.as_str()), Some("t1"));
        assert_eq!(
            result
                .get("status")
                .unwrap()
                .get("state")
                .and_then(|s| s.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn e2e_send_subscribe_returns_sse_stream() {
        // A runner that pushes two chunks then completes — exercises the SSE path.
        use crate::runner::{A2ARunner, A2ASseSink, TaskOutcome};
        use async_trait::async_trait;

        struct ChunkRunner;
        #[async_trait]
        impl A2ARunner for ChunkRunner {
            async fn run_task(&self, _: &str, _: &str) -> TaskOutcome {
                TaskOutcome::Done {
                    final_answer: "done".into(),
                    completed: true,
                    iterations: 1,
                }
            }
            async fn run_task_streaming(
                &self,
                _: &str,
                _: &str,
                sink: Arc<dyn A2ASseSink>,
            ) -> TaskOutcome {
                sink.push_chunk("Hel");
                sink.push_chunk("lo");
                TaskOutcome::Done {
                    final_answer: "Hello".into(),
                    completed: true,
                    iterations: 1,
                }
            }
            fn supports_streaming(&self) -> bool {
                true
            }
        }

        let card = AgentCard::new("sse-agent", "SSE", "https://sse.example.com");
        let store = Arc::new(TaskStore::new());
        let host = Arc::new(
            A2AServerHost::new(card, store)
                .with_runner(Arc::new(ChunkRunner) as Arc<dyn A2ARunner>),
        );
        let state = A2AWebState::new(host, "secret".to_string());
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/"))
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json")
            .body(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tasks/sendSubscribe",
                    "params": {
                        "id": "sse1",
                        "message": {"role":"user","parts":[{"type":"text","text":"stream"}]}
                    }
                }))
                .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        // SSE stream should contain artifact chunks (Hel, lo) and a final task event.
        assert!(text.contains("\"type\":\"artifact\""));
        assert!(text.contains("Hel"));
        assert!(text.contains("\"lo\""));
        assert!(text.contains("\"type\":\"task\""));
        assert!(text.contains("completed"));
    }

    #[tokio::test]
    async fn traceparent_header_propagates_to_runner() {
        // gap P0 #4 — a valid inbound `traceparent` HTTP header is lifted
        // into params.metadata and reaches the runner on BOTH the
        // tasks/send and tasks/sendSubscribe paths.
        use crate::runner::{A2ARunner, A2ASseSink, TaskOutcome};
        use async_trait::async_trait;

        const TP: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

        struct CapturingRunner {
            captured: Arc<std::sync::Mutex<Vec<Option<String>>>>,
        }
        #[async_trait]
        impl A2ARunner for CapturingRunner {
            async fn run_task(&self, _: &str, _: &str) -> TaskOutcome {
                TaskOutcome::Done {
                    final_answer: "done".into(),
                    completed: true,
                    iterations: 1,
                }
            }
            async fn run_task_with_trace(
                &self,
                session_id: &str,
                message_text: &str,
                traceparent: Option<&str>,
                sink: Option<Arc<dyn A2ASseSink>>,
            ) -> TaskOutcome {
                self.captured
                    .lock()
                    .unwrap()
                    .push(traceparent.map(|s| s.to_string()));
                let _ = sink; // streaming sink unused — the capture is the point
                self.run_task(session_id, message_text).await
            }
            fn supports_streaming(&self) -> bool {
                true
            }
        }

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let card = AgentCard::new("tp-agent", "traceparent", "https://tp.example.com");
        let store = Arc::new(TaskStore::new());
        let host = Arc::new(A2AServerHost::new(card, store).with_runner(
            Arc::new(CapturingRunner {
                captured: captured.clone(),
            }) as Arc<dyn A2ARunner>,
        ));
        let state = A2AWebState::new(host, "secret".to_string());
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let client = reqwest::Client::new();

        // Non-streaming: tasks/send
        let resp = client
            .post(format!("http://{addr}/"))
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json")
            .header("traceparent", TP)
            .body(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tasks/send",
                    "params": {
                        "id": "tp1",
                        "message": {"role":"user","parts":[{"type":"text","text":"hi"}]}
                    }
                }))
                .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Streaming: tasks/sendSubscribe
        let resp = client
            .post(format!("http://{addr}/"))
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json")
            .header("traceparent", TP)
            .body(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tasks/sendSubscribe",
                    "params": {
                        "id": "tp2",
                        "message": {"role":"user","parts":[{"type":"text","text":"hi"}]}
                    }
                }))
                .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let _ = resp.text().await; // drain the SSE stream to let the task finish

        // Malformed header must be dropped (no propagation).
        let resp = client
            .post(format!("http://{addr}/"))
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json")
            .header("traceparent", "not-a-valid-header")
            .body(
                serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": 3, "method": "tasks/send",
                    "params": {
                        "id": "tp3",
                        "message": {"role":"user","parts":[{"type":"text","text":"hi"}]}
                    }
                }))
                .unwrap(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 3, "runner hit once per request");
        assert_eq!(calls[0].as_deref(), Some(TP)); // tasks/send carried it
        assert_eq!(calls[1].as_deref(), Some(TP)); // sendSubscribe carried it
        assert_eq!(calls[2], None); // malformed header dropped
    }
}
