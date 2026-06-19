//! MCP crate error types.

use thiserror::Error;

/// Error type for MCP client/server operations.
#[derive(Debug, Error)]
pub enum McpError {
    /// Connection error (failed to connect to MCP server).
    #[error("MCP connection error: {0}")]
    Connection(String),

    /// Discovery error (failed to discover tools).
    #[error("MCP discovery error: {0}")]
    Discovery(String),

    /// Tool not found in the connected server.
    #[error("MCP tool not found: {0}")]
    ToolNotFound(String),

    /// Tool execution error.
    #[error("MCP tool execution error: {0}")]
    Execution(String),

    /// Configuration error.
    #[error("MCP config error: {0}")]
    Config(String),
}

/// Convenience type alias for MCP Results.
pub type Result<T> = std::result::Result<T, McpError>;
