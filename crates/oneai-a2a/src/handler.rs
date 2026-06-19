//! A2A Handler — processes A2A JSON-RPC protocol messages on the server side.
//!
//! The A2AHandler implements the server-side A2A protocol:
//! - `agent/getCard` → return cached AgentCard
//! - `tasks/send` → create Task, process via ToolRegistry, return result
//! - `tasks/get` → retrieve Task from TaskStore
//! - `tasks/cancel` → transition Task to Canceled state
//! - `tasks/sendSubscribe` → SSE streaming (placeholder for future)

use std::sync::Arc;

use crate::types::{AgentCard, Task, TaskState, Message, SendTaskParams, GetTaskParams, CancelTaskParams};
use crate::task_store::TaskStore;
use crate::transport::{JsonRpcRequest, METHOD_AGENT_GET_CARD, METHOD_TASKS_SEND, METHOD_TASKS_GET, METHOD_TASKS_CANCEL};

/// A2A JSON-RPC request handler.
///
/// Processes incoming A2A protocol messages and produces appropriate
/// JSON-RPC responses. Each method follows the A2A specification.
pub struct A2AHandler {
    /// Cached AgentCard for this agent.
    agent_card: AgentCard,
    /// Task store for managing task lifecycle.
    task_store: Arc<TaskStore>,
}

impl A2AHandler {
    /// Create a new handler with an AgentCard and TaskStore.
    pub fn new(agent_card: AgentCard, task_store: Arc<TaskStore>) -> Self {
        Self { agent_card, task_store }
    }

