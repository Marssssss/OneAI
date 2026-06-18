//! MCP Server Host — exposes OneAI tools via MCP JSON-RPC protocol.
//!
//! The McpServerHost serves OneAI's ToolRegistry tools to external MCP clients.
//! It implements the MCP protocol:
//! - `initialize` → handshake with client capabilities and server info
//! - `notifications/initialized` → acknowledge client initialization
//! - `tools/list` → list all registered OneAI tools as MCP tool definitions
//! - `tools/call` → invoke a OneAI tool and return MCP-format content blocks
//! - `resources/list` → list MCP resources (placeholder)
//!
//! Usage:
//! ```ignore
//! let host = McpServerHost::new(tool_registry);
//! host.run_stdio().await?;  // Run as MCP server via stdin/stdout
//! ```

use std::sync::Arc;

use oneai_core::traits::Tool;
use oneai_tool::ToolRegistry;

use crate::handler::McpHandler;
use crate::transport::McpStdioTransport;
use crate::router::McpRouter;

/// MCP server host — serves OneAI tools via the MCP protocol.
///
/// Wraps a `ToolRegistry` and exposes its tools to external MCP clients
/// (Claude Code, Cursor, VS Code extensions, etc.) via JSON-RPC.
///
/// The server implements the MCP lifecycle:
/// 1. Client sends `initialize` → server responds with capabilities
/// 2. Client sends `notifications/initialized` → server is ready
/// 3. Client sends `tools/list` → server lists all registered tools
/// 4. Client sends `tools/call` → server invokes the tool and returns result
pub struct McpServerHost {
    /// Tool registry containing the tools to serve.
    tool_registry: Arc<ToolRegistry>,
    /// Router for dispatching methods to handlers.
    router: Arc<McpRouter>,
    /// Server info (name and version).
    server_info: McpServerInfo,
}

/// Server identification information sent in the `initialize` response.
#[derive(Debug, Clone)]
pub struct McpServerInfo {
    /// Server name (reported to MCP clients).
    pub name: String,
    /// Server version.
    pub version: String,
}

impl Default for McpServerInfo {
    fn default() -> Self {
        Self {
            name: "oneai".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

impl McpServerHost {
    /// Create a new MCP server host from a tool registry.
    pub fn new(tool_registry: Arc<ToolRegistry>) -> Self {
        let server_info = McpServerInfo::default();
        let handler = Arc::new(McpHandler::new(tool_registry.clone(), server_info.clone()));
        let router = Arc::new(McpRouter::new(handler));

        Self {
            tool_registry,
            router,
            server_info,
        }
    }

    /// Create with custom server info.
    pub fn with_server_info(tool_registry: Arc<ToolRegistry>, server_info: McpServerInfo) -> Self {
        let handler = Arc::new(McpHandler::new(tool_registry.clone(), server_info.clone()));
        let router = Arc::new(McpRouter::new(handler));

        Self {
            tool_registry,
            router,
            server_info,
        }
    }

    /// Run the MCP server via Stdio transport (stdin/stdout).
    ///
    /// This is the primary mode for running as an MCP server in CLI tools
    /// like Claude Code, Cursor, etc. The server reads JSON-RPC messages
    /// from stdin and writes responses to stdout, using the MCP
    /// Content-Length framing protocol.
    ///
    /// Usage:
    /// ```ignore
    /// let host = McpServerHost::new(tool_registry);
    /// host.run_stdio().await?;
    /// ```
    pub async fn run_stdio(&self) -> oneai_core::error::Result<()> {
        let transport = McpStdioTransport::new(self.router.clone());
        transport.run().await
    }

    /// Process a single JSON-RPC message and return the response.
    ///
    /// Useful for testing or custom transport implementations.
    pub async fn process_message(&self, message: serde_json::Value) -> serde_json::Value {
        self.router.dispatch(message).await
    }

    /// Get the server info.
    pub fn server_info(&self) -> &McpServerInfo {
        &self.server_info
    }

    /// Get the tool registry.
    pub fn tool_registry(&self) -> &Arc<ToolRegistry> {
        &self.tool_registry
    }

    /// Convert a Tool into an MCP tool definition.
    ///
    /// MCP tool definitions have: name, description, inputSchema.
    /// OneAI tools have: name(), description(), parameters_schema().
    pub fn tool_to_mcp_definition(tool: &Arc<dyn Tool>) -> serde_json::Value {
        serde_json::json!({
            "name": tool.name(),
            "description": tool.description(),
            "inputSchema": tool.parameters_schema(),
        })
    }

    /// Convert a ToolOutput into MCP content blocks.
    ///
    /// MCP content blocks are arrays of objects with "type" and "text" fields.
    pub fn tool_output_to_mcp_content(output: &oneai_core::ToolOutput) -> Vec<serde_json::Value> {
        if output.success {
            vec![serde_json::json!({
                "type": "text",
                "text": output.content,
            })]
        } else {
            let error_text = output.error.as_deref().unwrap_or("Unknown error");
            vec![
                serde_json::json!({
                    "type": "text",
                    "text": format!("Error: {}", error_text),
                }),
                serde_json::json!({
                    "type": "text",
                    "text": output.content,
                }),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_tool::CalculatorTool;

    #[test]
    fn test_tool_to_mcp_definition() {
        let tool: Arc<dyn Tool> = Arc::new(CalculatorTool::new());
        let def = McpServerHost::tool_to_mcp_definition(&tool);

        assert_eq!(def.get("name").and_then(|n| n.as_str()), Some("calculator"));
        assert!(def.get("description").is_some());
        assert!(def.get("inputSchema").is_some());
    }

    #[test]
    fn test_tool_output_to_mcp_content_success() {
        let output = oneai_core::ToolOutput {
            success: true,
            content: "42".to_string(),
            error: None,
        };

        let content = McpServerHost::tool_output_to_mcp_content(&output);
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].get("type").and_then(|t| t.as_str()), Some("text"));
        assert_eq!(content[0].get("text").and_then(|t| t.as_str()), Some("42"));
    }

    #[test]
    fn test_tool_output_to_mcp_content_error() {
        let output = oneai_core::ToolOutput {
            success: false,
            content: String::new(),
            error: Some("Division by zero".to_string()),
        };

        let content = McpServerHost::tool_output_to_mcp_content(&output);
        assert_eq!(content.len(), 2);
        assert_eq!(content[0].get("type").and_then(|t| t.as_str()), Some("text"));
        assert!(content[0].get("text").unwrap().as_str().unwrap().contains("Error"));
    }

    #[tokio::test]
    async fn test_server_host_creation() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();

        let host = McpServerHost::new(registry);
        assert_eq!(host.server_info().name, "oneai");
    }

    #[tokio::test]
    async fn test_server_host_with_custom_info() {
        let registry = Arc::new(ToolRegistry::new());
        let info = McpServerInfo {
            name: "my-agent".to_string(),
            version: "1.0.0".to_string(),
        };

        let host = McpServerHost::with_server_info(registry, info);
        assert_eq!(host.server_info().name, "my-agent");
        assert_eq!(host.server_info().version, "1.0.0");
    }

    #[tokio::test]
    async fn test_full_protocol_flow() {
        // Create a host with a registered tool
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();
        let host = McpServerHost::new(registry);

        // Step 1: Initialize
        let init_response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }
        })).await;

