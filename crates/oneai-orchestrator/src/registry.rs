//! Session routing table: `session_id → (container, addr, state)`.
//!
//! Persistence mirrors `oneai-supervisor/src/registry.rs`: whole-file JSON,
//! atomic `write(tmp) → rename`. Reconcile on startup (D3): entries persisted
//! as Running/Resuming are health-probed — alive containers keep Running
//! (re-mounted), dead ones are marked `Crashed("orchestrator_restart")` and
//! resume lazily on the next request (D6).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::{OrchestratorError, Result};
use crate::fsm::{
    validate_transition, PersistedEntry, SessionEntry, SessionSnapshot, SessionState,
};
use crate::runner::{ContainerHandle, ContainerRunner};

/// Registry file name inside the registry dir.
pub const REGISTRY_FILE: &str = "sessions.json";

/// On-disk shape (versioned envelope like the supervisor registry).
#[derive(Debug, Serialize, Deserialize)]
struct RegistryFile {
    sessions: Vec<PersistedEntry>,
}

/// The routing table. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct RoutingTable {
    inner: Arc<RwLock<HashMap<String, Arc<SessionEntry>>>>,
    /// `<registry_dir>/sessions.json`
    path: PathBuf,
    /// Serializes whole-file persists: concurrent `write(tmp) → rename` on a
    /// SHARED tmp name races (one renamer steals the other's file → ENOENT).
    persist_lock: Arc<tokio::sync::Mutex<()>>,
}

impl RoutingTable {
    /// Empty table persisting to `<dir>/sessions.json`.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            path: dir.as_ref().join(REGISTRY_FILE),
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Registry file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert a brand-new entry. Err(AlreadyExists) if the id is taken.
    pub async fn insert_new(&self, entry: SessionEntry) -> Result<Arc<SessionEntry>> {
        let id = entry.spec.session_id.clone();
        {
            let mut map = self.inner.write().await;
            if map.contains_key(&id) {
                return Err(OrchestratorError::AlreadyExists(id));
            }
            let arc = Arc::new(entry);
            map.insert(id, arc.clone());
            Ok(arc)
        }
        // NOTE: insert_new does NOT persist — callers that need the entry
        // durable before the next state change (mid-spawn crash reconcile)
        // call `persist()` explicitly; `cas_transition` persists on every
        // successful transition anyway.
    }

    /// Get a session entry by id.
    pub async fn get(&self, id: &str) -> Option<Arc<SessionEntry>> {
        self.inner.read().await.get(id).cloned()
    }

    /// All sessions as JSON snapshots (sorted by id for stable output).
    pub async fn list(&self) -> Vec<SessionSnapshot> {
        let map = self.inner.read().await;
        let mut v: Vec<_> = map.values().map(|e| e.snapshot()).collect();
        v.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        v
    }

    /// CAS state transition: succeeds only if the current state equals
    /// `expected`. `new_handle`/`last_error` are merged when `Some`.
    /// On success: replaces the Arc, persists (outside the write lock), and
    /// signals `ready_notify` waiters when the new state is Running.
    ///
    /// Returns `Ok(None)` on a CAS miss (caller re-reads and retries or
    /// gives up); `Err` only on an illegal transition.
    pub async fn cas_transition(
        &self,
        id: &str,
        expected: SessionState,
        to: SessionState,
        new_handle: Option<ContainerHandle>,
        last_error: Option<String>,
    ) -> Result<Option<Arc<SessionEntry>>> {
        let arc = {
            let mut map = self.inner.write().await;
            let Some(entry) = map.get(id) else {
                return Ok(None);
            };
            if entry.state != expected {
                return Ok(None); // CAS miss
            }
            validate_transition(expected, to)?;
            let mut next = (**entry).clone();
            next.state = to;
            if new_handle.is_some() {
                next.handle = new_handle;
            }
            next.last_error = last_error;
            next.updated_at = chrono::Utc::now();
            let arc = Arc::new(next);
            map.insert(id.to_string(), arc.clone());
            arc
        };
        // Persist outside the lock (slow fsync must not block lookups).
        if let Err(e) = self.persist().await {
            tracing::error!(session = %id, error = %e, "routing table persist failed");
        }
        if to == SessionState::Running {
            arc.ready_notify.notify_waiters();
        }
        Ok(Some(arc))
    }

    /// Remove an entry entirely (post-Destroyed tombstone cleanup) and
    /// persist.
    pub async fn remove(&self, id: &str) -> Result<()> {
        {
            let mut map = self.inner.write().await;
            map.remove(id);
        }
        self.persist().await
    }

