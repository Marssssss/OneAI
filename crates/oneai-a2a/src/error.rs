//! A2A Protocol error types.

use thiserror::Error;

use crate::types::TaskState;

/// Error type for A2A protocol operations.
#[derive(Debug, Error)]
pub enum A2AError {
    /// Protocol-level error (malformed response, unsupported method, etc.)
    #[error("A2A protocol error: {0}")]
    Protocol(String),

    /// Network/HTTP transport error
    #[error("A2A network error: {0}")]
    Network(String),

    /// Task not found
    #[error("A2A task not found: {0}")]
    TaskNotFound(String),

    /// Invalid task state transition
    #[error("A2A invalid state transition: from {from} to {to}")]
    InvalidStateTransition {
        from: TaskState,
        to: TaskState,
    },

    /// Serialization/deserialization error
    #[error("A2A serialization error: {0}")]
    Serialization(String),

    /// JSON-RPC error response from the remote agent
    #[error("A2A JSON-RPC error: code={code}, message={message}")]
    JsonRpcError {
        code: i64,
        message: String,
    },

    /// Agent discovery error (failed to fetch AgentCard)
    #[error("A2A agent discovery error: {0}")]
    Discovery(String),

    /// Timeout waiting for task completion
    #[error("A2A timeout: {0}")]
    Timeout(String),
}

/// Convenience type alias for A2A Results.
pub type Result<T> = std::result::Result<T, A2AError>;
