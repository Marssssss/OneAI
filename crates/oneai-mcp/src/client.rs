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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use oneai_core::traits::Tool;
use oneai_core::ToolOutput;
use oneai_tool::mcp_real::{McpServerConfig, McpToolInfo, McpTransport};
use oneai_tool::RealMcpServerManager;

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
    /// The underlying server manager. Fully internally synchronized
    /// (`&self` methods), so the client shares a single `Arc` with no outer
    /// `Mutex` — `discover_tools` / `call_tool` don't serialize each other.
    manager: Arc<RealMcpServerManager>,
    /// Whether the client is currently connected.
    connected: AtomicBool,
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
            ..Default::default()
        };
        Self::from_config(config)
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
            ..Default::default()
        };
        Self::from_config(config)
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
            ..Default::default()
        };
        Self::from_config(config)
    }

    /// Create a client from a custom McpServerConfig.
    pub fn from_config(config: McpServerConfig) -> Self {
        Self {
            config,
            manager: Arc::new(RealMcpServerManager::new()),
            connected: AtomicBool::new(false),
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
        let tool_names = self
            .manager
            .connect_server(self.config.clone())
            .await
            .map_err(|e| McpError::Connection(e.to_string()))?;

        self.connected.store(true, Ordering::Relaxed);
        Ok(tool_names)
    }

    /// Discover available tools from the connected MCP server.
    ///
    /// Returns a list of `McpToolInfo` describing each tool's name,
    /// description, and input schema. This uses the discovered tools
    /// from the `connect()` phase.
    pub async fn discover_tools(&self) -> crate::error::Result<Vec<McpToolInfo>> {
        let wrappers = self.manager.all_tool_wrappers().await;

        let tool_infos: Vec<McpToolInfo> = wrappers
            .iter()
            .map(|w| {
                let tool: &dyn Tool = w.as_ref();
                McpToolInfo {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters_schema: tool.parameters_schema(),
                    server_name: "mcp-client".to_string(),
                }
            })
            .collect();
        Ok(tool_infos)
    }

    /// Call a specific tool on the connected MCP server.
    ///
    /// Invokes the MCP `tools/call` method with the given tool name
    /// and arguments. Returns the tool's output.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> crate::error::Result<ToolOutput> {
        // Find the tool wrapper (owned Arc — the read lock is released here).
        let wrapper = self
            .manager
            .get_tool_wrapper(tool_name)
            .await
            .ok_or_else(|| McpError::ToolNotFound(tool_name.to_string()))?;

        // Execute the tool (McpToolWrapper implements Tool trait)
        let tool: &dyn Tool = wrapper.as_ref();
        let result = tool
            .execute(arguments)
            .await
            .map_err(|e| McpError::Execution(e.to_string()))?;

        Ok(result)
    }

    /// Disconnect from the MCP server.
    ///
    /// Closes the transport connection and cleans up resources via the
    /// manager's `shutdown_all` (clears all bindings + shuts down the
    /// underlying connections).
    pub async fn disconnect(&self) -> crate::error::Result<()> {
        self.connected.store(false, Ordering::Relaxed);
        self.manager
            .shutdown_all()
            .await
            .map_err(|e| McpError::Connection(e.to_string()))?;
        Ok(())
    }

    /// Check if the client is currently connected.
    pub async fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
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
        assert!(matches!(
            client.config().transport,
            McpTransport::Stdio { .. }
        ));
    }

    #[test]
    fn test_mcp_client_sse_creation() {
        let client = McpClient::sse("http://localhost:3001/sse");
        assert_eq!(client.config().name, "mcp-client-sse");
        assert!(matches!(
            client.config().transport,
            McpTransport::Sse { .. }
        ));
    }

    #[test]
    fn test_mcp_client_streamable_http_creation() {
        let client = McpClient::streamable_http("http://localhost:3001/mcp");
        assert_eq!(client.config().name, "mcp-client-http");
        assert!(matches!(
            client.config().transport,
            McpTransport::StreamableHttp { .. }
        ));
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
            ..Default::default()
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
