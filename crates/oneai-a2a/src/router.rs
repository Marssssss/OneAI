//! A2A Router — dispatches JSON-RPC methods to the A2A handler.
//!
//! The A2ARouter receives parsed JSON-RPC messages and routes them to the
//! appropriate A2AHandler method based on the "method" field.
//!
//! Supported A2A methods:
//! - `agent/getCard` → handler.handle_get_card()
//! - `tasks/send` → handler.handle_send_task()
//! - `tasks/get` → handler.handle_get_task()
//! - `tasks/cancel` → handler.handle_cancel_task()
//! - `tasks/sendSubscribe` → handler.handle_send_subscribe()

use std::sync::Arc;

use crate::handler::A2AHandler;

/// A2A JSON-RPC method router.
///
/// Dispatches incoming JSON-RPC messages to the appropriate handler method
/// based on the "method" field. Follows the same pattern as McpRouter.
pub struct A2ARouter {
    /// Handler for processing A2A protocol messages.
    handler: Arc<A2AHandler>,
}

impl A2ARouter {
    /// Create a new router with the given handler.
    pub fn new(handler: Arc<A2AHandler>) -> Self {
        Self { handler }
    }

    /// Dispatch a JSON-RPC message to the appropriate handler.
    ///
    /// Extracts the "method" field and routes accordingly.
    /// Returns a JSON-RPC response for requests, or an error for unknown methods.
    pub async fn dispatch(&self, message: serde_json::Value) -> serde_json::Value {
        let method = message.get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("");

        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(serde_json::json!({}));

        tracing::debug!("A2A dispatch: method={}, id={:?}", method, id);

        match method {
            "agent/getCard" => {
                self.handler.handle_get_card(id).await
            }
            "tasks/send" => {
                self.handler.handle_send_task(id, &params).await
            }
            "tasks/get" => {
                self.handler.handle_get_task(id, &params).await
            }
            "tasks/cancel" => {
                self.handler.handle_cancel_task(id, &params).await
            }
            "tasks/sendSubscribe" => {
                self.handler.handle_send_subscribe(id, &params).await
            }
            "" => {
                // No method field — invalid request
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32600,
                        "message": "Invalid request — missing method field",
                    }
                })
            }
            _ => {
                // Unknown method
                tracing::warn!("A2A unknown method: {}", method);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method '{}' not found", method),
                    }
                })
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentCard;
    use crate::task_store::TaskStore;

    fn create_router() -> Arc<A2ARouter> {
        let card = AgentCard::new("test-agent", "A test agent", "https://test.example.com");
        let store = Arc::new(TaskStore::new());
        let handler = Arc::new(A2AHandler::new(card, store));
        Arc::new(A2ARouter::new(handler))
    }

    #[tokio::test]
    async fn test_dispatch_get_card() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "agent/getCard",
            "params": {}
        });

        let response = router.dispatch(message).await;
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(1));
        assert!(response.get("result").is_some());
        let result = response.get("result").unwrap();
        assert_eq!(result.get("name").and_then(|n| n.as_str()), Some("test-agent"));
    }

    #[tokio::test]
    async fn test_dispatch_send_task() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/send",
            "params": {
                "id": "task-1",
                "message": {
                    "role": "user",
                    "parts": [{"type": "text", "text": "Hello"}]
                }
            }
        });

        let response = router.dispatch(message).await;
        assert!(response.get("result").is_some());
    }

    #[tokio::test]
    async fn test_dispatch_get_task() {
        let router = create_router();

        // First create a task
        router.dispatch(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks/send",
            "params": {
                "id": "task-2",
                "message": {
                    "role": "user",
                    "parts": [{"type": "text", "text": "Find"}]
                }
            }
        })).await;

        // Then get it
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/get",
            "params": {
                "id": "task-2"
            }
        });

        let response = router.dispatch(message).await;
        assert!(response.get("result").is_some());
    }

    #[tokio::test]
    async fn test_dispatch_cancel_task() {
        let card = AgentCard::new("cancel-agent", "Cancel test", "https://cancel.example.com");
        let store = Arc::new(TaskStore::new());
        let handler = Arc::new(A2AHandler::new(card, store.clone()));
        let router = Arc::new(A2ARouter::new(handler));

        // Create task in Working state
        store.create_task("task-3", crate::types::Message::user_text("Working")).await.unwrap();
        store.transition_task("task-3", crate::types::TaskState::Working).await.unwrap();

        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tasks/cancel",
            "params": {
                "id": "task-3"
            }
        });

        let response = router.dispatch(message).await;
        let result = response.get("result").unwrap();
        let status = result.get("status").unwrap();
        assert_eq!(status.get("state").and_then(|s| s.as_str()), Some("canceled"));
    }

    #[tokio::test]
    async fn test_dispatch_unknown_method() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "unknown/method",
            "params": {}
        });

        let response = router.dispatch(message).await;
        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32601));
    }

    #[tokio::test]
    async fn test_dispatch_invalid_request() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5
        });

        let response = router.dispatch(message).await;
        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32600));
    }

    #[tokio::test]
    async fn test_dispatch_send_subscribe() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tasks/sendSubscribe",
            "params": {
                "id": "task-4",
                "message": {
                    "role": "user",
                    "parts": [{"type": "text", "text": "Stream"}]
                }
            }
        });

        let response = router.dispatch(message).await;
        assert!(response.get("result").is_some());
    }
}
