//! Orchestrator configuration.
//!
//! Load order: struct defaults → `~/.oneai/orchestrator.toml` (if present) →
//! CLI flag overrides. Mirrors the `oneai-mcp/src/config.rs` file pattern.
//!
//! ```toml
//! # ~/.oneai/orchestrator.toml
//! listen = "127.0.0.1:9191"
//! image = "oneai-engine:mvs1"
//! container_bind_host = "127.0.0.1"
//! container_port = 8787
//! idle_timeout_secs = 1800
//! resume_timeout_secs = 60
//! registry_dir = "/home/me/.oneai/orchestrator"
//! # Host file bind-mounted read-only into every session container as
//! # /home/oneai/.oneai/config.toml (provider keys — the MVS1 D5 shortcut).
//! provider_config = "/home/me/.oneai/config.toml"
//! # Env var NAMES copied from the orchestrator's own environment into every
//! # spawned container (values resolved at spawn time).
//! passthrough_env = ["OPENAI_API_KEY", "ANTHROPIC_API_KEY"]
//! docker_bin = "docker"
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default control-plane listen address.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:9191";
/// Default session container image (built by `deploy/docker/Dockerfile`).
pub const DEFAULT_IMAGE: &str = "oneai-engine:mvs1";
/// Default container-internal port (`oneai web` HTTP+WS+SPA).
pub const DEFAULT_CONTAINER_PORT: u16 = 8787;
/// Default idle timeout before auto-hibernation (30 min).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;
/// Default time a WS upgrade waits for a resuming session (seconds).
pub const DEFAULT_RESUME_TIMEOUT_SECS: u64 = 60;
/// Bearer secret env var for the frontend → orchestrator channel (D7).
pub const ORCHESTRATOR_SECRET_ENV: &str = "ONEAI_ORCHESTRATOR_SECRET";

/// Orchestrator configuration. See the module docs for the TOML shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OrchestratorConfig {
    /// Control-plane listen address. Default `127.0.0.1:9191`.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Container image for session containers. Default `oneai-engine:mvs1`.
    #[serde(default = "default_image")]
    pub image: String,
    /// Host interface container ports are published on. Default `127.0.0.1`
    /// — the D7 mitigation: containers are reachable only from the
    /// orchestrator host, never from the LAN (per-session internal ws auth
    /// is deferred until the engine grows an optional ws auth hook).
    #[serde(default = "default_bind_host")]
    pub container_bind_host: String,
    /// Container-internal port (`oneai web`). Default 8787.
    #[serde(default = "default_container_port")]
    pub container_port: u16,
    /// Idle seconds before a Running session auto-hibernates. Default 1800.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Seconds a WS upgrade waits for Creating/Resuming sessions. Default 60.
    #[serde(default = "default_resume_timeout")]
    pub resume_timeout_secs: u64,
    /// Directory holding `sessions.json` (routing-table persistence).
    /// Default `~/.oneai/orchestrator`.
    #[serde(default = "default_registry_dir")]
    pub registry_dir: PathBuf,
    /// Host file bind-mounted read-only into every container as the engine
    /// `config.toml` (provider keys — the MVS1 D5 bind-mount shortcut).
    #[serde(default)]
    pub provider_config: Option<PathBuf>,
    /// Env var NAMES copied from the orchestrator environment into every
    /// spawned container (values resolved at spawn time; missing names are
    /// skipped).
    #[serde(default)]
    pub passthrough_env: Vec<String>,
    /// Docker CLI binary. Default `docker`.
    #[serde(default = "default_docker_bin")]
    pub docker_bin: String,
}

fn default_listen() -> String {
    DEFAULT_LISTEN.to_string()
}
fn default_image() -> String {
    DEFAULT_IMAGE.to_string()
}
fn default_bind_host() -> String {
    "127.0.0.1".to_string()
}
fn default_container_port() -> u16 {
    DEFAULT_CONTAINER_PORT
}
fn default_idle_timeout() -> u64 {
    DEFAULT_IDLE_TIMEOUT_SECS
}
fn default_resume_timeout() -> u64 {
    DEFAULT_RESUME_TIMEOUT_SECS
}
fn default_docker_bin() -> String {
    "docker".to_string()
}

fn default_registry_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".oneai")
        .join("orchestrator")
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            image: default_image(),
            container_bind_host: default_bind_host(),
            container_port: default_container_port(),
            idle_timeout_secs: default_idle_timeout(),
            resume_timeout_secs: default_resume_timeout(),
            registry_dir: default_registry_dir(),
            provider_config: None,
            passthrough_env: Vec::new(),
            docker_bin: default_docker_bin(),
        }
    }
}

