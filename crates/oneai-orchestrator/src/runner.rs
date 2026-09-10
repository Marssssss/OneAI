//! Container backend abstraction.
//!
//! `ContainerRunner` is the seam between the orchestrator (FSM, routing
//! table, control plane) and the container engine. MVS2 ships `DockerRunner`
//! (`docker.rs`); MVS4 adds `K8sRunner`. Tests use an in-memory fake
//! (`tests/common/mod.rs`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Immutable specification handed to [`ContainerRunner::spawn`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    /// Caller-chosen unique session id; validated `[a-zA-Z0-9_-]+`.
    pub session_id: String,
    /// Container image, e.g. `oneai-engine:mvs1`.
    pub image: String,
    /// Named volume holding engine state (`~/.oneai`: SQLite + JSONL).
    pub state_volume: String,
    /// Named volume holding the workspace (`/workspace`).
    pub workspace_volume: String,
    /// Env vars injected into the container (D5).
    pub env: Vec<(String, String)>,
    /// Host interface the container port is published on (default
    /// `127.0.0.1` — the D7 mitigation, see `config.rs`).
    pub bind_host: String,
    /// Container-internal port the engine listens on.
    pub container_port: u16,
    /// Optional host file bind-mounted read-only as the engine config.toml.
    pub provider_config: Option<std::path::PathBuf>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

impl SessionSpec {
    /// Validate a session id: non-empty, ≤64 chars, `[a-zA-Z0-9_-]+` so it
    /// is safe to embed in container/volume names.
    pub fn validate_session_id(id: &str) -> Result<()> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(crate::error::OrchestratorError::InvalidSessionId(
                id.to_string(),
            ));
        }
        Ok(())
    }

    /// Docker container name for this session (`oneai-orch-<id>`).
    pub fn container_name(&self) -> String {
        container_name(&self.session_id)
    }
}

/// Canonical container name for a session id.
pub fn container_name(session_id: &str) -> String {
    format!("oneai-orch-{session_id}")
}

/// Canonical state-volume name for a session id.
pub fn state_volume_name(session_id: &str) -> String {
    format!("oneai-orch-{session_id}-state")
}

/// Canonical workspace-volume name for a session id.
pub fn workspace_volume_name(session_id: &str) -> String {
    format!("oneai-orch-{session_id}-ws")
}

/// What the routing table needs to reach and manage a spawned container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerHandle {
    /// Backend container id (docker long id).
    pub container_id: String,
    /// Backend container name (`oneai-orch-<session_id>`).
    pub container_name: String,
    /// Host port the container port is published on. Dynamic — re-inspected
    /// after every (re)start.
    pub host_port: u16,
}

/// Container backend. All methods are idempotent-friendly: `spawn` creates
/// volumes if missing; `destroy` tolerates an already-gone container.
#[async_trait]
pub trait ContainerRunner: Send + Sync {
    /// Create + start a container for `spec`. Returns the handle with the
    /// inspected dynamic host port.
    async fn spawn(&self, spec: &SessionSpec) -> Result<ContainerHandle>;

    /// Stop the container, preserving volumes (hibernate, D6).
    async fn stop(&self, handle: &ContainerHandle) -> Result<()>;

    /// Start an existing stopped container. The dynamic host port may have
    /// changed — implementations MUST re-inspect and return a fresh handle.
    async fn start(&self, handle: &ContainerHandle) -> Result<ContainerHandle>;

    /// Health probe: `Ok(true)` = container running AND engine port
    /// accepting connections; `Ok(false)`/`Err` = unhealthy/dead.
    async fn health(&self, handle: &ContainerHandle) -> Result<bool>;

    /// Commit the container FS layer to `tag` (snapshot-based hibernation,
    /// optional D6 path).
    async fn commit(&self, handle: &ContainerHandle, tag: &str) -> Result<()>;

    /// Remove the container; when `remove_volumes` also remove its named
    /// volumes (destroy path — `false` keeps them for later resume).
    async fn destroy(&self, handle: &ContainerHandle, remove_volumes: bool) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_validation() {
        assert!(SessionSpec::validate_session_id("abc-123_X").is_ok());
        assert!(SessionSpec::validate_session_id("").is_err());
        assert!(SessionSpec::validate_session_id("a b").is_err());
        assert!(SessionSpec::validate_session_id("a/b").is_err());
        assert!(SessionSpec::validate_session_id("..").is_err());
        assert!(SessionSpec::validate_session_id(&"x".repeat(65)).is_err());
        assert!(SessionSpec::validate_session_id(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn naming_conventions() {
        assert_eq!(container_name("s1"), "oneai-orch-s1");
        assert_eq!(state_volume_name("s1"), "oneai-orch-s1-state");
        assert_eq!(workspace_volume_name("s1"), "oneai-orch-s1-ws");
    }
}
