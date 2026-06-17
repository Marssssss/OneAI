//! A2A Protocol client — sends JSON-RPC requests to remote A2A agents.
//!
//! The `A2AClient` is the client-side interface for interacting with remote
//! A2A agents. It supports:
//!
//! - **Discovery**: Fetch an Agent's AgentCard to learn its capabilities
//! - **Task management**: Create, get, and cancel tasks on the remote agent
//! - **Streaming**: Subscribe to task updates via SSE for long-running tasks
//!
//! The client uses `reqwest` for HTTP transport and `eventsource-stream`
//! for SSE response parsing. All communication follows the A2A JSON-RPC 2.0
//! protocol format.
//!
//! ## Usage
//!
//! ```ignore
//! // Create a client targeting a remote agent
//! let mut client = A2AClient::new("https://remote-agent.example.com");
//!
//! // Discover the agent's capabilities
//! let card = client.discover().await?;
//! println!("Agent: {} — skills: {}", card.name, card.skills.len());
//!
//! // Send a task to the remote agent
//! let task = client.send_task(
//!     "task-123",
//!     Message::user_text("Analyze this code"),
//!     None,
//! ).await?;
//!
//! // Poll for task completion
//! let completed = client.get_task("task-123", None).await?;
//! ```

use std::collections::HashMap;
use std::pin::Pin;

use futures::Stream;
use futures::StreamExt;
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::{A2AError, Result};
use crate::types::*;
use crate::transport::*;

// ─── A2AClient ──────────────────────────────────────────────────────────────────

/// Client for interacting with a remote A2A agent.
///
/// The client communicates with the agent via HTTP POST requests carrying
/// JSON-RPC 2.0 messages. It supports:
///
/// 1. **Discovery** — fetch the agent's AgentCard via `agent/getCard`
/// 2. **Task operations** — create, query, and cancel tasks
/// 3. **Streaming** — subscribe to task updates via SSE
///
/// The client maintains a mutable `next_id` counter for JSON-RPC request IDs
/// and an optional cached `AgentCard` for efficient subsequent operations.
pub struct A2AClient {
    /// HTTP client for sending JSON-RPC requests.
    http_client: reqwest::Client,
    /// The remote agent's endpoint URL.
    agent_url: String,
    /// Cached AgentCard (populated after `discover()`).
    agent_card: Option<AgentCard>,
    /// Next JSON-RPC request ID (incremented for each request).
    next_id: u64,
    /// Custom headers to include in every request (e.g., authentication).
    headers: HashMap<String, String>,
    /// Request timeout in seconds.
    timeout_secs: u64,
}

