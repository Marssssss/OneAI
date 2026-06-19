//! MCP Client — standalone wrapper for connecting to external MCP servers.
//!
//! The McpClient provides a simple, standalone API for connecting to a single
//! MCP server, discovering its tools, and invoking them. It wraps the existing
//! `McpServerManager` infrastructure from `oneai-tool/src/mcp_real.rs`.
//!
//! ## Usage
//! ```ignore
//! // Connect to an MCP server via stdio transport
//! let client = McpClient::stdio("npx", &["-y", "@anthropic/mcp-server-filesystem"]);
//! client.connect().await?;
//!
//! // Discover available tools
//! let tools = client.discover_tools().await?;
//! for tool in &tools {
//!     println!("  • {} — {}", tool.name, tool.description);
//! }
//!
//! // Call a specific tool
//! let result = client.call_tool("read_file", json!({"path": "/tmp/test.txt"})).await?;
//! println!("Result: {}", result.content);
//!
//! // Disconnect
//! client.disconnect().await?;
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use oneai_tool::mcp_real::{McpServerConfig, McpTransport, McpToolInfo};
use oneai_tool::RealMcpServerManager;
use oneai_core::traits::Tool;
use oneai_core::ToolOutput;

use crate::error::McpError;

// ─── McpClient ──────────────────────────────────────────────────────────────────

/// Standalone MCP client for connecting to a single external MCP server.
///
/// Wraps `McpServerManager` to provide a simpler, focused API for:
/// - Connecting to a server (stdio, SSE, or streamable_http)
/// - Discovering available tools
/// - Invoking specific tools
/// - Disconnecting
///
/// This is the recommended API for one-off MCP server connections.
/// For persistent multi-server management, use `McpPluginRegistry` instead.
pub struct McpClient {
    /// Configuration for the MCP server to connect to.
    config: McpServerConfig,
    /// The underlying server manager (wrapped in Mutex for thread-safe access).
    manager: Arc<Mutex<RealMcpServerManager>>,
    /// Whether the client is currently connected.
    connected: Arc<Mutex<bool>>,
}