impl OrchestratorConfig {
    /// Load from an explicit TOML path (errors if the file exists but is
    /// malformed).
    pub fn load_from(path: &std::path::Path) -> crate::error::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| crate::error::OrchestratorError::Config(format!("{path:?}: {e}")))?;
        toml::from_str(&text)
            .map_err(|e| crate::error::OrchestratorError::Config(format!("{path:?}: {e}")))
    }

    /// Load from the default location `~/.oneai/orchestrator.toml`; missing
    /// file is fine (defaults apply), malformed file is an error.
    pub fn load_default() -> crate::error::Result<Self> {
        let path = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".oneai")
            .join("orchestrator.toml");
        if path.exists() {
            Self::load_from(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Apply CLI overrides (any `Some(..)` wins over file/default values).
    pub fn with_overrides(
        mut self,
        listen: Option<&str>,
        image: Option<&str>,
        registry_dir: Option<&str>,
        idle_timeout_secs: Option<u64>,
    ) -> Self {
        if let Some(v) = listen {
            self.listen = v.to_string();
        }
        if let Some(v) = image {
            self.image = v.to_string();
        }
        if let Some(v) = registry_dir {
            self.registry_dir = PathBuf::from(v);
        }
        if let Some(v) = idle_timeout_secs {
            self.idle_timeout_secs = v;
        }
        self
    }

    /// Resolve the env pairs to inject into a session container: every name
    /// in `passthrough_env` that is set in the orchestrator's own
    /// environment.
    pub fn resolved_passthrough_env(&self) -> Vec<(String, String)> {
        self.passthrough_env
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|v| (name.clone(), v)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_consts() {
        let c = OrchestratorConfig::default();
        assert_eq!(c.listen, DEFAULT_LISTEN);
        assert_eq!(c.image, DEFAULT_IMAGE);
        assert_eq!(c.container_port, DEFAULT_CONTAINER_PORT);
        assert_eq!(c.idle_timeout_secs, DEFAULT_IDLE_TIMEOUT_SECS);
        assert_eq!(c.resume_timeout_secs, DEFAULT_RESUME_TIMEOUT_SECS);
        assert_eq!(c.container_bind_host, "127.0.0.1");
        assert!(c.registry_dir.ends_with(".oneai/orchestrator"));
    }

    #[test]
    fn toml_parse_partial_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchestrator.toml");
        std::fs::write(
            &path,
            r#"
            listen = "0.0.0.0:9999"
            idle_timeout_secs = 60
            passthrough_env = ["OPENAI_API_KEY"]
            "#,
        )
        .unwrap();
        let c = OrchestratorConfig::load_from(&path).unwrap();
        assert_eq!(c.listen, "0.0.0.0:9999");
        assert_eq!(c.idle_timeout_secs, 60);
        assert_eq!(c.passthrough_env, vec!["OPENAI_API_KEY"]);
        // untouched fields keep defaults
        assert_eq!(c.image, DEFAULT_IMAGE);
        assert_eq!(c.container_port, DEFAULT_CONTAINER_PORT);
    }

    #[test]
    fn toml_parse_malformed_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "listen = ").unwrap();
        assert!(OrchestratorConfig::load_from(&path).is_err());
    }

    #[test]
    fn cli_overrides_win() {
        let c = OrchestratorConfig::default().with_overrides(
            Some("127.0.0.1:1234"),
            Some("img:latest"),
            Some("/tmp/reg"),
            Some(5),
        );
        assert_eq!(c.listen, "127.0.0.1:1234");
        assert_eq!(c.image, "img:latest");
        assert_eq!(c.registry_dir, PathBuf::from("/tmp/reg"));
        assert_eq!(c.idle_timeout_secs, 5);
    }

    #[test]
    fn passthrough_env_resolves_only_set_names() {
        let c = OrchestratorConfig {
            passthrough_env: vec![
                "ONEAI_ORCH_TEST_SET".to_string(),
                "ONEAI_ORCH_TEST_UNSET".to_string(),
            ],
            ..OrchestratorConfig::default()
        };
        std::env::set_var("ONEAI_ORCH_TEST_SET", "v1");
        std::env::remove_var("ONEAI_ORCH_TEST_UNSET");
        let env = c.resolved_passthrough_env();
        assert_eq!(
            env,
            vec![("ONEAI_ORCH_TEST_SET".to_string(), "v1".to_string())]
        );
        std::env::remove_var("ONEAI_ORCH_TEST_SET");
    }
}
