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
//! # MVS3-C deep hibernation: a Hibernating session idle for this many extra
//! # seconds has its volumes archived (tar.gz) under archive_dir, then the
//! # container + local volumes are destroyed; resume restores them. 0/unset
//! # = disabled. Needs the throwaway helper image (alpine:3.20) pullable.
//! deep_archive_timeout_secs = 86400
//! archive_dir = "/srv/oneai-archive"
//! # MVS4-A multi-replica: shared Postgres routing table + per-session
//! # leases. Env `ONEAI_PG_DSN` wins over this field. Requires
//! # lease_ttl_secs > 0 (leasing is what makes sweeps/reconcile safe
//! # across replicas; escape hatch for migration: ONEAI_ORCH_PG_NO_LEASE=1).
//! pg_dsn = "postgres://user:pass@127.0.0.1:5432/oneai"
//! lease_ttl_secs = 30
//! replica_id = ""   # empty → uuid v4 generated at boot
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
/// Default per-session lease TTL for multi-replica mode (MVS4-A); renewal
/// runs at ttl/3. Only meaningful on shared (Pg) stores — file mode is
/// single-replica and skips leasing entirely.
pub const DEFAULT_LEASE_TTL_SECS: u64 = 30;
/// Bearer secret env var for the frontend → orchestrator channel (D7).
pub const ORCHESTRATOR_SECRET_ENV: &str = "ONEAI_ORCHESTRATOR_SECRET";
/// Shared-Postgres DSN env (same selection rule as the persistence Pg
/// stores); wins over `OrchestratorConfig::pg_dsn`.
pub const PG_DSN_ENV: &str = "ONEAI_PG_DSN";
/// Escape hatch (`=1`): allow the Pg backend with leasing disabled —
/// migration/testing only; multi-replica safety is GONE without leases.
pub const PG_NO_LEASE_ENV: &str = "ONEAI_ORCH_PG_NO_LEASE";

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
    /// Seconds a *Hibernating* session (already `docker stop`ped by the idle
    /// sweep) stays untouched before its volumes are deep-archived to
    /// `archive_dir` and the container+volumes are destroyed (MVS3-C).
    /// `0` = disabled (the default — deep archive is opt-in, mirroring the
    /// `idle_timeout_secs == 0` semantics). Requires `archive_dir`.
    #[serde(default)]
    pub deep_archive_timeout_secs: u64,
    /// Root directory of the volume archive store
    /// (`archive::LocalDirArchiveStore`; point at an NFS/cloud-disk mount
    /// for real cold storage). Required when `deep_archive_timeout_secs > 0`.
    #[serde(default)]
    pub archive_dir: Option<PathBuf>,
    /// Shared Postgres DSN for the multi-replica routing table (MVS4-A).
    /// Env `ONEAI_PG_DSN` takes precedence (same selection rule as the six
    /// persistence Pg stores). Empty/None = file backend (single replica).
    #[serde(default)]
    pub pg_dsn: Option<String>,
    /// Per-session lease TTL for multi-replica mode (MVS4-A). Default 30;
    /// renewal at ttl/3. Must be > 0 when the shared (Pg) store is active —
    /// leasing is what makes sweeps/reconcile ownership-scoped there.
    #[serde(default = "default_lease_ttl")]
    pub lease_ttl_secs: u64,
    /// Replica identity for lease ownership. Empty (default) → a uuid v4 is
    /// generated at boot (leases die with the process anyway, so a stable
    /// id across restarts buys nothing — a fresh id avoids colliding with
    /// the previous incarnation's not-yet-expired leases).
    #[serde(default)]
    pub replica_id: String,
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
fn default_lease_ttl() -> u64 {
    DEFAULT_LEASE_TTL_SECS
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
            deep_archive_timeout_secs: 0,
            archive_dir: None,
            pg_dsn: None,
            lease_ttl_secs: default_lease_ttl(),
            replica_id: String::new(),
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
    // One flat positional list mirrors the CLI flags 1:1; a overrides
    // struct would just move the 8-field boilerplate to the call site.
    #[allow(clippy::too_many_arguments)]
    pub fn with_overrides(
        mut self,
        listen: Option<&str>,
        image: Option<&str>,
        registry_dir: Option<&str>,
        idle_timeout_secs: Option<u64>,
        deep_archive_timeout_secs: Option<u64>,
        archive_dir: Option<&str>,
        lease_ttl_secs: Option<u64>,
        replica_id: Option<&str>,
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
        if let Some(v) = deep_archive_timeout_secs {
            self.deep_archive_timeout_secs = v;
        }
        if let Some(v) = archive_dir {
            self.archive_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = lease_ttl_secs {
            self.lease_ttl_secs = v;
        }
        if let Some(v) = replica_id {
            self.replica_id = v.to_string();
        }
        self
    }

    /// Effective shared-store DSN (MVS4-A): env `ONEAI_PG_DSN` wins over
    /// `pg_dsn` — the same selection rule as the six persistence Pg stores.
    /// `None` = file backend (single replica).
    pub fn resolve_pg_dsn(&self) -> Option<String> {
        std::env::var(PG_DSN_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| self.pg_dsn.clone().filter(|s| !s.is_empty()))
    }

    /// Whether the no-lease escape hatch is set (`ONEAI_ORCH_PG_NO_LEASE=1`).
    pub fn pg_no_lease_escape() -> bool {
        std::env::var(PG_NO_LEASE_ENV)
            .ok()
            .is_some_and(|v| v == "1")
    }

    /// Cross-field validation. Called by `OrchestratorState::new` so every
    /// construction path (file, defaults, CLI overrides) is covered.
    pub fn validate(&self) -> crate::error::Result<()> {
        // MVS4-A: a shared Pg routing table WITHOUT leasing is a foot-gun —
        // sweeps and reconcile would race across replicas with no ownership
        // arbitration. Refuse to start (escape hatch for migration/tests).
        if self.resolve_pg_dsn().is_some()
            && self.lease_ttl_secs == 0
            && !Self::pg_no_lease_escape()
        {
            return Err(crate::error::OrchestratorError::Config(format!(
                "a shared Postgres routing table ({PG_DSN_ENV}/pg_dsn) requires \
                 lease_ttl_secs > 0 (multi-replica ownership); set --lease-ttl \
                 (default {DEFAULT_LEASE_TTL_SECS}s) or {PG_NO_LEASE_ENV}=1 to \
                 explicitly run lease-less (NOT multi-replica safe)"
            )));
        }
        if self.deep_archive_timeout_secs > 0 {
            if self.archive_dir.is_none() {
                return Err(crate::error::OrchestratorError::Config(
                    "deep_archive_timeout_secs > 0 requires archive_dir \
                     (the volume-archive store root, e.g. /srv/oneai-archive)"
                        .into(),
                ));
            }
            if self.idle_timeout_secs == 0 {
                return Err(crate::error::OrchestratorError::Config(
                    "deep_archive_timeout_secs > 0 requires idle_timeout_secs > 0 \
                     (deep archive is the SECOND hibernation tier — nothing ever \
                     reaches it when the idle sweep is disabled)"
                        .into(),
                ));
            }
        }
        Ok(())
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
        // Deep archive is opt-in: disabled + no dir by default, and the
        // default config passes validation.
        assert_eq!(c.deep_archive_timeout_secs, 0);
        assert!(c.archive_dir.is_none());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn deep_archive_validation() {
        // timeout > 0 without a dir → actionable config error.
        let c = OrchestratorConfig {
            deep_archive_timeout_secs: 60,
            ..OrchestratorConfig::default()
        };
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("archive_dir"), "{err}");
        // Both set → ok.
        let c = OrchestratorConfig {
            archive_dir: Some(PathBuf::from("/srv/oneai-archive")),
            ..c
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn toml_parse_deep_archive_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchestrator.toml");
        std::fs::write(
            &path,
            r#"
            deep_archive_timeout_secs = 3600
            archive_dir = "/srv/oneai-archive"
            "#,
        )
        .unwrap();
        let c = OrchestratorConfig::load_from(&path).unwrap();
        assert_eq!(c.deep_archive_timeout_secs, 3600);
        assert_eq!(c.archive_dir, Some(PathBuf::from("/srv/oneai-archive")));
        assert!(c.validate().is_ok());
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
            Some(90),
            Some("/tmp/arch"),
            Some(15),
            Some("rep-x"),
        );
        assert_eq!(c.listen, "127.0.0.1:1234");
        assert_eq!(c.image, "img:latest");
        assert_eq!(c.registry_dir, PathBuf::from("/tmp/reg"));
        assert_eq!(c.idle_timeout_secs, 5);
        assert_eq!(c.deep_archive_timeout_secs, 90);
        assert_eq!(c.archive_dir, Some(PathBuf::from("/tmp/arch")));
        assert_eq!(c.lease_ttl_secs, 15);
        assert_eq!(c.replica_id, "rep-x");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn pg_without_lease_rejected_unless_escape() {
        // Config-level DSN (env is process-global — keep this test on the
        // field so parallel tests can't race the environment).
        let c = OrchestratorConfig {
            pg_dsn: Some("postgres://u:p@127.0.0.1:5432/db".into()),
            lease_ttl_secs: 0,
            ..OrchestratorConfig::default()
        };
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("lease_ttl_secs"), "{err}");
        // ttl > 0 → ok.
        let ok = OrchestratorConfig {
            lease_ttl_secs: 30,
            ..c.clone()
        };
        assert!(ok.validate().is_ok());
        // Default (no DSN, ttl 30) stays valid.
        assert!(OrchestratorConfig::default().validate().is_ok());
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
