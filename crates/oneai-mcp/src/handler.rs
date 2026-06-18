//! MCP Handler — processes MCP protocol messages.
//!
//! The McpHandler implements the MCP server-side protocol:
//! - `initialize` → respond with server capabilities and info
//! - `notifications/initialized` → acknowledge (no response needed)
//! - `tools/list` → list all registered OneAI tools as MCP definitions
//! - `tools/call` → invoke a OneAI tool and return MCP-format result
//! - `resources/list` → list MCP resources (placeholder)

use std::sync::Arc;

use oneai_tool::ToolRegistry;

use crate::server::McpServerInfo;

/// MCP JSON-RPC request handler.
///
/// Processes incoming MCP messages and produces appropriate responses.
/// Each method is handled according to the MCP specification.
pub struct McpHandler {
    /// Tool registry containing the tools to serve.
    tool_registry: Arc<ToolRegistry>,
    /// Server identification info.
    server_info: McpServerInfo,
}

impl McpHandler {
    /// Create a new handler with a tool registry and server info.
    pub fn new(tool_registry: Arc<ToolRegistry>, server_info: McpServerInfo) -> Self {
        Self { tool_registry, server_info }
    }

    /// Handle an `initialize` request.
    ///
    /// Returns server capabilities and info according to the MCP spec.
    /// The response includes:
    /// - protocolVersion: "2024-11-05"
    /// - capabilities: { tools: {} }
    /// - serverInfo: { name, version }
    pub async fn handle_initialize(
        &self,
        id: Option<serde_json::Value>,
        params: Option<&serde_json::Value>,
    ) -> serde_json::Value {
        // Log client capabilities if provided
        if let Some(params) = params {
            if let Some(client_info) = params.get("clientInfo") {
                tracing::info!("MCP client connected: {:?}", client_info);
            }
            if let Some(protocol_version) = params.get("protocolVersion") {
                tracing::info!("MCP client protocol version: {:?}", protocol_version);
            }
        }

        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {},
                    "resources": {},
                },
                "serverInfo": {
                    "name": self.server_info.name,
                    "version": self.server_info.version,
                }
            }
        })
    }

    /// Handle a `notifications/initialized` notification.
    ///
    /// This is a notification (no "id" field), so we don't send a response.
    /// Just log that the client has completed initialization.
    pub fn handle_initialized_notification(&self) -> Option<serde_json::Value> {
        tracing::info!("MCP client completed initialization");
        None // Notifications don't get responses
    }

    /// Handle a `tools/list` request.
    ///
    /// Returns all registered OneAI tools as MCP tool definitions.
    /// Each tool is converted using `McpServerHost::tool_to_mcp_definition()`.
    pub async fn handle_tools_list(
        &self,
        id: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let tool_names = self.tool_registry.list_names().await;
        let mut tool_definitions = Vec::new();

        for tool_name in &tool_names {
            if let Some(tool) = self.tool_registry.get(tool_name).await {
                tool_definitions.push(serde_json::json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "inputSchema": tool.parameters_schema(),
                }));
            }
        }

        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": tool_definitions,
            }
        })
    }

    /// Handle a `tools/call` request.
    ///
    /// Invokes the specified OneAI tool with the given arguments and
    /// returns the result as MCP content blocks.
    ///
    /// The request format:
    /// ```json
    /// {
    ///   "method": "tools/call",
    ///   "params": {
    ///     "name": "calculator",
    ///     "arguments": { "expression": "2+3" }
    ///   }
    /// }
    /// ```
    ///
    /// The response format:
    /// ```json
    /// {
    ///   "result": {
    ///     "content": [
    ///       { "type": "text", "text": "5" }
    ///     ]
    ///   }
    /// }
    /// ```
    pub async fn handle_tools_call(
        &self,
        id: Option<serde_json::Value>,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        let tool_name = params.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("");

        let args = params.get("arguments")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        if tool_name.is_empty() {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "Missing or empty tool name in params",
                }
            });
        }

        // Look up the tool in the registry
        let tool = self.tool_registry.get(tool_name).await;

        if let Some(tool) = tool {
            // Execute the tool
            let result = tool.execute(args).await;

            match result {
                Ok(output) => {
                    // Convert ToolOutput → MCP content blocks
                    let content = crate::server::McpServerHost::tool_output_to_mcp_content(&output);

                    if output.success {
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": content,
                            }
                        })
                    } else {
                        // Tool executed but returned an error result
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": content,
                                "isError": true,
                            }
                        })
                    }
                }
                Err(e) => {
                    // Tool execution failed with an OneAI error
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": format!("Tool execution error: {}", e),
                        }
                    })
                }
            }
        } else {
            // Tool not found
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Tool '{}' not found", tool_name),
                }
            })
        }
    }

    /// Handle a `resources/list` request.
    ///
    /// Returns a placeholder list of resources. In a full implementation,
    /// this would list available data resources that the MCP server exposes.
    pub async fn handle_resources_list(
        &self,
        id: Option<serde_json::Value>,
    ) -> serde_json::Value {
        // Placeholder — OneAI doesn't currently expose resources via MCP
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "resources": [],
            }
        })
    }

    /// Handle a `ping` request.
    ///
    /// Simple heartbeat — returns an empty result.
    pub fn handle_ping(
        &self,
        id: Option<serde_json::Value>,
    ) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {},
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_tool::CalculatorTool;

    #[tokio::test]
    async fn test_handle_initialize() {
        let registry = Arc::new(ToolRegistry::new());
        let handler = McpHandler::new(registry, McpServerInfo::default());

        let response = handler.handle_initialize(Some(serde_json::json!(1)), None).await;
        assert_eq!(response.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"));
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(1));

        let result = response.get("result").unwrap();
        assert_eq!(result.get("protocolVersion").and_then(|v| v.as_str()), Some("2024-11-05"));
        assert!(result.get("capabilities").is_some());
        assert!(result.get("serverInfo").is_some());
    }

    #[tokio::test]
    async fn test_handle_initialized_notification() {
        let registry = Arc::new(ToolRegistry::new());
        let handler = McpHandler::new(registry, McpServerInfo::default());

        // Notifications don't produce responses
        let response = handler.handle_initialized_notification();
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn test_handle_tools_list_empty() {
        let registry = Arc::new(ToolRegistry::new());
        let handler = McpHandler::new(registry, McpServerInfo::default());

        let response = handler.handle_tools_list(Some(serde_json::json!(2))).await;
        let result = response.get("result").unwrap();
        let tools = result.get("tools").unwrap().as_array().unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn test_handle_tools_list_with_tools() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();

        let handler = McpHandler::new(registry, McpServerInfo::default());

        let response = handler.handle_tools_list(Some(serde_json::json!(3))).await;
        let result = response.get("result").unwrap();
        let tools = result.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].get("name").and_then(|n| n.as_str()), Some("calculator"));
    }

    #[tokio::test]
    async fn test_handle_tools_call_success() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();

        let handler = McpHandler::new(registry, McpServerInfo::default());

        let params = serde_json::json!({
            "name": "calculator",
            "arguments": { "expression": "2+3" }
        });

        let response = handler.handle_tools_call(Some(serde_json::json!(4)), &params).await;
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(4));

        let result = response.get("result").unwrap();
        let content = result.get("content").unwrap().as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].get("type").and_then(|t| t.as_str()), Some("text"));
        assert_eq!(content[0].get("text").and_then(|t| t.as_str()), Some("5"));
    }

    #[tokio::test]
    async fn test_handle_tools_call_not_found() {
        let registry = Arc::new(ToolRegistry::new());
        let handler = McpHandler::new(registry, McpServerInfo::default());

        let params = serde_json::json!({
            "name": "nonexistent",
            "arguments": {}
        });

        let response = handler.handle_tools_call(Some(serde_json::json!(5)), &params).await;
        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32601));
        assert!(error.get("message").unwrap().as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_handle_tools_call_missing_name() {
        let registry = Arc::new(ToolRegistry::new());
        let handler = McpHandler::new(registry, McpServerInfo::default());

        let params = serde_json::json!({});

        let response = handler.handle_tools_call(Some(serde_json::json!(6)), &params).await;
        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32602));
    }

    #[tokio::test]
    async fn test_handle_ping() {
        let registry = Arc::new(ToolRegistry::new());
        let handler = McpHandler::new(registry, McpServerInfo::default());

        let response = handler.handle_ping(Some(serde_json::json!(7)));
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(7));
        assert!(response.get("result").is_some());
    }

    #[tokio::test]
    async fn test_handle_resources_list() {
        let registry = Arc::new(ToolRegistry::new());
        let handler = McpHandler::new(registry, McpServerInfo::default());

        let response = handler.handle_resources_list(Some(serde_json::json!(8))).await;
        let result = response.get("result").unwrap();
        let resources = result.get("resources").unwrap().as_array().unwrap();
        assert!(resources.is_empty());
    }
}