    /// Persist the whole table atomically: write `<file>.tmp` → rename.
    /// Parent dir is created on demand. Unix: best-effort chmod 600 (the
    /// file may contain injected env secrets — MVS2 limitation, D5 notes
    /// Secret Manager for production).
    pub async fn persist(&self) -> Result<()> {
        let sessions: Vec<PersistedEntry> = {
            let map = self.inner.read().await;
            let mut v: Vec<_> = map.values().map(|e| e.to_persisted()).collect();
            v.sort_by(|a, b| a.spec.session_id.cmp(&b.spec.session_id));
            v
        };
        let file = RegistryFile { sessions };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|e| OrchestratorError::Persist(e.to_string()))?;
        // Serialize write+rename across concurrent transitions (a shared tmp
        // name races: renamer A steals renamer B's file → ENOENT).
        let _guard = self.persist_lock.lock().await;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = self
            .path
            .with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
        tokio::fs::write(&tmp, &json).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await;
        }
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }

    /// Load the table from disk (missing file = empty table; malformed file
    /// is an error — better to refuse than silently drop live sessions).
    pub async fn load(dir: impl AsRef<Path>) -> Result<Self> {
        let table = Self::new(dir);
        if !table.path.exists() {
            return Ok(table);
        }
        let bytes = tokio::fs::read(table.path()).await?;
        let file: RegistryFile = serde_json::from_slice(&bytes)
            .map_err(|e| OrchestratorError::Persist(format!("{}: {e}", table.path.display())))?;
        {
            let mut map = table.inner.write().await;
            for p in file.sessions {
                let entry = SessionEntry::from_persisted(p);
                map.insert(entry.spec.session_id.clone(), Arc::new(entry));
            }
        }
        Ok(table)
    }

    /// Load + startup reconcile (D3/R3): for every entry persisted as
    /// Running/Resuming/Creating, probe the container. Alive → force to
    /// Running (re-mounted). Dead/gone → Crashed("orchestrator_restart").
    /// Hibernating entries stay as-is (their container is stopped by
    /// design). Failed/Destroyed are kept for API visibility.
    pub async fn load_and_reconcile(
        dir: impl AsRef<Path>,
        runner: &dyn ContainerRunner,
    ) -> Result<Self> {
        let table = Self::load(dir).await?;
        let candidates: Vec<(String, Arc<SessionEntry>)> = {
            let map = table.inner.read().await;
            map.iter()
                .filter(|(_, e)| {
                    matches!(
                        e.state,
                        SessionState::Running | SessionState::Resuming | SessionState::Creating
                    )
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        for (id, entry) in candidates {
            let alive = match &entry.handle {
                Some(h) => runner.health(h).await.unwrap_or(false),
                None => false,
            };
            let from = entry.state;
            let mut next = table.inner.write().await;
            if let Some(cur) = next.get(&id) {
                if cur.state != from {
                    continue; // moved underneath us; leave it
                }
                let mut e2 = (**cur).clone();
                if alive {
                    e2.state = SessionState::Running;
                } else {
                    e2.state = SessionState::Crashed;
                    e2.last_error = Some("orchestrator_restart".into());
                }
                e2.updated_at = chrono::Utc::now();
                let arc = Arc::new(e2);
                next.insert(id.clone(), arc.clone());
                drop(next);
                if alive {
                    arc.ready_notify.notify_waiters();
                }
                tracing::info!(
                    session = %id,
                    from = ?from,
                    alive,
                    "reconciled after orchestrator restart"
                );
            }
        }
        table.persist().await?;
        Ok(table)
    }

    /// Idle-hibernation candidates: Running, no attached WS connections, and
    /// idle for longer than `timeout_ms`. Advisory — the caller must still
    /// CAS (state may move between here and there).
    pub async fn list_idle_candidates(&self, timeout_ms: u64) -> Vec<String> {
        let map = self.inner.read().await;
        map.values()
            .filter(|e| {
                e.state == SessionState::Running
                    && e.active_conns.load(std::sync::atomic::Ordering::Relaxed) == 0
                    && e.idle_ms() > timeout_ms
            })
            .map(|e| e.spec.session_id.clone())
            .collect()
    }

    /// Number of sessions in the table.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Whether the table is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runner::SessionSpec;
    use chrono::Utc;

    pub(crate) fn test_spec(id: &str) -> SessionSpec {
        SessionSpec {
            session_id: id.into(),
            image: "img".into(),
            state_volume: format!("oneai-orch-{id}-state"),
            workspace_volume: format!("oneai-orch-{id}-ws"),
            env: vec![],
            bind_host: "127.0.0.1".into(),
            container_port: 8787,
            provider_config: None,
            created_at: Utc::now(),
        }
    }

    fn handle(port: u16) -> ContainerHandle {
        ContainerHandle {
            container_id: "cid".into(),
            container_name: "oneai-orch-s1".into(),
            host_port: port,
        }
    }

    #[tokio::test]
    async fn insert_and_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        t.insert_new(SessionEntry::new_creating(test_spec("s1")))
            .await
            .unwrap();
        let dup = t
            .insert_new(SessionEntry::new_creating(test_spec("s1")))
            .await;
        assert!(matches!(dup, Err(OrchestratorError::AlreadyExists(_))));
        assert_eq!(t.len().await, 1);
    }

    #[tokio::test]
    async fn cas_hit_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        t.insert_new(SessionEntry::new_creating(test_spec("s1")))
            .await
            .unwrap();
        // Hit: Creating → Running with handle.
        let r = t
            .cas_transition(
                "s1",
                SessionState::Creating,
                SessionState::Running,
                Some(handle(40001)),
                None,
            )
            .await
            .unwrap();
        assert!(r.is_some());
        let e = t.get("s1").await.unwrap();
        assert_eq!(e.state, SessionState::Running);
        assert_eq!(e.handle.as_ref().unwrap().host_port, 40001);
        // Miss: expected state no longer matches.
        let r = t
            .cas_transition(
                "s1",
                SessionState::Creating,
                SessionState::Failed,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(r.is_none());
        // Unknown id → None.
        let r = t
            .cas_transition(
                "nope",
                SessionState::Running,
                SessionState::Crashed,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn cas_illegal_transition_errors() {
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        t.insert_new(SessionEntry::new_creating(test_spec("s1")))
            .await
            .unwrap();
        let r = t
            .cas_transition(
                "s1",
                SessionState::Creating,
                SessionState::Hibernating,
                None,
                None,
            )
            .await;
        assert!(matches!(
            r,
            Err(OrchestratorError::IllegalTransition { .. })
        ));
    }

    #[tokio::test]
    async fn cas_concurrent_exactly_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        t.insert_new(SessionEntry::new_creating(test_spec("s1")))
            .await
            .unwrap();
        let mut joins = Vec::new();
        for _ in 0..10 {
            let t = t.clone();
            joins.push(tokio::spawn(async move {
                t.cas_transition(
                    "s1",
                    SessionState::Creating,
                    SessionState::Running,
                    Some(handle(1)),
                    None,
                )
                .await
                .unwrap()
                .is_some()
            }));
        }
        let wins = futures::future::join_all(joins)
            .await
            .into_iter()
            .filter(|r| *r.as_ref().unwrap())
            .count();
        assert_eq!(wins, 1);
    }

    #[tokio::test]
    async fn persist_reload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        t.insert_new(SessionEntry::new_creating(test_spec("a")))
            .await
            .unwrap();
        t.insert_new(SessionEntry::new_creating(test_spec("b")))
            .await
            .unwrap();
        t.cas_transition(
            "a",
            SessionState::Creating,
            SessionState::Running,
            Some(handle(41234)),
            None,
        )
        .await
        .unwrap();
        let t2 = RoutingTable::load(dir.path()).await.unwrap();
        assert_eq!(t2.len().await, 2);
        let a = t2.get("a").await.unwrap();
        assert_eq!(a.state, SessionState::Running);
        assert_eq!(a.handle.as_ref().unwrap().host_port, 41234);
        assert_eq!(t2.get("b").await.unwrap().state, SessionState::Creating);
        // No leftover .tmp file.
        assert!(!dir.path().join("sessions.json.tmp").exists());
    }

    #[tokio::test]
    async fn concurrent_insert_and_persist_no_tmp_race() {
        // Regression: concurrent persists used to share one tmp file name —
        // renamer A stole renamer B's file → ENOENT (found by the MVS2
        // real-docker acceptance run).
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        let mut joins = Vec::new();
        for i in 0..10 {
            let t = t.clone();
            joins.push(tokio::spawn(async move {
                let id = format!("c{i}");
                t.insert_new(SessionEntry::new_creating(test_spec(&id)))
                    .await
                    .unwrap();
                t.persist().await.unwrap();
            }));
        }
        for j in joins {
            j.await.unwrap();
        }
        assert_eq!(t.len().await, 10);
        let t2 = RoutingTable::load(dir.path()).await.unwrap();
        assert_eq!(t2.len().await, 10);
    }

    #[tokio::test]
    async fn load_malformed_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(REGISTRY_FILE), "{oops").unwrap();
        assert!(RoutingTable::load(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn remove_clears_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        t.insert_new(SessionEntry::new_creating(test_spec("s1")))
            .await
            .unwrap();
        t.remove("s1").await.unwrap();
        assert!(t.get("s1").await.is_none());
        let t2 = RoutingTable::load(dir.path()).await.unwrap();
        assert_eq!(t2.len().await, 0);
    }

    #[tokio::test]
    async fn idle_candidates_respect_conns_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        t.insert_new(SessionEntry::new_creating(test_spec("idle")))
            .await
            .unwrap();
        t.insert_new(SessionEntry::new_creating(test_spec("busy")))
            .await
            .unwrap();
        for id in ["idle", "busy"] {
            t.cas_transition(
                id,
                SessionState::Creating,
                SessionState::Running,
                None,
                None,
            )
            .await
            .unwrap();
        }
        // busy has an attached connection.
        t.get("busy")
            .await
            .unwrap()
            .active_conns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Both are "idle" for 0ms timeout, but busy is vetoed.
        let c = t.list_idle_candidates(0).await;
        assert_eq!(c, vec!["idle".to_string()]);
        // Huge timeout → nobody.
        assert!(t.list_idle_candidates(u64::MAX).await.is_empty());
    }
}
