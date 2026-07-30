//! Crate-wide error type for `oneai-supervisor`.

use std::io;

use thiserror::Error;

/// Result alias for supervisor operations.
pub type Result<T> = std::result::Result<T, SupervisorError>;

/// Errors raised by the supervisor crate.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SupervisorError {
    /// An instance id was not found in the registry.
    #[error("instance not found: {0}")]
    InstanceNotFound(String),

    /// An instance id is already registered.
    #[error("instance already exists: {0}")]
    InstanceExists(String),

    /// The runner has no configured LLM provider.
    #[error("no LLM provider configured")]
    NoProvider,

    /// An instance-level operation failed (delegated from the runner).
    #[error("instance error: {0}")]
    Instance(String),

    /// The IPC protocol was violated (malformed line, bad method, bad params).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The transport layer reported an I/O error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// JSON (de)serialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