        assert_eq!(init_response.get("id").and_then(|v| v.as_u64()), Some(1));
        let result = init_response.get("result").unwrap();
        assert_eq!(result.get("protocolVersion").and_then(|v| v.as_str()), Some("2024-11-05"));
        assert!(result.get("capabilities").is_some());
        assert_eq!(result.get("serverInfo").unwrap().get("name").and_then(|n| n.as_str()), Some("oneai"));

        // Step 2: Initialized notification (should return no-response sentinel)
        let initialized_response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })).await;
        assert!(initialized_response.get("__mcp_no_response").is_some());

        // Step 3: tools/list
        let tools_response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        })).await;

        let tools_result = tools_response.get("result").unwrap();
        let tools = tools_result.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].get("name").and_then(|n| n.as_str()), Some("calculator"));

        // Step 4: tools/call
        let call_response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "calculator",
                "arguments": { "expression": "2+3" }
            }
        })).await;

        let call_result = call_response.get("result").unwrap();
        let content = call_result.get("content").unwrap().as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].get("type").and_then(|t| t.as_str()), Some("text"));
        assert_eq!(content[0].get("text").and_then(|t| t.as_str()), Some("5"));

        // Step 5: ping
        let ping_response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "ping"
        })).await;
        assert!(ping_response.get("result").is_some());
    }

    #[tokio::test]
    async fn test_protocol_error_unknown_tool() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();
        let host = McpServerHost::new(registry);

        // Call a tool that doesn't exist
        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "nonexistent_tool",
                "arguments": {}
            }
        })).await;

        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32601));
    }

    #[tokio::test]
    async fn test_protocol_error_unknown_method() {
        let registry = Arc::new(ToolRegistry::new());
        let host = McpServerHost::new(registry);

        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "unknown/method"
        })).await;

        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32601));
    }

    #[tokio::test]
    async fn test_tool_call_error_result() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();
        let host = McpServerHost::new(registry);

        // Call calculator with invalid expression — should return isError: true
        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "calculator",
                "arguments": { "expression": "" }
            }
        })).await;

        let result = response.get("result").unwrap();
        assert!(result.get("isError").and_then(|e| e.as_bool()).unwrap_or(false));
    }
}
