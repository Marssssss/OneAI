//! Session routing table: `session_id → (container, addr, state)`.
//!
//! MVS4-A: the *durable* half lives behind [`SessionStore`] (`store.rs`) —
//! whole-file JSON by default (`FileSessionStore`, byte-compatible with the
//! MVS2 `sessions.json`), shared Postgres in multi-replica mode
//! (`PgSessionStore`). What remains here is the per-replica hot cache: the
//! process-local atomics (`active_conns`, `last_activity_ms`,
//! `ready_notify`) that the WS proxy bumps per frame without any IO, plus
//! the merge logic that keeps cache entries aligned with fresh store reads.
//!
//! Reconcile on startup (D3): entries persisted as Running/Resuming are
//! health-probed — alive containers keep Running (re-mounted), dead ones are
//! marked `Crashed("orchestrator_restart")` and resume lazily on the next
//! request (D6). Multi-replica reconcile is lease-gated (MVS4-A): a replica
//! only probes/takes over sessions whose lease is expired or self-held.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::archive::ArchiveManifest;
use crate::error::Result;
use crate::fsm::{SessionEntry, SessionSnapshot, SessionState};
use crate::runner::{ContainerHandle, ContainerRunner};
use crate::store::{ClaimOutcome, FileSessionStore, LeaseIdentity, SessionStore, StoredSession};

pub use crate::store::REGISTRY_FILE;

/// Minimum gap between two durable activity flushes for the same session
/// (the per-frame `touch()` stays a local atomic; only the flush hits the
/// store). 1s granularity keeps a cross-replica idle sweep's view of
/// `last_activity_ms` fresh enough for timeouts measured in minutes.
pub const ACTIVITY_FLUSH_MS: u64 = 1000;

/// The routing table. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct RoutingTable {
    /// Per-replica hot cache: process-local atomics + ready-notify hub,
    /// refreshed from the store on every durable read/write.
    inner: Arc<RwLock<HashMap<String, Arc<SessionEntry>>>>,
    store: Arc<dyn SessionStore>,
}

/// Merge one stored row into the cache.
///
/// The Arc is only clone-and-replaced when the DURABLE fields actually
/// differ from the cached view. An unchanged row returns the very same Arc —
/// critical because live handles to the entry (`ConnGuard` inside a running
/// proxy, a sweep's veto read) mutate its atomics in place; replacing the
/// Arc on every read would orphan those handles and leak `active_conns`.
///
/// Durable activity is merged monotonically *in place* (fetch_max): another
/// replica's newer flush wins over a stale local clock, never the other way
/// around, and no Arc replacement is needed for it.
fn merge_stored(
    map: &mut HashMap<String, Arc<SessionEntry>>,
    s: &StoredSession,
) -> Arc<SessionEntry> {
    let id = &s.entry.spec.session_id;
    if let Some(existing) = map.get(id) {
        // Cross-replica activity: bump the shared atomics in place. Also
        // raise the flush watermark so our own flusher never writes an
        // older value back over a foreign newer one.
        if s.last_activity_ms > existing.last_activity_ms.load(Ordering::Relaxed) {
            existing
                .last_activity_ms
                .fetch_max(s.last_activity_ms, Ordering::Relaxed);
            existing
                .last_flushed_activity_ms
                .fetch_max(s.last_activity_ms, Ordering::Relaxed);
        }
        let unchanged = existing.state == s.entry.state
            && existing.handle == s.entry.handle
            && existing.last_error == s.entry.last_error
            && existing.updated_at == s.entry.updated_at
            && existing.archived == s.entry.archived;
        if unchanged {
            return existing.clone();
        }
        let mut next = (**existing).clone();
        next.handle = s.entry.handle.clone();
        next.state = s.entry.state;
        next.last_error = s.entry.last_error.clone();
        next.updated_at = s.entry.updated_at;
        next.archived = s.entry.archived.clone();
        let arc = Arc::new(next);
        map.insert(id.clone(), arc.clone());
        return arc;
    }
    let e = SessionEntry::from_persisted(s.entry.clone());
    if s.last_activity_ms > 0 {
        // Backends that track durable activity (Pg): seed the idle clock
        // with the truth instead of "now" (a fresh cache entry must not
        // look freshly-active). File mode stores 0 → the from_persisted
        // "clock starts now" restart grace applies.
        e.last_activity_ms
            .store(s.last_activity_ms, Ordering::Relaxed);
        e.last_flushed_activity_ms
            .store(s.last_activity_ms, Ordering::Relaxed);
    }
    let arc = Arc::new(e);
    map.insert(id.clone(), arc.clone());
    arc
}

