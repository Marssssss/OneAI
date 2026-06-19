//! MCP Discovery — one-shot "connect_and_discover" for external MCP servers.
//!
//! McpDiscovery provides a convenience API for connecting to an MCP server,
//! discovering its tools, and optionally disconnecting immediately. This is
//! useful for quick inspection of what tools an MCP server offers without
//! maintaining a persistent connection.
//!
//! ## Usage
//! ```ignore
//! // One-shot discovery — connect, list tools, disconnect
//! let tools = McpDiscovery::connect_and_discover(
//!     &McpServerConfig::stdio_command("npx", &["@anthropic/mcp-server-filesystem"])
//! ).await?;
//!
//! for tool in &tools {
//!     println!("  • {} — {}", tool.name, tool.description);
//! }
//! ```

use oneai_tool::mcp_real::{McpServerConfig, McpToolInfo};
use crate::client::McpClient;
use crate::error::McpError;

// ─── McpDiscovery ────────────────────────────────────────────────────────────────

/// One-shot MCP server discovery — connect, list tools, disconnect.
///
/// Connects to an MCP server, discovers its available tools, and returns
/// the tool metadata without keeping the connection alive. For persistent
/// connections, use `McpClient` directly.
///
/// This is the programmatic equivalent of `oneai mcp discover <url>`.
pub struct McpDiscovery;

impl McpDiscovery {
    /// Connect to an MCP server, discover tools, and disconnect.
    ///
    /// This is a one-shot operation: the connection is opened, tools are
    /// discovered via the `tools/list` MCP method, and the connection is
    /// immediately closed. The returned `McpToolInfo` list contains only
    /// metadata (name, description, input_schema) — not the tool wrappers.
    ///
    /// **Usage**:
    /// ```ignore
    /// let config = McpServerConfig {
    ///     name: "filesystem".to_string(),
    ///     transport: McpTransport::Stdio {
    ///         command: "npx".to_string(),
    ///         args: vec!["-y".to_string(), "@anthropic/mcp-server-filesystem".to_string()],
    ///     },
    ///     enabled: true,
    /// };
    /// let tools = McpDiscovery::connect_and_discover(&config).await?;
    /// ```
    pub async fn connect_and_discover(config: &McpServerConfig) -> crate::error::Result<Vec<McpToolInfo>> {
        let client = McpClient::from_config(config.clone());

        // Connect
        client.connect().await
            .map_err(|e| McpError::Connection(format!("Discovery connection failed: {}", e)))?;

        // Discover tools
        let tools = client.discover_tools().await
            .map_err(|e| McpError::Discovery(format!("Discovery failed: {}", e)))?;

        // Disconnect (best effort — don't fail if disconnect fails)
        let _ = client.disconnect().await;

        Ok(tools)
    }

    /// Discover tools from a stdio-based MCP server.
    ///
    /// Convenience method that creates the config and performs discovery.
    pub async fn discover_stdio(command: &str, args: &[&str]) -> crate::error::Result<Vec<McpToolInfo>> {
        let config = McpServerConfig {
            name: "discovery-stdio".to_string(),
            transport: oneai_tool::mcp_real::McpTransport::Stdio {
                command: command.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: std::collections::HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
        };
        Self::connect_and_discover(&config).await
    }

    /// Discover tools from an SSE-based MCP server.
    ///
    /// Convenience method for HTTP-based MCP servers.
    pub async fn discover_sse(url: &str) -> crate::error::Result<Vec<McpToolInfo>> {
        let config = McpServerConfig {
            name: "discovery-sse".to_string(),
            transport: oneai_tool::mcp_real::McpTransport::Sse {
                url: url.to_string(),
                headers: std::collections::HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
        };
        Self::connect_and_discover(&config).await
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_discovery_config_creation_stdio() {
        let config = McpServerConfig {
            name: "test-stdio".to_string(),
            transport: oneai_tool::mcp_real::McpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string()],
                env: std::collections::HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
        };
        assert_eq!(config.name, "test-stdio");
    }

    #[test]
    fn test_mcp_discovery_config_creation_sse() {
        let config = McpServerConfig {
            name: "test-sse".to_string(),
            transport: oneai_tool::mcp_real::McpTransport::Sse {
                url: "http://localhost:3001/sse".to_string(),
                headers: std::collections::HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
        };
        assert_eq!(config.name, "test-sse");
    }

    #[test]
    fn test_mcp_tool_info_metadata() {
        // Verify McpToolInfo fields are accessible
        let info = McpToolInfo {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"}
                },
                "required": ["path"]
            }),
            server_name: "filesystem".to_string(),
        };
        assert_eq!(info.name, "read_file");
        assert_eq!(info.description, "Read a file");
    }
}
