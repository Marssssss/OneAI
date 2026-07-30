//! Persisted instance registry — the durable record of supervised instances.
//!
//! The registry survives supervisor restarts as `instances.json` under a root
//! dir (default `~/.oneai/server/`). A reconnecting native-app client reads it
//! to learn which instances existed and which need re-spawning. The live
//! `AgentLoop` handle is **not** persisted (it lives in the in-proc
//! `Supervisor`); `recover_after_restart()` reconciles the two: any instance
//! persisted as `Running` has lost its live handle, so it's marked
//! `Crashed("supervisor_restart")`.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::Result;
use crate::runner::TurnSummary;

/// Specification of a supervised instance — supplied by the spawner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceSpec {
    /// Caller-chosen unique id.
    pub id: String,
    /// Domain pack name (e.g. `coding`).
    pub domain: String,
    /// Model override (optional; falls back to the daemon's default).
    pub model: Option<String>,
    /// User id for memory / persistence namespacing.
    pub user: Option<String>,
    /// When the instance was first registered.
    pub created_at: DateTime<Utc>,
}

/// Lifecycle status of a supervised instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum InstanceStatus {
    /// Registered but no turn in flight.
    Idle,
    /// A turn is currently running.
    Running,
    /// A stop was requested; waiting for the in-flight turn to abort.
    Stopping,
    /// Explicitly stopped (not restarted).
    Stopped,
    /// The live handle is gone but the durable record survives. The string is
    /// the reason (e.g. `supervisor_restart`, `crashed`).
    Crashed(String),
}

/// A durable snapshot of one instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub spec: InstanceSpec,
    pub status: InstanceStatus,
    pub updated_at: DateTime<Utc>,
    /// The last completed turn's summary, if any.
    pub last_turn: Option<TurnSummary>,
}

/// The on-disk registry file shape.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    instances: Vec<InstanceInfo>,
}

/// A persisted instance registry.
pub struct InstanceRegistry {
    root: PathBuf,
    inner: RwLock<HashMap<String, InstanceInfo>>,
}

impl InstanceRegistry {
    /// Create or load a registry rooted at `root` (the `instances.json` file
    /// lives directly under it). Existing entries are loaded.
    pub async fn new(root: PathBuf) -> Result<Self> {
        let inner = RwLock::new(HashMap::new());
        let reg = Self { root, inner };
        reg.load_into().await?;
        Ok(reg)
    }

    fn path(&self) -> PathBuf {
        self.root.join("instances.json")
    }

    fn tmp_path(&self) -> PathBuf {
        self.root.join("instances.json.tmp")
    }

