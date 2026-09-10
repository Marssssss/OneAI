//! Orchestrator error type.

/// Errors surfaced by the orchestrator control plane.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OrchestratorError {
    /// Container backend (docker CLI) failure; carries stderr/exit context.
    #[error("container backend error: {0}")]
    Runner(String),

    /// Session not found in the routing table.
    #[error("session not found: {0}")]
    NotFound(String),

    /// Session id already taken.
    #[error("session already exists: {0}")]
    AlreadyExists(String),

    /// Illegal FSM transition (see `fsm::validate_transition`).
    #[error("illegal state transition: {from:?} -> {to:?}")]
    IllegalTransition {
        from: crate::fsm::SessionState,
        to: crate::fsm::SessionState,
    },

    /// CAS miss: the session state moved underneath the caller.
    #[error("state CAS miss for session {id}: expected {expected:?}, found {found:?}")]
    CasMiss {
        id: String,
        expected: crate::fsm::SessionState,
        found: crate::fsm::SessionState,
    },

    /// Timed out waiting for a session to become ready (resume/spawn).
    #[error("timed out waiting for session {0} to become ready")]
    ResumeTimeout(String),

    /// Session is in a state that cannot serve requests.
    #[error("session {id} is not runnable (state {state:?}): {reason}")]
    NotRunnable {
        id: String,
        state: crate::fsm::SessionState,
        reason: String,
    },

    /// Invalid session id (must match `[a-zA-Z0-9_-]+`).
    #[error("invalid session id: {0}")]
    InvalidSessionId(String),

    /// Registry persistence / load failure.
    #[error("registry persistence error: {0}")]
    Persist(String),

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),

    /// Local IO failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, OrchestratorError>;