impl McpClient {
    /// Create a client for a stdio-based MCP server.
    ///
    /// Launches a subprocess and communicates via stdin/stdout using
    /// the MCP Content-Length framing protocol.
    ///
    /// **Usage**:
    /// ```ignore
    /// let client = McpClient::stdio("npx", &["-y", "@anthropic/mcp-server-filesystem"]);
    /// ```
    pub fn stdio(command: &str, args: &[&str]) -> Self {
        let config = McpServerConfig {
            name: "mcp-client".to_string(),
            transport: McpTransport::Stdio {
                command: command.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
        };
        Self {
            config,
            manager: Arc::new(Mutex::new(RealMcpServerManager::new())),
            connected: Arc::new(Mutex::new(false)),
        }
    }

    /// Create a client for an SSE-based MCP server.
    ///
    /// Connects via HTTP to the server's SSE endpoint for receiving
    /// events and POST endpoint for sending requests.
    pub fn sse(url: &str) -> Self {
        let config = McpServerConfig {
            name: "mcp-client-sse".to_string(),
            transport: McpTransport::Sse {
                url: url.to_string(),
                headers: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
        };
        Self {
            config,
            manager: Arc::new(Mutex::new(RealMcpServerManager::new())),
            connected: Arc::new(Mutex::new(false)),
        }
    }

    /// Create a client for a StreamableHttp MCP server.
    ///
    /// Uses the newer streamable HTTP transport that combines POST requests
    /// with SSE response streams.
    pub fn streamable_http(url: &str) -> Self {
        let config = McpServerConfig {
            name: "mcp-client-http".to_string(),
            transport: McpTransport::StreamableHttp {
                url: url.to_string(),
                headers: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
        };
        Self {
            config,
            manager: Arc::new(Mutex::new(RealMcpServerManager::new())),
            connected: Arc::new(Mutex::new(false)),
        }
    }

    /// Create a client from a custom McpServerConfig.
    pub fn from_config(config: McpServerConfig) -> Self {
        Self {
            config,
            manager: Arc::new(Mutex::new(RealMcpServerManager::new())),
            connected: Arc::new(Mutex::new(false)),
        }
    }

    /// Get the server configuration.
    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    /// Connect to the MCP server.
    ///
    /// Establishes the transport connection, performs the MCP
    /// initialization handshake, and discovers available tools.
    /// After connecting, tools can be queried and invoked.
    ///
    /// Returns the list of discovered tool names.
    pub async fn connect(&self) -> crate::error::Result<Vec<String>> {
        let mut manager = self.manager.lock().await;
        let tool_names = manager.connect_server(self.config.clone()).await
            .map_err(|e| McpError::Connection(e.to_string()))?;

        let mut connected = self.connected.lock().await;
        *connected = true;
        Ok(tool_names)
    }

    /// Discover available tools from the connected MCP server.
    ///
    /// Returns a list of `McpToolInfo` describing each tool's name,
    /// description, and input schema. This uses the discovered tools
    /// from the `connect()` phase.
    pub async fn discover_tools(&self) -> crate::error::Result<Vec<McpToolInfo>> {
        let manager = self.manager.lock().await;
        let wrappers = manager.all_tool_wrappers();

        let tool_infos: Vec<McpToolInfo> = wrappers.iter().map(|w| {
            let tool: &dyn Tool = w.as_ref();
            McpToolInfo {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters_schema: tool.parameters_schema(),
                server_name: "mcp-client".to_string(),
            }
        }).collect();
        Ok(tool_infos)
    }

    /// Call a specific tool on the connected MCP server.
    ///
    /// Invokes the MCP `tools/call` method with the given tool name
    /// and arguments. Returns the tool's output.
    pub async fn call_tool(&self, tool_name: &str, arguments: serde_json::Value) -> crate::error::Result<ToolOutput> {
        let manager = self.manager.lock().await;

        // Find the tool wrapper
        let wrapper = manager.get_tool_wrapper(tool_name)
            .ok_or_else(|| McpError::ToolNotFound(tool_name.to_string()))?;

        // Execute the tool (McpToolWrapper implements Tool trait)
        let tool: &dyn Tool = wrapper.as_ref();
        let result = tool.execute(arguments).await
            .map_err(|e| McpError::Execution(e.to_string()))?;

        Ok(result)
    }

    /// Disconnect from the MCP server.
    ///
    /// Closes the transport connection and cleans up resources.
    pub async fn disconnect(&self) -> crate::error::Result<()> {
        let mut connected = self.connected.lock().await;
        *connected = false;

        // The McpServerManager doesn't have an explicit disconnect method,
        // but dropping the connection closes the subprocess/socket.
        // We reset the manager to a fresh state.
        let mut manager = self.manager.lock().await;
        *manager = RealMcpServerManager::new();

        Ok(())
    }

    /// Check if the client is currently connected.
    pub async fn is_connected(&self) -> bool {
        *self.connected.lock().await
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_client_stdio_creation() {
        let client = McpClient::stdio("npx", &["-y", "@anthropic/mcp-server-filesystem"]);
        assert_eq!(client.config().name, "mcp-client");
        assert!(matches!(client.config().transport, McpTransport::Stdio { .. }));
    }

    #[test]
    fn test_mcp_client_sse_creation() {
        let client = McpClient::sse("http://localhost:3001/sse");
        assert_eq!(client.config().name, "mcp-client-sse");
        assert!(matches!(client.config().transport, McpTransport::Sse { .. }));
    }

    #[test]
    fn test_mcp_client_streamable_http_creation() {
        let client = McpClient::streamable_http("http://localhost:3001/mcp");
        assert_eq!(client.config().name, "mcp-client-http");
        assert!(matches!(client.config().transport, McpTransport::StreamableHttp { .. }));
    }

    #[test]
    fn test_mcp_client_from_config() {
        let config = McpServerConfig {
            name: "custom-server".to_string(),
            transport: McpTransport::Stdio {
                command: "my-mcp-server".to_string(),
                args: vec!["--port".to_string(), "8080".to_string()],
                env: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
        };
        let client = McpClient::from_config(config);
        assert_eq!(client.config().name, "custom-server");
    }

    #[tokio::test]
    async fn test_mcp_client_initially_not_connected() {
        let client = McpClient::stdio("echo", &[]);
        assert!(!client.is_connected().await);
    }
}