impl A2AClient {
    /// Create a new A2A client targeting the given agent URL.
    pub fn new(agent_url: impl Into<String>) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            agent_url: agent_url.into(),
            agent_card: None,
            next_id: 1,
            headers: HashMap::new(),
            timeout_secs: 30,
        }
    }

    /// Create a client with custom HTTP headers (e.g., for authentication).
    pub fn with_headers(agent_url: impl Into<String>, headers: HashMap<String, String>) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            agent_url: agent_url.into(),
            agent_card: None,
            next_id: 1,
            headers,
            timeout_secs: 30,
        }
    }

    /// Set a custom request timeout (in seconds).
    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    /// Get the cached AgentCard (if `discover()` has been called).
    pub fn agent_card(&self) -> Option<&AgentCard> {
        self.agent_card.as_ref()
    }

    /// Get the agent URL.
    pub fn agent_url(&self) -> &str {
        &self.agent_url
    }

    // ─── Discovery ──────────────────────────────────────────────────────────────

    /// Discover the remote agent's capabilities by fetching its AgentCard.
    ///
    /// Sends an `agent/getCard` JSON-RPC request to the agent's endpoint.
    /// The AgentCard is cached for subsequent reference.
    ///
    /// Returns the AgentCard describing the agent's name, skills, capabilities,
    /// and authentication requirements.
    pub async fn discover(&mut self) -> Result<AgentCard> {
        let request = JsonRpcRequest::new(self.next_id, METHOD_AGENT_GET_CARD, serde_json::json!({}));
        self.next_id += 1;

        let response = self.send_jsonrpc(&request).await?;
        let result = response.result_value()?;

        // The result may be the AgentCard directly or wrapped in a "card" field
        let card_json = if result.get("name").is_some() {
            result
        } else if let Some(card) = result.get("card") {
            card.clone()
        } else {
            return Err(A2AError::Discovery("AgentCard not found in response".to_string()));
        };

        let card: AgentCard = serde_json::from_value(card_json)
            .map_err(|e| A2AError::Serialization(format!("AgentCard parse error: {}", e)))?;

        tracing::info!("A2A discovered agent '{}' at '{}' — {} skills, streaming={}",
            card.name, card.url, card.skills.len(), card.capabilities.streaming);

        self.agent_card = Some(card.clone());
        Ok(card)
    }

    // ─── Task Operations ────────────────────────────────────────────────────────

    /// Send a task to the remote agent.
    ///
    /// Creates a new task or continues an existing task with a message from
    /// the client. Returns the Task object with its current state.
    ///
    /// If the task ID is new, the agent creates a fresh task. If the ID
    /// corresponds to an existing task (e.g., from a previous `input-required`
    /// state), the agent continues processing.
    pub async fn send_task(
        &mut self,
        task_id: impl Into<String>,
        message: Message,
        session_id: Option<String>,
    ) -> Result<Task> {
        let params = SendTaskParams {
            id: task_id.into(),
            message,
            session_id,
            history_length: None,
            push_notification: None,
            metadata: None,
        };

        let request = JsonRpcRequest::new(
            self.next_id,
            METHOD_TASKS_SEND,
            serde_json::to_value(&params)
                .map_err(|e| A2AError::Serialization(e.to_string()))?,
        );
        self.next_id += 1;

        let response = self.send_jsonrpc(&request).await?;
        let result = response.result_value()?;

        serde_json::from_value(result)
            .map_err(|e| A2AError::Serialization(format!("Task parse error: {}", e)))
    }

    /// Get the current state of an existing task.
    ///
    /// Returns the Task object with its current status, history, and artifacts.
    pub async fn get_task(&self, task_id: &str, history_length: Option<usize>) -> Result<Task> {
        let params = GetTaskParams {
            id: task_id.to_string(),
            history_length,
        };

        let request = JsonRpcRequest::new(
            self.next_id, // Note: immutable borrow, can't increment — acceptable for read ops
            METHOD_TASKS_GET,
            serde_json::to_value(&params)
                .map_err(|e| A2AError::Serialization(e.to_string()))?,
        );

        let response = self.send_jsonrpc(&request).await?;
        let result = response.result_value()?;

        serde_json::from_value(result)
            .map_err(|e| A2AError::Serialization(format!("Task parse error: {}", e)))
    }

    /// Cancel a running task.
    ///
    /// Requests the remote agent to cancel the task. Returns the Task
    /// with its updated state (should be `Canceled` if successful).
    pub async fn cancel_task(&mut self, task_id: &str) -> Result<Task> {
        let params = CancelTaskParams {
            id: task_id.to_string(),
        };

        let request = JsonRpcRequest::new(
            self.next_id,
            METHOD_TASKS_CANCEL,
            serde_json::to_value(&params)
                .map_err(|e| A2AError::Serialization(e.to_string()))?,
        );
        self.next_id += 1;

        let response = self.send_jsonrpc(&request).await?;
        let result = response.result_value()?;

        serde_json::from_value(result)
            .map_err(|e| A2AError::Serialization(format!("Task parse error: {}", e)))
    }

    // ─── Streaming ──────────────────────────────────────────────────────────────

    /// Send a task with SSE streaming subscription.
    ///
    /// Like `send_task()`, but the remote agent sends incremental updates
    /// via Server-Sent Events (SSE) instead of returning a single response.
    /// This is useful for long-running tasks where you want to receive
    /// intermediate status updates and artifacts as they're produced.
    ///
    /// Returns a `TaskStream` that yields `TaskStreamEvent` items as they arrive.
    pub async fn send_subscribe(
        &mut self,
        task_id: impl Into<String>,
        message: Message,
        session_id: Option<String>,
    ) -> Result<TaskStream> {
        let params = SendTaskParams {
            id: task_id.into(),
            message,
            session_id,
            history_length: None,
            push_notification: None,
            metadata: None,
        };

        let request = JsonRpcRequest::new(
            self.next_id,
            METHOD_TASKS_SEND_SUBSCRIBE,
            serde_json::to_value(&params)
                .map_err(|e| A2AError::Serialization(e.to_string()))?,
        );
        self.next_id += 1;

        // SSE streaming requires a different HTTP approach — POST and read SSE response
        let request_json = request.to_json()?;

        let http_request = self.http_client
            .post(&self.agent_url)
            .header("Content-Type", "application/json")
            .body(request_json)
            .timeout(std::time::Duration::from_secs(self.timeout_secs * 10)); // Longer timeout for streaming

        let response = http_request.send().await
            .map_err(|e| A2AError::Network(format!("SSE HTTP POST error: {}", e)))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(A2AError::Network(format!("SSE HTTP POST returned status {}: {}", status.as_u16(), &body[..body.len().min(200)])));
        }

        // Create a channel for streaming events
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // Spawn a background task to parse SSE events from the response body
        tokio::spawn(async move {
            let mut buffer = String::new();
            let mut stream = response.bytes_stream();

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

                        // Process complete SSE events (delimited by double newline)
                        while let Some(event_end) = buffer.find("\n\n") {
                            let event_block = buffer[..event_end].to_string();
                            buffer = buffer[event_end + 2..].to_string();

                            // Parse each data line in the event block
                            for line in event_block.lines() {
                                if line.starts_with("data: ") {
                                    if let Ok(event) = parse_sse_event(line) {
                                        if tx.send(event).await.is_err() {
                                            return; // Channel closed
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("SSE stream error: {}", e);
                        return;
                    }
                }
            }
        });

        Ok(TaskStream { inner: ReceiverStream::new(rx) })
    }

    // ─── HTTP Transport ─────────────────────────────────────────────────────────

    /// Send a JSON-RPC request via HTTP POST and receive the response.
    async fn send_jsonrpc(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let request_json = request.to_json()?;
        let request_value: Value = serde_json::from_str(&request_json)
            .map_err(|e| A2AError::Serialization(e.to_string()))?;

        let mut http_request = self.http_client
            .post(&self.agent_url)
            .json(&request_value)
            .timeout(std::time::Duration::from_secs(self.timeout_secs));

        for (key, value) in &self.headers {
            if let Ok(header_name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
                if let Ok(header_value) = reqwest::header::HeaderValue::from_str(value) {
                    http_request = http_request.header(header_name, header_value);
                }
            }
        }

        tracing::debug!("A2A JSON-RPC request: method={}, id={}", request.method, request.id);

        let response = http_request.send().await
            .map_err(|e| A2AError::Network(format!("HTTP POST error to {}: {}", self.agent_url, e)))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(A2AError::Network(format!("HTTP POST returned status {} from {}: {}", status.as_u16(), self.agent_url, &body[..body.len().min(200)])));
        }

        let body = response.text().await
            .map_err(|e| A2AError::Network(format!("HTTP response read error: {}", e)))?;

        // The response might be plain JSON or SSE format
        // Try plain JSON first (most common for non-streaming requests)
        if let Ok(json) = serde_json::from_str::<Value>(&body) {
            let rpc_response: JsonRpcResponse = serde_json::from_value(json)
                .map_err(|e| A2AError::Serialization(format!("JSON-RPC response parse error: {}", e)))?;
            return Ok(rpc_response);
        }

        // Try SSE format — find the JSON data in SSE events
        for line in body.lines() {
            if line.starts_with("data: ") {
                let data = line.trim_start_matches("data: ").trim();
                if let Ok(json) = serde_json::from_str::<Value>(data) {
                    let rpc_response: JsonRpcResponse = serde_json::from_value(json)
                        .map_err(|e| A2AError::Serialization(format!("SSE JSON-RPC parse error: {}", e)))?;
                    return Ok(rpc_response);
                }
            }
        }

        Err(A2AError::Protocol(format!("Could not parse response from {}: {}", self.agent_url, &body[..body.len().min(200)])))
    }
}

