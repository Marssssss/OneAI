//! A2A Protocol transport layer — JSON-RPC 2.0 message framing and HTTP transport.
//!
//! Implements the JSON-RPC 2.0 message format used by A2A for all client→agent
//! communication. Also provides SSE (Server-Sent Events) stream parsing for
//! the `tasks/sendSubscribe` streaming method.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{A2AError, Result};

// ─── JSON-RPC 2.0 Types ────────────────────────────────────────────────────────

/// JSON-RPC 2.0 request — sent by the A2A client to the remote agent.
///
/// All A2A methods use this standard format:
/// ```json
/// {
///   "jsonrpc": "2.0",
///   "id": 1,
///   "method": "tasks/send",
///   "params": { ... }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// JSON-RPC version — always "2.0".
    pub jsonrpc: String,
    /// Request identifier — used to match responses.
    pub id: u64,
    /// The A2A method name (e.g., "tasks/send", "agent/getCard").
    pub method: String,
    /// Method parameters (method-specific).
    pub params: Value,
}

impl JsonRpcRequest {
    /// Create a new JSON-RPC request for the given method and params.
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }

    /// Serialize to a JSON string for HTTP POST transport.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| A2AError::Serialization(e.to_string()))
    }
}

/// JSON-RPC 2.0 response — received from the remote agent.
///
/// Contains either a `result` (success) or an `error` (failure).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// JSON-RPC version — always "2.0".
    pub jsonrpc: String,
    /// Request identifier — matches the request's `id`.
    pub id: u64,
    /// Result payload (present on success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error payload (present on failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Parse a JSON-RPC response from a raw JSON string.
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| A2AError::Serialization(e.to_string()))
    }

    /// Check if this is a success response (has `result`, no `error`).
    pub fn is_success(&self) -> bool {
        self.error.is_none() && self.result.is_some()
    }

    /// Check if this is an error response (has `error`).
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// Extract the result value, or return an A2AError if this is an error response.
    pub fn result_value(&self) -> Result<Value> {
        if let Some(error) = &self.error {
            return Err(A2AError::JsonRpcError {
                code: error.code,
                message: error.message.clone(),
            });
        }
        self.result.clone().ok_or_else(|| A2AError::Protocol("No result in response".to_string()))
    }
}

/// JSON-RPC 2.0 error object — describes a protocol-level error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code (standard JSON-RPC codes or A2A-specific codes).
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ─── A2A Method Constants ──────────────────────────────────────────────────────

/// A2A JSON-RPC method names.
pub const METHOD_TASKS_SEND: &str = "tasks/send";
pub const METHOD_TASKS_SEND_SUBSCRIBE: &str = "tasks/sendSubscribe";
pub const METHOD_TASKS_GET: &str = "tasks/get";
pub const METHOD_TASKS_CANCEL: &str = "tasks/cancel";
pub const METHOD_AGENT_GET_CARD: &str = "agent/getCard";

// ─── SSE Streaming ──────────────────────────────────────────────────────────────

/// SSE event types for A2A streaming responses.
///
/// When using `tasks/sendSubscribe`, the remote agent sends SSE events
/// with the following event types:
/// - `task`: A complete Task object update
/// - `status`: A TaskStatus update
/// - `artifact`: An Artifact chunk (for streaming outputs)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum TaskStreamEvent {
    /// Complete Task object update.
    #[serde(rename = "task")]
    Task { task: Value },
    /// TaskStatus update (state change).
    #[serde(rename = "status")]
    Status { status: Value },
    /// Artifact chunk (streaming output).
    #[serde(rename = "artifact")]
    Artifact { artifact: Value },
}

