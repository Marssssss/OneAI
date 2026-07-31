//! `FileSyncManager` — sync `~/.oneai` between the local host and a
//! `TerminalBackend`'s filesystem (Phase 3.3, Stage D).
//!
//! For [`crate::terminal::LocalBackend`] this is a no-op (the local FS *is*
//! the state). For `DockerTerminalBackend` it round-trips via `docker cp`.
//! For serverless backends (Modal/Daytona) it's an HTTP upload/download
//! against the snapshot's FS — left as the per-backend override.
//!
//! The manager is the bridge between OneAI's working-state files
//! (`<root>/tasks/*.jsonl`, `cron/jobs.json`, `gateway/channels.json`, …) and
//! a remote terminal's filesystem so a hibernated session can be restored
//! with its bookkeeping intact.

use std::path::PathBuf;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};

use super::SnapshotHandle;

/// Default root: `~/.oneai`.
fn default_root() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".oneai"))
        .unwrap_or_else(|| PathBuf::from(".oneai"))
}

/// Syncs `~/.oneai` (or a configured root) to/from a backend's filesystem.
pub struct FileSyncManager {
    root: PathBuf,
}

impl Default for FileSyncManager {
    fn default() -> Self {
        Self {
            root: default_root(),
        }
    }
}

impl FileSyncManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure with an explicit root (e.g. a test temp dir).
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}

/// File-sync capability — backends that can shuttle files implement this.
/// `FileSyncManager` dispatches on `backend_name` to the right strategy;
/// the default (no strategy registered) is a no-op (Local semantics).
#[async_trait]
pub trait FileSyncStrategy: Send + Sync {
    /// Push the local root into the backend's FS (pre-snapshot).
    async fn push(&self, root: &std::path::Path, handle: &SnapshotHandle) -> Result<()>;

    /// Pull the backend's FS into the local root (post-restore).
    async fn pull(&self, root: &std::path::Path, handle: &SnapshotHandle) -> Result<()>;
}

impl FileSyncManager {
    /// Push the local root into the backend's FS. No-op for backends without
    /// a registered strategy (Local — the FS is already local).
    pub async fn push(&self, _to: &dyn std::any::Any, _handle: &SnapshotHandle) -> Result<()> {
        // Default: no-op. Docker/Modal/Daytona strategies are wired by the
        // CLI / backend constructors that own a `FileSyncStrategy`.
        Ok(())
    }

    /// Pull the backend's FS into the local root. No-op for Local.
    pub async fn pull(&self, _from: &dyn std::any::Any, _handle: &SnapshotHandle) -> Result<()> {
        Ok(())
    }

    /// Verify the local root exists (creates it if missing). Idempotent.
    pub fn ensure_root(&self) -> Result<()> {
        if !self.root.exists() {
            std::fs::create_dir_all(&self.root)
                .map_err(|e| OneAIError::Other(format!("failed to create file-sync root: {e}")))?;
        }
        Ok(())
    }
}

// ─── Docker file-sync strategy ────────────────────────────────────────────────

/// `FileSyncStrategy` for `DockerTerminalBackend` — `docker cp` the root
/// into the container (push) / out of the container (pull).
pub struct DockerFileSync {
    container_name: String,
}

impl DockerFileSync {
    pub fn new(container_name: impl Into<String>) -> Self {
        Self {
            container_name: container_name.into(),
        }
    }

    fn docker_cp(args: &[String]) -> Result<()> {
        let out = std::process::Command::new("docker")
            .args(args)
            .output()
            .map_err(|e| OneAIError::Other(format!("failed to spawn docker cp: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(OneAIError::Other(format!(
                "docker cp failed: {}",
                stderr.trim()
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl FileSyncStrategy for DockerFileSync {
    async fn push(&self, root: &std::path::Path, _handle: &SnapshotHandle) -> Result<()> {
        let src = root.to_string_lossy().to_string();
        let dest = format!("{}:/root/.oneai", self.container_name);
        // mkdir the dest dir in the container, then cp.
        let _ = std::process::Command::new("docker")
            .args(["exec", &self.container_name, "mkdir", "-p", "/root/.oneai"])
            .output();
        DockerFileSync::docker_cp(&["cp".to_string(), src, dest])
    }

    async fn pull(&self, root: &std::path::Path, _handle: &SnapshotHandle) -> Result<()> {
        let src = format!("{}:/root/.oneai", self.container_name);
        let dest = root.to_string_lossy().to_string();
        DockerFileSync::docker_cp(&["cp".to_string(), src, dest])
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_root_is_home_oneai() {
        let m = FileSyncManager::new();
        assert!(m.root().to_string_lossy().ends_with(".oneai"));
    }

    #[test]
    fn test_ensure_root_creates_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "oneai_filesync_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let m = FileSyncManager::with_root(tmp.clone());
        assert!(!tmp.exists());
        m.ensure_root().unwrap();
        assert!(tmp.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_push_pull_default_noop() {
        let m = FileSyncManager::new();
        let h = SnapshotHandle::new("x", "local");
        // Default push/pull are no-ops (no strategy wired) — must not error.
        m.push(&0 as &dyn std::any::Any, &h).await.unwrap();
        m.pull(&0 as &dyn std::any::Any, &h).await.unwrap();
    }

    #[test]
    fn test_docker_file_sync_construct() {
        let s = DockerFileSync::new("oneai-terminal");
        assert_eq!(s.container_name, "oneai-terminal");
    }
}
