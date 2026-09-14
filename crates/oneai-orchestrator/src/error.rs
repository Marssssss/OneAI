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

    /// Invalid tenant id (MVS4-B: `[a-zA-Z0-9_-]*`, ≤64, empty allowed).
    #[error("invalid tenant id: {0}")]
    InvalidTenantId(String),

    /// Tenant quota exceeded (MVS4-B): concurrent-session cap, token budget
    /// or per-replica create rate. Maps to HTTP 429; `retry_after_secs` is
    /// `Some` only for the rate-limit reason (the others need a deletion or
    /// a budget change — retrying alone won't help).
    #[error("quota exceeded for tenant '{tenant}': {message}")]
    QuotaExceeded {
        tenant: String,
        reason: crate::quota::QuotaReason,
        limit: u64,
        current: u64,
        retry_after_secs: Option<u64>,
        message: String,
    },

    /// Registry persistence / load failure.
    #[error("registry persistence error: {0}")]
    Persist(String),

    /// Configuration error.
    #[error("config error: {0}")]
    Config(String),

    /// Shared session-store (Postgres) failure — the multi-replica backend
    /// (MVS4-A). Surfaces the server-side SQLSTATE + detail via pg_common's
    /// error mapping.
    #[error("session store (Pg) error: {0}")]
    Pg(String),

    /// A lease we believed we held is gone (lost to a takeover after a
    /// renewal gap). Carries `session_id`.
    #[error("lease lost for session {0} (taken over by another replica?)")]
    LeaseLost(String),

    /// Local IO failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(feature = "postgres")]
impl From<oneai_core::error::OneAIError> for OrchestratorError {
    fn from(e: oneai_core::error::OneAIError) -> Self {
        OrchestratorError::Pg(e.to_string())
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, OrchestratorError>;