    async fn load_into(&self) -> Result<()> {
        let path = self.path();
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => {
                let file: RegistryFile = serde_json::from_str(&s)?;
                let mut map = self.inner.write().await;
                for info in file.instances {
                    map.insert(info.spec.id.clone(), info);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    async fn persist(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.root).await?;
        let snapshot: Vec<InstanceInfo> = self.inner.read().await.values().cloned().collect();
        let file = RegistryFile {
            instances: snapshot,
        };
        let json = serde_json::to_vec_pretty(&file)?;
        let tmp = self.tmp_path();
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, self.path()).await?;
        Ok(())
    }

    /// Register a new instance. Fails if the id already exists.
    pub async fn register(&self, spec: InstanceSpec, status: InstanceStatus) -> Result<()> {
        let mut map = self.inner.write().await;
        if map.contains_key(&spec.id) {
            return Err(crate::error::SupervisorError::InstanceExists(
                spec.id.clone(),
            ));
        }
        let now = Utc::now();
        map.insert(
            spec.id.clone(),
            InstanceInfo {
                spec,
                status,
                updated_at: now,
                last_turn: None,
            },
        );
        drop(map);
        self.persist().await
    }

    /// Remove an instance from the registry.
    pub async fn unregister(&self, id: &str) -> Result<()> {
        let mut map = self.inner.write().await;
        let existed = map.remove(id).is_some();
        drop(map);
        if existed {
            self.persist().await?;
        }
        Ok(())
    }

    /// Snapshot of all instances.
    pub async fn list(&self) -> Vec<InstanceInfo> {
        self.inner.read().await.values().cloned().collect()
    }

    /// Get one instance.
    pub async fn get(&self, id: &str) -> Option<InstanceInfo> {
        self.inner.read().await.get(id).cloned()
    }

    /// Update an instance's status. No-op (not an error) if missing.
    pub async fn set_status(&self, id: &str, status: InstanceStatus) -> Result<()> {
        {
            let mut map = self.inner.write().await;
            if let Some(info) = map.get_mut(id) {
                info.status = status;
                info.updated_at = Utc::now();
            } else {
                return Ok(());
            }
        }
        self.persist().await
    }

    /// Record the last completed turn for an instance.
    pub async fn set_last_turn(&self, id: &str, turn: TurnSummary) -> Result<()> {
        {
            let mut map = self.inner.write().await;
            if let Some(info) = map.get_mut(id) {
                info.last_turn = Some(turn);
                info.updated_at = Utc::now();
            } else {
                return Ok(());
            }
        }
        self.persist().await
    }

    /// Reconcile after a supervisor restart: every `Running` instance has lost
    /// its live handle, so mark it `Crashed("supervisor_restart")` and persist.
    pub async fn recover_after_restart(&self) -> Result<()> {
        {
            let mut map = self.inner.write().await;
            for info in map.values_mut() {
                if matches!(info.status, InstanceStatus::Running) {
                    info.status = InstanceStatus::Crashed("supervisor_restart".to_string());
                    info.updated_at = Utc::now();
                }
            }
        }
        self.persist().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> InstanceSpec {
        InstanceSpec {
            id: id.to_string(),
            domain: "coding".to_string(),
            model: None,
            user: None,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        {
            let reg = InstanceRegistry::new(root.clone()).await.unwrap();
            reg.register(spec("a"), InstanceStatus::Idle).await.unwrap();
            reg.register(spec("b"), InstanceStatus::Idle).await.unwrap();
            assert_eq!(reg.list().await.len(), 2);
        }
        // Reload from disk — entries survive.
        let reg = InstanceRegistry::new(root).await.unwrap();
        let ids: Vec<String> = reg.list().await.iter().map(|i| i.spec.id.clone()).collect();
        assert!(ids.contains(&"a".to_string()));
        assert!(ids.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn recover_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = InstanceRegistry::new(root.clone()).await.unwrap();
        reg.register(spec("a"), InstanceStatus::Running)
            .await
            .unwrap();
        reg.register(spec("b"), InstanceStatus::Idle).await.unwrap();

        reg.recover_after_restart().await.unwrap();

        let a = reg.get("a").await.unwrap();
        assert!(matches!(
            a.status,
            InstanceStatus::Crashed(ref r) if r == "supervisor_restart"
        ));
        let b = reg.get("b").await.unwrap();
        assert!(matches!(b.status, InstanceStatus::Idle));

        // Recovered state persists across reload.
        let reg = InstanceRegistry::new(root).await.unwrap();
        let a = reg.get("a").await.unwrap();
        assert!(matches!(a.status, InstanceStatus::Crashed(_)));
    }

    #[tokio::test]
    async fn atomic_write_valid() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let reg = InstanceRegistry::new(root.clone()).await.unwrap();
        reg.register(spec("a"), InstanceStatus::Running)
            .await
            .unwrap();
        // After every write the file is valid JSON and no tmp file lingers.
        reg.set_status("a", InstanceStatus::Idle).await.unwrap();
        reg.set_status("a", InstanceStatus::Stopped).await.unwrap();
        let s = tokio::fs::read_to_string(root.join("instances.json"))
            .await
            .unwrap();
        assert!(serde_json::from_str::<RegistryFile>(&s).is_ok());
        assert!(!tokio::fs::try_exists(root.join("instances.json.tmp"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn duplicate_register_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let reg = InstanceRegistry::new(dir.path().to_path_buf())
            .await
            .unwrap();
        reg.register(spec("a"), InstanceStatus::Idle).await.unwrap();
        let err = reg.register(spec("a"), InstanceStatus::Idle).await;
        assert!(err.is_err());
    }
}