    /// Handle `agent/getCard` request — return the cached AgentCard.
    pub async fn handle_get_card(&self, id: Option<serde_json::Value>) -> serde_json::Value {
        let card_json = serde_json::to_value(&self.agent_card)
            .unwrap_or_else(|e| serde_json::json!({"error": format!("Serialization error: {}", e)}));

        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": card_json,
        })
    }

    /// Handle `tasks/send` request — create a Task and process it.
    ///
    /// Creates a Task in Submitted state, transitions to Working,
    /// then attempts to process the message. For P4-1, the processing
    /// is simplified: we transition to Working then immediately complete
    /// with a placeholder response. Full AgentLoop integration comes later.
    pub async fn handle_send_task(&self, id: Option<serde_json::Value>, params: &serde_json::Value) -> serde_json::Value {
        // Parse the SendTaskParams
        let send_params: SendTaskParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": format!("Invalid params for tasks/send: {}", e),
                    }
                });
            }
        };

        // Create the task
        let task = match self.task_store.create_task(&send_params.id, send_params.message.clone()).await {
            Ok(t) => t,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!("Task creation error: {}", e),
                    }
                });
            }
        };

        // Transition to Working state
        let task = match self.task_store.transition_task(&send_params.id, TaskState::Working).await {
            Ok(t) => t,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!("Task transition error: {}", e),
                    }
                });
            }
        };

        // For P4-1: simplified processing — complete with placeholder response
        // Full processing with ToolRegistry/AgentLoop integration comes later
        let agent_response = Message::agent_text(format!("Task '{}' received and processed. Agent capabilities: {} skills available.", send_params.id, self.agent_card.skills.len()));

        let artifact = crate::types::Artifact::text("response", &agent_response.parts.iter()
            .filter_map(|p| match p {
                crate::types::Part::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"));

        // Complete the task
        match self.task_store.complete_task(&send_params.id, Some(artifact)).await {
            Ok(completed_task) => {
                let task_json = serde_json::to_value(&completed_task)
                    .unwrap_or_else(|e| serde_json::json!({"error": format!("Serialization error: {}", e)}));

                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": task_json,
                })
            }
            Err(e) => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!("Task completion error: {}", e),
                    }
                })
            }
        }
    }

    /// Handle `tasks/get` request — retrieve an existing task.
    pub async fn handle_get_task(&self, id: Option<serde_json::Value>, params: &serde_json::Value) -> serde_json::Value {
        let get_params: GetTaskParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": format!("Invalid params for tasks/get: {}", e),
                    }
                });
            }
        };

        match self.task_store.get_task(&get_params.id).await {
            Ok(task) => {
                let task_json = serde_json::to_value(&task)
                    .unwrap_or_else(|e| serde_json::json!({"error": format!("Serialization error: {}", e)}));

                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": task_json,
                })
            }
            Err(e) => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32001,
                        "message": format!("Task not found: {}", e),
                    }
                })
            }
        }
    }

    /// Handle `tasks/cancel` request — cancel an existing task.
    pub async fn handle_cancel_task(&self, id: Option<serde_json::Value>, params: &serde_json::Value) -> serde_json::Value {
        let cancel_params: CancelTaskParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": format!("Invalid params for tasks/cancel: {}", e),
                    }
                });
            }
        };

        // Task must be in Submitted or Working state to cancel
        match self.task_store.cancel_task(&cancel_params.id).await {
            Ok(task) => {
                let task_json = serde_json::to_value(&task)
                    .unwrap_or_else(|e| serde_json::json!({"error": format!("Serialization error: {}", e)}));

                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": task_json,
                })
            }
            Err(e) => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32001,
                        "message": format!("Cancel error: {}", e),
                    }
                })
            }
        }
    }

    /// Handle `tasks/sendSubscribe` request — SSE streaming (placeholder).
    ///
    /// For P4-1, this returns the same result as `tasks/send` but wrapped
    /// in a stream-compatible format. Full SSE streaming will be implemented
    /// when the axum HTTP server is added.
    pub async fn handle_send_subscribe(&self, id: Option<serde_json::Value>, params: &serde_json::Value) -> serde_json::Value {
        // For now, delegate to handle_send_task — SSE streaming is a future enhancement
        self.handle_send_task(id, params).await
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentCapabilities;

    fn create_handler() -> A2AHandler {
        let card = AgentCard::new("test-agent", "A test agent", "https://test.example.com");
        let store = Arc::new(TaskStore::new());
        A2AHandler::new(card, store)
    }

    #[tokio::test]
    async fn test_handle_get_card() {
        let handler = create_handler();
        let response = handler.handle_get_card(Some(serde_json::json!(1))).await;

        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(1));
        let result = response.get("result").unwrap();
        assert_eq!(result.get("name").and_then(|n| n.as_str()), Some("test-agent"));
    }

    #[tokio::test]
    async fn test_handle_send_task() {
        let handler = create_handler();
        let params = serde_json::json!({
            "id": "task-test-1",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Hello agent"}]
            }
        });

        let response = handler.handle_send_task(Some(serde_json::json!(2)), &params).await;
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(2));

        let result = response.get("result").unwrap();
        assert_eq!(result.get("id").and_then(|v| v.as_str()), Some("task-test-1"));
        // Task should be Completed
        let status = result.get("status").unwrap();
        assert_eq!(status.get("state").and_then(|s| s.as_str()), Some("completed"));
    }

    #[tokio::test]
    async fn test_handle_get_task() {
        let handler = create_handler();
        // First create a task
        let send_params = serde_json::json!({
            "id": "task-test-2",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Find info"}]
            }
        });
        handler.handle_send_task(Some(serde_json::json!(1)), &send_params).await;

        // Then get the task
        let get_params = serde_json::json!({
            "id": "task-test-2"
        });
        let response = handler.handle_get_task(Some(serde_json::json!(2)), &get_params).await;
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(2));

        let result = response.get("result").unwrap();
        assert_eq!(result.get("id").and_then(|v| v.as_str()), Some("task-test-2"));
    }

    #[tokio::test]
    async fn test_handle_get_nonexistent_task() {
        let handler = create_handler();
        let get_params = serde_json::json!({
            "id": "nonexistent"
        });
        let response = handler.handle_get_task(Some(serde_json::json!(3)), &get_params).await;

        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32001));
    }

    #[tokio::test]
    async fn test_handle_cancel_task() {
        let handler = create_handler();
        // Create a task
        let send_params = serde_json::json!({
            "id": "task-test-3",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Cancel this"}]
            }
        });
        handler.handle_send_task(Some(serde_json::json!(1)), &send_params).await;

        // Cancel the task (note: already completed, so this will fail —
        // let's create a task that stays in Working state instead)
        let store = Arc::new(TaskStore::new());
        let card = AgentCard::new("cancel-agent", "Cancel test", "https://cancel.example.com");
        let handler = A2AHandler::new(card, store.clone());

        // Manually create a task in Working state
        store.create_task("task-cancel", Message::user_text("Working task")).await.unwrap();
        store.transition_task("task-cancel", TaskState::Working).await.unwrap();

        let cancel_params = serde_json::json!({
            "id": "task-cancel"
        });
        let response = handler.handle_cancel_task(Some(serde_json::json!(2)), &cancel_params).await;

        let result = response.get("result").unwrap();
        let status = result.get("status").unwrap();
        assert_eq!(status.get("state").and_then(|s| s.as_str()), Some("canceled"));
    }

    #[tokio::test]
    async fn test_handle_send_task_invalid_params() {
        let handler = create_handler();
        let params = serde_json::json!({"invalid": true});

        let response = handler.handle_send_task(Some(serde_json::json!(4)), &params).await;
        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32602));
    }

    #[tokio::test]
    async fn test_handle_send_subscribe_delegates_to_send() {
        let handler = create_handler();
        let params = serde_json::json!({
            "id": "task-subscribe-1",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Stream test"}]
            }
        });

        let response = handler.handle_send_subscribe(Some(serde_json::json!(5)), &params).await;
        // Should produce the same result as handle_send_task
        assert!(response.get("result").is_some());
    }
}
