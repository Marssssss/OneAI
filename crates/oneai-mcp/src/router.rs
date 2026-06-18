//! MCP Router — dispatches JSON-RPC methods to handler functions.
//!
//! The McpRouter receives parsed JSON-RPC messages and routes them to the
//! appropriate McpHandler method based on the "method" field.

use std::sync::Arc;

use crate::handler::McpHandler;

/// MCP JSON-RPC method router.
///
/// Dispatches incoming JSON-RPC messages to the appropriate handler method
/// based on the "method" field in the message.
///
/// Supported methods:
/// - `initialize` → handler.handle_initialize()
/// - `notifications/initialized` → handler.handle_initialized_notification()
/// - `tools/list` → handler.handle_tools_list()
/// - `tools/call` → handler.handle_tools_call()
/// - `resources/list` → handler.handle_resources_list()
/// - `ping` → handler.handle_ping()
pub struct McpRouter {
    /// Handler for processing MCP protocol messages.
    handler: Arc<McpHandler>,
}

impl McpRouter {
    /// Create a new router with the given handler.
    pub fn new(handler: Arc<McpHandler>) -> Self {
        Self { handler }
    }

    /// Dispatch a JSON-RPC message to the appropriate handler.
    ///
    /// Extracts the "method" field and routes accordingly.
    /// Returns a JSON-RPC response (for requests) or None (for notifications).
    /// For unknown methods, returns a JSON-RPC error response.
    pub async fn dispatch(&self, message: serde_json::Value) -> serde_json::Value {
        let method = message.get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("");

        let id = message.get("id").cloned();
        let params = message.get("params");

        tracing::debug!("MCP dispatch: method={}, id={:?}", method, id);

        match method {
            "initialize" => {
                self.handler.handle_initialize(id, params).await
            }
            "notifications/initialized" => {
                // Notifications don't need a response, but for Stdio transport
                // we return an empty marker so the transport knows to skip writing
                self.handler.handle_initialized_notification();
                // Return a "no-response" sentinel — the transport should not write this
                serde_json::json!({"__mcp_no_response": true})
            }
            "tools/list" => {
                self.handler.handle_tools_list(id).await
            }
            "tools/call" => {
                let params_val = params.cloned().unwrap_or(serde_json::json!({}));
                self.handler.handle_tools_call(id, &params_val).await
            }
            "resources/list" => {
                self.handler.handle_resources_list(id).await
            }
            "ping" => {
                self.handler.handle_ping(id)
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
                tracing::warn!("MCP unknown method: {}", method);
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

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_tool::ToolRegistry;
    use oneai_tool::CalculatorTool;
    use std::sync::Arc;

    fn create_router() -> Arc<McpRouter> {
        let registry = Arc::new(ToolRegistry::new());
        let handler = Arc::new(McpHandler::new(
            registry,
            crate::server::McpServerInfo::default(),
        ));
        Arc::new(McpRouter::new(handler))
    }

    // Helper: need async context for registering tools
    async fn create_router_with_tools_async() -> Arc<McpRouter> {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();
        let handler = Arc::new(McpHandler::new(
            registry,
            crate::server::McpServerInfo::default(),
        ));
        Arc::new(McpRouter::new(handler))
    }

    #[tokio::test]
    async fn test_dispatch_initialize() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }
        });

        let response = router.dispatch(message).await;
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(1));
        assert!(response.get("result").is_some());
    }

    #[tokio::test]
    async fn test_dispatch_initialized_notification() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });

        let response = router.dispatch(message).await;
        // Should be no-response sentinel
        assert!(response.get("__mcp_no_response").is_some());
    }

    #[tokio::test]
    async fn test_dispatch_tools_list() {
        let router = create_router_with_tools_async().await;
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });

        let response = router.dispatch(message).await;
        let result = response.get("result").unwrap();
        let tools = result.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 1);
    }

    #[tokio::test]
    async fn test_dispatch_tools_call() {
        let router = create_router_with_tools_async().await;
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "calculator",
                "arguments": { "expression": "2+3" }
            }
        });

        let response = router.dispatch(message).await;
        let result = response.get("result").unwrap();
        let content = result.get("content").unwrap().as_array().unwrap();
        assert_eq!(content[0].get("text").and_then(|t| t.as_str()), Some("5"));
    }

    #[tokio::test]
    async fn test_dispatch_ping() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "ping"
        });

        let response = router.dispatch(message).await;
        assert!(response.get("result").is_some());
    }

    #[tokio::test]
    async fn test_dispatch_unknown_method() {
        let router = create_router();
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "unknown_method"
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
            "id": 6
        });

        let response = router.dispatch(message).await;
        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32600));
    }
}