// ─── TaskStream ──────────────────────────────────────────────────────────────────

/// A stream of SSE events from an A2A agent for a subscribed task.
///
/// Yields `TaskStreamEvent` items as they arrive from the remote agent:
/// - `Task`: Complete Task object update
/// - `Status`: TaskStatus state change
/// - `Artifact`: Artifact chunk (streaming output)
pub struct TaskStream {
    inner: ReceiverStream<TaskStreamEvent>,
}

impl Stream for TaskStream {
    type Item = TaskStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = A2AClient::new("https://agent.example.com");
        assert_eq!(client.agent_url(), "https://agent.example.com");
        assert!(client.agent_card().is_none());
    }

    #[test]
    fn test_client_with_headers() {
        let headers = HashMap::from([
            ("Authorization".to_string(), "Bearer token123".to_string()),
        ]);
        let client = A2AClient::with_headers("https://agent.example.com", headers);
        assert_eq!(client.headers.get("Authorization").unwrap(), "Bearer token123");
    }

    #[test]
    fn test_client_with_timeout() {
        let client = A2AClient::new("https://agent.example.com").with_timeout(60);
        assert_eq!(client.timeout_secs, 60);
    }

    #[test]
    fn test_send_task_params_building() {
        let params = SendTaskParams {
            id: "task-001".to_string(),
            message: Message::user_text("Hello agent"),
            session_id: Some("session-abc".to_string()),
            history_length: Some(5),
            push_notification: None,
            metadata: None,
        };

        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value.get("id").and_then(|v| v.as_str()), Some("task-001"));
        assert_eq!(value.get("sessionId").and_then(|v| v.as_str()), Some("session-abc"));
        assert_eq!(value.get("historyLength").and_then(|v| v.as_u64()), Some(5));
    }

    #[test]
    fn test_get_task_params_building() {
        let params = GetTaskParams {
            id: "task-002".to_string(),
            history_length: Some(10),
        };
        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value.get("id").and_then(|v| v.as_str()), Some("task-002"));
        assert_eq!(value.get("historyLength").and_then(|v| v.as_u64()), Some(10));
    }

    #[test]
    fn test_cancel_task_params_building() {
        let params = CancelTaskParams {
            id: "task-003".to_string(),
        };
        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value.get("id").and_then(|v| v.as_str()), Some("task-003"));
    }
}
