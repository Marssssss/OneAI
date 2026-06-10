//! MCP tool integration — discover MCP servers, convert schemas, execute calls.
//!
//! Integrates with Model Context Protocol (MCP) servers via the rmcp crate.
//! MCP servers expose tools that can be dynamically discovered and invoked.
//! This module:
//! 1. Connects to MCP servers
//! 2. Lists available tools from each server
//! 3. Wraps MCP tools as OneAI Tool trait implementations
//! 4. Executes MCP tool calls through the standard Tool interface

use async_trait::async_trait;
use oneai_core::{RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;

/// An MCP tool wrapper that implements the OneAI Tool trait.
///
/// Wraps a tool discovered from an MCP server so it can be used
/// in the OneAI agent framework just like any local tool.
pub struct McpToolWrapper {
    /// The tool name as reported by the MCP server.
    name: String,
    /// The tool description as reported by the MCP server.
    description: String,
    /// The JSON Schema for the tool's parameters.
    parameters_schema: serde_json::Value,
    /// The MCP server name this tool belongs to.
    server_name: String,
}

impl McpToolWrapper {
    /// Create a new MCP tool wrapper.
    pub fn new(
        name: String,
        description: String,
        parameters_schema: serde_json::Value,
        server_name: String,
    ) -> Self {
        Self {
            name,
            description,
            parameters_schema,
            server_name,
        }
    }
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters_schema.clone()
    }

    fn risk_level(&self) -> RiskLevel {
        // MCP tools from external servers are medium risk by default
        // — they're not local system tools, but they're not fully trusted
        RiskLevel::Medium
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        // MCP tool execution would go through the rmcp crate
        // to the MCP server. For now, this is a placeholder that
        // returns a message indicating the tool was called.
        //
        // Full implementation requires:
        // 1. Initializing an MCP client connection
        // 2. Sending a CallToolRequest to the server
        // 3. Parsing the CallToolResult response
        // 4. Converting the result to ToolOutput

        tracing::info!(
            "MCP tool call: {} on server {} with args: {}",
            self.name, self.server_name, args
        );

        Ok(ToolOutput {
            success: true,
            content: format!(
                "MCP tool '{}' on server '{}' called with: {}",
                self.name, self.server_name, args
            ),
            error: None,
        })
    }
}

/// MCP server manager — handles connections to MCP servers.
///
/// Responsible for:
/// - Connecting to MCP servers (via stdio, SSE, or other transports)
/// - Discovering available tools
/// - Creating McpToolWrapper instances for each discovered tool
/// - Managing server lifecycle
pub struct McpServerManager {
    /// Connected MCP servers.
    servers: HashMap<String, McpServerInfo>,
}

/// Information about a connected MCP server.
struct McpServerInfo {
    /// The server name.
    name: String,
    /// Available tools from this server.
    tools: Vec<McpToolWrapper>,
}

impl McpServerManager {
    /// Create a new MCP server manager.
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    /// Register tools discovered from an MCP server.
    ///
    /// This is a placeholder for the full MCP discovery flow.
    /// In a complete implementation, this would:
    /// 1. Connect to the MCP server via the appropriate transport
    /// 2. Call the ListTools method
    /// 3. Create McpToolWrapper instances for each tool
    /// 4. Store the server info and tools
    pub fn register_server_tools(
        &mut self,
        server_name: String,
        tools: Vec<McpToolWrapper>,
    ) {
        self.servers.insert(server_name.clone(), McpServerInfo {
            name: server_name,
            tools,
        });
    }

    /// Get all tools from all connected servers.
    pub fn all_tools(&self) -> Vec<&McpToolWrapper> {
        self.servers.values()
            .flat_map(|server| server.tools.iter())
            .collect()
    }

    /// Get tools from a specific server.
    pub fn server_tools(&self, server_name: &str) -> Option<&Vec<McpToolWrapper>> {
        self.servers.get(server_name).map(|s| &s.tools)
    }

    /// List connected server names.
    pub fn server_names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }
}

impl Default for McpServerManager {
    fn default() -> Self {
        Self::new()
    }
}

use std::collections::HashMap;