/// Parse SSE data lines into TaskStreamEvents.
///
/// SSE format: `data: <json>\n\n`
/// The JSON payload is the event content.
pub fn parse_sse_event(data: &str) -> Result<TaskStreamEvent> {
    // SSE data lines start with "data: " prefix
    let json_str = data.trim_start_matches("data: ").trim();

    // Try to parse as a generic JSON value first
    let value: Value = serde_json::from_str(json_str)
        .map_err(|e| A2AError::Serialization(format!("SSE data parse error: {}", e)))?;

    // Determine event type from the JSON structure
    // A2A SSE events carry a "type" field in the JSON data
    if let Some(event_type) = value.get("type").and_then(|t| t.as_str()) {
        match event_type {
            "task" => Ok(TaskStreamEvent::Task { task: value }),
            "status" => Ok(TaskStreamEvent::Status { status: value }),
            "artifact" => Ok(TaskStreamEvent::Artifact { artifact: value }),
            _ => Err(A2AError::Protocol(format!("Unknown SSE event type: {}", event_type))),
        }
    } else {
        // If no "type" field, assume it's a full Task update
        Ok(TaskStreamEvent::Task { task: value })
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_json_rpc_request_creation() {
        let request = JsonRpcRequest::new(1, METHOD_TASKS_SEND, json!({"id": "task-1", "message": {"role": "user", "parts": []}}));
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.id, 1);
        assert_eq!(request.method, "tasks/send");
    }

    #[test]
    fn test_json_rpc_request_serialization() {
        let request = JsonRpcRequest::new(42, METHOD_AGENT_GET_CARD, json!({}));
        let json = request.to_json().unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"method\":\"agent/getCard\""));
    }

    #[test]
    fn test_json_rpc_response_success() {
        let json = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"id": "task-1", "status": {"state": "working"}}
        }"#;
        let response = JsonRpcResponse::from_json(json).unwrap();
        assert!(response.is_success());
        assert!(!response.is_error());
        let result = response.result_value().unwrap();
        assert_eq!(result.get("id").and_then(|v| v.as_str()), Some("task-1"));
    }

    #[test]
    fn test_json_rpc_response_error() {
        let json = r#"{
            "jsonrpc": "2.0",
            "id": 2,
            "error": {"code": -32600, "message": "Invalid Request"}
        }"#;
        let response = JsonRpcResponse::from_json(json).unwrap();
        assert!(response.is_error());
        assert!(!response.is_success());

        let err = response.result_value().unwrap_err();
        match err {
            A2AError::JsonRpcError { code, message } => {
                assert_eq!(code, -32600);
                assert_eq!(message, "Invalid Request");
            }
            _ => panic!("Expected JsonRpcError"),
        }
    }

    #[test]
    fn test_json_rpc_error_object() {
        let error = JsonRpcError {
            code: -32601,
            message: "Method not found".to_string(),
            data: Some(json!({"method": "unknown/method"})),
        };
        let json = serde_json::to_string(&error).unwrap();
        let deserialized: JsonRpcError = serde_json::from_str(&json).unwrap();
        assert_eq!(error.code, deserialized.code);
        assert_eq!(error.message, deserialized.message);
    }

    #[test]
    fn test_method_constants() {
        assert_eq!(METHOD_TASKS_SEND, "tasks/send");
        assert_eq!(METHOD_TASKS_SEND_SUBSCRIBE, "tasks/sendSubscribe");
        assert_eq!(METHOD_TASKS_GET, "tasks/get");
        assert_eq!(METHOD_TASKS_CANCEL, "tasks/cancel");
        assert_eq!(METHOD_AGENT_GET_CARD, "agent/getCard");
    }

    #[test]
    fn test_sse_event_task() {
        let data = r#"data: {"type": "task", "id": "t-1", "status": {"state": "working"}}"#;
        let event = parse_sse_event(data).unwrap();
        match event {
            TaskStreamEvent::Task { task } => {
                assert_eq!(task.get("id").and_then(|v| v.as_str()), Some("t-1"));
            }
            _ => panic!("Expected Task event"),
        }
    }

    #[test]
    fn test_sse_event_status() {
        let data = r#"data: {"type": "status", "state": "completed"}"#;
        let event = parse_sse_event(data).unwrap();
        match event {
            TaskStreamEvent::Status { status } => {
                assert_eq!(status.get("state").and_then(|v| v.as_str()), Some("completed"));
            }
            _ => panic!("Expected Status event"),
        }
    }

    #[test]
    fn test_sse_event_artifact() {
        let data = r#"data: {"type": "artifact", "name": "result"}"#;
        let event = parse_sse_event(data).unwrap();
        match event {
            TaskStreamEvent::Artifact { artifact } => {
                assert_eq!(artifact.get("name").and_then(|v| v.as_str()), Some("result"));
            }
            _ => panic!("Expected Artifact event"),
        }
    }

    #[test]
    fn test_sse_event_no_type_defaults_to_task() {
        let data = r#"data: {"id": "t-2", "status": {"state": "input-required"}}"#;
        let event = parse_sse_event(data).unwrap();
        match event {
            TaskStreamEvent::Task { task } => {
                assert_eq!(task.get("id").and_then(|v| v.as_str()), Some("t-2"));
            }
            _ => panic!("Expected Task event (default)"),
        }
    }
}