impl RoutingTable {
    /// Empty table backed by the file store at `<dir>/sessions.json`
    /// (MVS2 default; dir created on the first write).
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self::with_store(Arc::new(FileSessionStore::new(dir)))
    }

    /// Table backed by an explicit store (the MVS4-A injection point for
    /// `PgSessionStore` / test fakes).
    pub fn with_store(store: Arc<dyn SessionStore>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            store,
        }
    }

    /// The durable store behind this table (lease ops, backend capability
    /// probes).
    pub fn store(&self) -> Arc<dyn SessionStore> {
        self.store.clone()
    }

    /// Backing file path (file backend only; `None` for shared stores).
    pub fn path(&self) -> Option<&Path> {
        self.store.store_path()
    }

    /// Insert a brand-new entry. Err(AlreadyExists) if the id is taken —
    /// on shared backends the primary key arbitrates ACROSS replicas.
    /// Durable immediately (mid-spawn crash reconcile depends on it).
    pub async fn insert_new(&self, entry: SessionEntry) -> Result<Arc<SessionEntry>> {
        let id = entry.spec.session_id.clone();
        self.store.insert(&entry.to_persisted()).await?;
        let arc = Arc::new(entry);
        self.inner.write().await.insert(id, arc.clone());
        Ok(arc)
    }

    /// Get a session entry by id (fresh store read merged into the cache).
    /// On a store outage the stale cache entry is served (logged loudly) so
    /// in-flight proxies and control-plane reads degrade instead of failing.
    pub async fn get(&self, id: &str) -> Option<Arc<SessionEntry>> {
        match self.store.get(id).await {
            Ok(Some(s)) => {
                let mut map = self.inner.write().await;
                Some(merge_stored(&mut map, &s))
            }
            Ok(None) => {
                // Gone from the shared truth (e.g. destroyed by another
                // replica) — drop the local shadow.
                self.inner.write().await.remove(id);
                None
            }
            Err(e) => {
                tracing::error!(
                    session = %id, error = %e,
                    "session store read failed; serving cached entry (may be stale)"
                );
                self.inner.read().await.get(id).cloned()
            }
        }
    }

    /// Refresh the whole cache from the store; returns the fresh rows.
    async fn refresh_cache(&self) -> Result<Vec<StoredSession>> {
        let all = self.store.load_all().await?;
        let mut map = self.inner.write().await;
        let live: HashSet<&str> = all
            .iter()
            .map(|s| s.entry.spec.session_id.as_str())
            .collect();
        map.retain(|id, _| live.contains(id.as_str()));
        for s in &all {
            merge_stored(&mut map, s);
        }
        Ok(all)
    }

    /// All sessions as JSON snapshots (sorted by id for stable output),
    /// refreshed from the store so replicas converge on the shared truth.
    /// On a store outage the cached view is served (logged loudly).
    pub async fn list(&self) -> Vec<SessionSnapshot> {
        if let Err(e) = self.refresh_cache().await {
            tracing::error!(error = %e, "session store list failed; serving cached snapshots");
        }
        let map = self.inner.read().await;
        let mut v: Vec<_> = map.values().map(|e| e.snapshot()).collect();
        v.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        v
    }

    /// CAS state transition: succeeds only if the current state equals
    /// `expected` (arbitrated by the STORE — cross-replica safe).
    /// `new_handle`/`last_error` are merged when `Some`. On success the
    /// cache is re-merged and `ready_notify` waiters are signalled when the
    /// new state is Running.
    ///
    /// Returns `Ok(None)` on a CAS miss (caller re-reads and retries or
    /// gives up); `Err` only on an illegal transition or store failure.
    pub async fn cas_transition(
        &self,
        id: &str,
        expected: SessionState,
        to: SessionState,
        new_handle: Option<ContainerHandle>,
        last_error: Option<String>,
    ) -> Result<Option<Arc<SessionEntry>>> {
        let updated = self
            .store
            .cas_transition(id, expected, to, new_handle, last_error)
            .await?;
        let Some(p) = updated else {
            return Ok(None);
        };
        let arc = {
            let mut map = self.inner.write().await;
            merge_stored(&mut map, &StoredSession::new(p))
        };
        if to == SessionState::Running {
            arc.ready_notify.notify_waiters();
        }
        Ok(Some(arc))
    }

    /// Remove an entry entirely (post-Destroyed tombstone cleanup).
    pub async fn remove(&self, id: &str) -> Result<()> {
        self.store.remove(id).await?;
        self.inner.write().await.remove(id);
        Ok(())
    }

    /// Durability barrier (file backend: whole-file atomic write; shared
    /// backends: no-op — every mutation is already durable).
    pub async fn persist(&self) -> Result<()> {
        self.store.flush().await
    }

    /// Durable activity flush for one session, throttled to
    /// [`ACTIVITY_FLUSH_MS`] granularity unless `force` (proxy teardown /
    /// lease release must flush so a foreign idle sweep never sees stale
    /// activity). The per-frame `entry.touch()` remains a local atomic.
    pub async fn flush_activity(&self, id: &str, force: bool) -> Result<()> {
        let (val, prev_flushed) = {
            let map = self.inner.read().await;
            let Some(e) = map.get(id) else {
                return Ok(());
            };
            (
                e.last_activity_ms.load(Ordering::Relaxed),
                e.last_flushed_activity_ms.load(Ordering::Relaxed),
            )
        };
        if val <= prev_flushed || (!force && val - prev_flushed < ACTIVITY_FLUSH_MS) {
            return Ok(());
        }
        self.store.touch_activity(id, val).await?;
        if let Some(e) = self.inner.read().await.get(id) {
            e.last_flushed_activity_ms.fetch_max(val, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Load the table from the file backend (missing file = empty table;
    /// malformed file is an error — better to refuse than silently drop live
    /// sessions).
    pub async fn load(dir: impl AsRef<Path>) -> Result<Self> {
        let store = Arc::new(FileSessionStore::open(dir).await?);
        Self::load_with_store(store).await
    }

    /// Load the cache from an explicit store.
    pub async fn load_with_store(store: Arc<dyn SessionStore>) -> Result<Self> {
        let table = Self::with_store(store);
        table.refresh_cache().await?;
        Ok(table)
    }

    /// Load + startup reconcile (D3/R3) on the file backend (single
    /// replica — no lease gating).
    pub async fn load_and_reconcile(
        dir: impl AsRef<Path>,
        runner: &dyn ContainerRunner,
    ) -> Result<Self> {
        let store = Arc::new(FileSessionStore::open(dir).await?);
        Self::reconcile_with_store(store, runner, None).await
    }

    /// Load + startup reconcile against an explicit store. For every entry
    /// persisted as Running/Resuming/Creating, probe the container. Alive →
    /// force to Running (re-mounted). Dead/gone → Crashed("orchestrator_restart").
    /// Hibernating entries stay as-is (their container is stopped by
    /// design). Failed/Destroyed are kept for API visibility.
    ///
    /// MVS4-A multi-replica gating: when `lease` is present (and the store
    /// leases), each candidate must be lease-CLAIMED before it is probed —
    /// the claim is the cross-replica mutex, so two replicas reconciling
    /// simultaneously never double-probe or fight over the same entry:
    /// a live foreign lease → `HeldByOther` → skipped (its owner's own
    /// reconcile/sweeps handle it); an expired/unowned lease → claimed and
    /// reconciled here. Dead entries release the claim again so any replica
    /// can own the eventual resume.
    pub async fn reconcile_with_store(
        store: Arc<dyn SessionStore>,
        runner: &dyn ContainerRunner,
        lease: Option<&LeaseIdentity>,
    ) -> Result<Self> {
        let table = Self::with_store(store);
        let lease = lease.filter(|_| table.store.supports_leasing());
        let all = table.refresh_cache().await?;
        for s in &all {
            if !matches!(
                s.entry.state,
                SessionState::Running | SessionState::Resuming | SessionState::Creating
            ) {
                continue;
            }
            let id = &s.entry.spec.session_id;
            if let Some(l) = lease {
                match table
                    .store
                    .try_claim_lease(id, &l.replica_id, l.ttl)
                    .await?
                {
                    ClaimOutcome::Owned { .. } => {}
                    ClaimOutcome::HeldByOther { owner_replica, .. } => {
                        tracing::debug!(
                            session = %id, %owner_replica,
                            "reconcile: live foreign lease — skipping (owner probes it)"
                        );
                        continue;
                    }
                    ClaimOutcome::NotFound => continue,
                }
            }
            let alive = match &s.entry.handle {
                Some(h) => runner.health(h).await.unwrap_or(false),
                None => false,
            };
            // Freshness re-read: the entry may have moved while we probed.
            let Some(fresh) = table.store.get(id).await? else {
                continue;
            };
            if fresh.entry.state != s.entry.state {
                continue; // moved underneath us; leave it
            }
            let mut e = fresh.entry.clone();
            if alive {
                e.state = SessionState::Running;
            } else {
                e.state = SessionState::Crashed;
                e.last_error = Some("orchestrator_restart".into());
            }
            e.updated_at = chrono::Utc::now();
            table.store.force_update(&e).await?;
            if !alive {
                // Crashed: hand ownership back so whichever replica gets
                // the resume request owns the session from there.
                if let Some(l) = lease {
                    let _ = table.store.release_lease(id, &l.replica_id).await;
                }
            }
            let arc = {
                let mut map = table.inner.write().await;
                merge_stored(
                    &mut map,
                    &StoredSession {
                        entry: e,
                        lease: fresh.lease.clone(),
                        last_activity_ms: fresh.last_activity_ms,
                    },
                )
            };
            if alive {
                arc.ready_notify.notify_waiters();
            }
            tracing::info!(
                session = %id,
                from = ?fresh.entry.state,
                alive,
                "reconciled after orchestrator restart"
            );
        }
        table.persist().await?;
        Ok(table)
    }

    /// Idle-hibernation candidates: Running, no attached WS connections, and
    /// idle for longer than `timeout_ms`. The store is re-read first so the
    /// idle clock accounts for activity flushed by ANOTHER replica's proxy
    /// (monotonic merge in `merge_stored`). Advisory — the caller must still
    /// CAS (state may move between here and there).
    pub async fn list_idle_candidates(&self, timeout_ms: u64) -> Vec<String> {
        if let Err(e) = self.refresh_cache().await {
            tracing::warn!(error = %e, "idle sweep: store refresh failed; using cached view");
        }
        let map = self.inner.read().await;
        map.values()
            .filter(|e| {
                e.state == SessionState::Running
                    && e.active_conns.load(Ordering::Relaxed) == 0
                    && e.idle_ms() > timeout_ms
            })
            .map(|e| e.spec.session_id.clone())
            .collect()
    }

    /// Deep-archive candidates (MVS3-C): Hibernating entries with no
    /// attached connections, in that state for longer than `timeout_ms`
    /// (`updated_at`-based, so it survives orchestrator restarts), not yet
    /// archived, and still holding the stopped container's handle.
    /// Advisory — the sweep re-validates under `cas_set_archived`.
    pub async fn list_deep_archive_candidates(&self, timeout_ms: u64) -> Vec<String> {
        let cutoff = chrono::Utc::now()
            - chrono::Duration::milliseconds(timeout_ms.min(i64::MAX as u64) as i64);
        if let Err(e) = self.refresh_cache().await {
            tracing::warn!(error = %e, "deep-archive sweep: store refresh failed; using cached view");
        }
        let map = self.inner.read().await;
        map.values()
            .filter(|e| {
                e.state == SessionState::Hibernating
                    && e.archived.is_none()
                    && e.handle.is_some()
                    && e.active_conns.load(Ordering::Relaxed) == 0
                    && e.updated_at <= cutoff
            })
            .map(|e| e.spec.session_id.clone())
            .collect()
    }

    /// CAS on the deep-archive marker (NOT the lifecycle state — the entry
    /// stays `Hibernating` through a deep archive; see `archive.rs`).
    /// Arbitrated by the store: exactly one concurrent claim wins ACROSS
    /// replicas.
    ///
    /// - `Some(manifest)` (claim): succeeds only from `Hibernating` with no
    ///   marker; clears the stale handle (the container+volumes are
    ///   destroyed right after) and bumps `updated_at`.
    /// - `None` (release, post successful restore+resume): succeeds only
    ///   when a marker is present.
    ///
    /// Returns `false` on a CAS miss (concurrent claim, entry moved, or
    /// gone).
    pub async fn cas_set_archived(
        &self,
        id: &str,
        manifest: Option<ArchiveManifest>,
    ) -> Result<bool> {
        match self.store.cas_set_archived(id, manifest).await? {
            Some(p) => {
                let mut map = self.inner.write().await;
                merge_stored(&mut map, &StoredSession::new(p));
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Update `last_error` without a state transition (deep-archive sweep
    /// failure diagnostics). No-op when the entry is gone.
    pub async fn set_last_error(&self, id: &str, msg: String) -> Result<()> {
        self.store.set_last_error(id, &msg).await?;
        let mut map = self.inner.write().await;
        if let Some(entry) = map.get(id) {
            let mut next = (**entry).clone();
            next.last_error = Some(msg);
            map.insert(id.to_string(), Arc::new(next));
        }
        Ok(())
    }

    /// Number of sessions in the local cache (populated at load; refreshed
    /// by `list`/sweeps — use `list()` for the authoritative shared view).
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
    use crate::error::OrchestratorError;
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
            .fetch_add(1, Ordering::Relaxed);
        // Both are "idle" for 0ms timeout, but busy is vetoed.
        let c = t.list_idle_candidates(0).await;
        assert_eq!(c, vec!["idle".to_string()]);
        // Huge timeout → nobody.
        assert!(t.list_idle_candidates(u64::MAX).await.is_empty());
    }

    #[tokio::test]
    async fn flush_activity_throttles_and_forces() {
        // File backend no-ops the durable write; assert the throttle state
        // machine (last_flushed_activity_ms) itself.
        let dir = tempfile::tempdir().unwrap();
        let t = RoutingTable::new(dir.path());
        t.insert_new(SessionEntry::new_creating(test_spec("f1")))
            .await
            .unwrap();
        let e = t.get("f1").await.unwrap();
        e.last_activity_ms.store(5000, Ordering::Relaxed);
        // First flush always goes through (prev_flushed = 0).
        t.flush_activity("f1", false).await.unwrap();
        assert_eq!(e.last_flushed_activity_ms.load(Ordering::Relaxed), 5000);
        // +999ms → throttled; +1000ms → flushed; force → always.
        e.last_activity_ms.store(5999, Ordering::Relaxed);
        t.flush_activity("f1", false).await.unwrap();
        assert_eq!(e.last_flushed_activity_ms.load(Ordering::Relaxed), 5000);
        t.flush_activity("f1", true).await.unwrap();
        assert_eq!(e.last_flushed_activity_ms.load(Ordering::Relaxed), 5999);
        // +1ms past the forced flush → still throttled.
        e.last_activity_ms.store(6000, Ordering::Relaxed);
        t.flush_activity("f1", false).await.unwrap();
        assert_eq!(e.last_flushed_activity_ms.load(Ordering::Relaxed), 5999);
        // +1000ms past the watermark → flushed without force.
        e.last_activity_ms.store(6999, Ordering::Relaxed);
        t.flush_activity("f1", false).await.unwrap();
        assert_eq!(e.last_flushed_activity_ms.load(Ordering::Relaxed), 6999);
        // Unknown id → no-op.
        t.flush_activity("nope", true).await.unwrap();
    }

    /// Two tables sharing one store see each other's writes (the
    /// multi-replica convergence property, exercised with the file backend
    /// as the shared truth).
    #[tokio::test]
    async fn shared_store_converges_across_tables() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn SessionStore> =
            Arc::new(FileSessionStore::open(dir.path()).await.unwrap());
        let a = RoutingTable::with_store(store.clone());
        let b = RoutingTable::with_store(store);
        a.insert_new(SessionEntry::new_creating(test_spec("x")))
            .await
            .unwrap();
        a.cas_transition(
            "x",
            SessionState::Creating,
            SessionState::Running,
            Some(handle(43999)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        // B never saw the insert, yet reads converge via the store.
        let x = b.get("x").await.unwrap();
        assert_eq!(x.state, SessionState::Running);
        assert_eq!(x.handle.as_ref().unwrap().host_port, 43999);
        assert_eq!(b.list().await.len(), 1);
        // CAS from B arbitrates against A's state.
        let missed = b
            .cas_transition(
                "x",
                SessionState::Creating,
                SessionState::Failed,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(missed.is_none());
        let hit = b
            .cas_transition(
                "x",
                SessionState::Running,
                SessionState::Hibernating,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(hit.is_some());
        assert_eq!(a.get("x").await.unwrap().state, SessionState::Hibernating);
    }
}
