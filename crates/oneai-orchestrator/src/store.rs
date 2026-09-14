//! Durable session-store abstraction (MVS4-A).
//!
//! The routing table's *shared truth* lives behind [`SessionStore`]:
//!
//! - [`FileSessionStore`] — the MVS2 default: whole-file JSON
//!   (`<registry_dir>/sessions.json`), atomic `write(tmp) → rename`
//!   (supervisor-registry pattern). Single replica; lease operations are
//!   no-ops (the lone replica implicitly owns every session).
//! - `PgSessionStore` (feature `postgres`, `pg_session_store.rs`) — the
//!   multi-replica backend: one row per session in a shared Postgres, CAS
//!   transitions expressed as conditional `UPDATE … RETURNING`, per-session
//!   leases via `owner_replica`/`lease_expires_at` columns.
//!
//! [`RoutingTable`](crate::registry::RoutingTable) keeps a per-replica
//! in-memory hot cache (the process-local atomics: `active_conns`,
//! `last_activity_ms`, `ready_notify`) and delegates every durable read and
//! write to the store, so replicas converge on the shared state without
//! per-frame Pg traffic (activity is flushed at ≥1s granularity via
//! [`SessionStore::touch_activity`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::archive::ArchiveManifest;
use crate::error::{OrchestratorError, Result};
use crate::fsm::{validate_transition, PersistedEntry, SessionState};
use crate::runner::ContainerHandle;

/// Registry file name inside the registry dir (file backend).
pub const REGISTRY_FILE: &str = "sessions.json";

/// On-disk shape of the file backend (versioned envelope like the supervisor
/// registry). Kept byte-compatible with MVS2/MVS3 `sessions.json` files.
#[derive(Debug, Serialize, Deserialize)]
struct RegistryFile {
    sessions: Vec<PersistedEntry>,
}

/// Per-session lease metadata (multi-replica ownership).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseInfo {
    /// Replica currently owning the session.
    pub owner_replica: String,
    /// Lease deadline; past this instant any replica may take over.
    pub expires_at: DateTime<Utc>,
}

/// Outcome of a lease-claim attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// We now own the lease.
    Owned { lease_expires_at: DateTime<Utc> },
    /// Another replica holds a fresh lease.
    HeldByOther {
        owner_replica: String,
        lease_expires_at: DateTime<Utc>,
    },
    /// Session is gone.
    NotFound,
}

/// A durable session row as returned by the store: the lifecycle entry plus
/// multi-replica metadata that has no place in `PersistedEntry` (which is the
/// MVS2 file-format contract).
#[derive(Debug, Clone)]
pub struct StoredSession {
    /// Lifecycle state (spec/handle/state/last_error/updated_at/archived).
    pub entry: PersistedEntry,
    /// Current lease; `None` when unowned — or always `None` on backends
    /// without leasing (file mode).
    pub lease: Option<LeaseInfo>,
    /// Last durable activity timestamp (unix millis) written by the owning
    /// replica's throttled flush; `0` on backends that don't track it.
    pub last_activity_ms: u64,
}

impl StoredSession {
    /// Wrap a plain entry (no lease, no durable activity).
    pub fn new(entry: PersistedEntry) -> Self {
        Self {
            entry,
            lease: None,
            last_activity_ms: 0,
        }
    }
}

/// Shared-truth session store. Implementations must be safe for concurrent
/// use by multiple replicas (CAS operations are the synchronization
/// primitive; leases gate ownership-scoped work like sweeps).
///
/// CAS contract (mirrors the MVS2 in-memory `RoutingTable` semantics):
/// unknown id → `Ok(None)`; current state ≠ expected → `Ok(None)`; illegal
/// FSM transition → `Err(IllegalTransition)`.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Load every session (startup / reconcile / list).
    async fn load_all(&self) -> Result<Vec<StoredSession>>;

    /// Fetch one session with its lease + activity metadata.
    async fn get(&self, id: &str) -> Result<Option<StoredSession>>;

    /// Insert a brand-new entry. `Err(AlreadyExists)` when the id is taken
    /// (across ALL replicas on shared backends — the primary key arbitrates).
    async fn insert(&self, entry: &PersistedEntry) -> Result<()>;

    /// CAS state transition; `new_handle`/`last_error` are merged when
    /// `Some`. Returns the post-transition entry on a hit, `None` on a miss.
    async fn cas_transition(
        &self,
        id: &str,
        expected: SessionState,
        to: SessionState,
        new_handle: Option<ContainerHandle>,
        last_error: Option<String>,
    ) -> Result<Option<PersistedEntry>>;

    /// CAS on the deep-archive marker (MVS3-C): `Some(manifest)` claims
    /// (only from `Hibernating` with no marker; clears the stale handle),
    /// `None` releases (only when a marker is present). Returns the
    /// post-update entry on a hit.
    async fn cas_set_archived(
        &self,
        id: &str,
        manifest: Option<ArchiveManifest>,
    ) -> Result<Option<PersistedEntry>>;

    /// Unconditional overwrite of the durable fields (startup reconcile
    /// only — callers must gate it themselves, e.g. via a lease claim).
    /// No-op when the id is gone.
    async fn force_update(&self, entry: &PersistedEntry) -> Result<()>;

    /// Update `last_error` without a state transition. No-op when gone.
    async fn set_last_error(&self, id: &str, msg: &str) -> Result<()>;

    /// Persist an activity timestamp (unix millis). Backends that don't
    /// track durable activity (file mode) no-op. Implementations must keep
    /// the stored value monotonic (`GREATEST`).
    async fn touch_activity(&self, id: &str, activity_ms: u64) -> Result<()>;

    /// Remove an entry entirely (post-Destroyed tombstone cleanup).
    async fn remove(&self, id: &str) -> Result<()>;

    /// Durability barrier: file backend writes the whole file; row-oriented
    /// backends no-op (every mutation is already durable).
    async fn flush(&self) -> Result<()>;

    /// Filesystem path of the backing store, when it has one (banner /
    /// diagnostics). `None` for shared backends.
    fn store_path(&self) -> Option<&Path> {
        None
    }

    // ── Leases (multi-replica ownership; no-ops without leasing) ─────────

    /// Whether this backend supports per-session leases. File mode: false —
    /// the single replica implicitly owns everything and callers skip lease
    /// gating entirely.
    fn supports_leasing(&self) -> bool;

    /// Claim ownership, or renew when we already own it. Succeeds when the
    /// session is unowned, the lease has expired, or `replica_id` is the
    /// current owner.
    async fn try_claim_lease(
        &self,
        id: &str,
        replica_id: &str,
        ttl: Duration,
    ) -> Result<ClaimOutcome>;

    /// Renew a lease we own. `Ok(None)` when we no longer own it (lost to a
    /// takeover) or the session is gone.
    async fn renew_lease(
        &self,
        id: &str,
        replica_id: &str,
        ttl: Duration,
    ) -> Result<Option<DateTime<Utc>>>;

    /// Release our lease early (clean disconnect / shutdown). No-op when we
    /// don't own it.
    async fn release_lease(&self, id: &str, replica_id: &str) -> Result<()>;

    /// Release every lease held by `replica_id` (graceful shutdown).
    async fn release_all_leases(&self, replica_id: &str) -> Result<()>;

    /// Ids of all sessions currently owned by `replica_id` with a fresh
    /// lease (sweep scoping). Backends without leasing return every id.
    async fn list_owned_ids(&self, replica_id: &str) -> Result<Vec<String>>;
}

// ─── Lease identity + RAII guard (MVS4-A) ─────────────────────────────────────

/// Who this replica is, for lease purposes. Carried by the sweep and the WS
/// proxy so every ownership-scoped action claims/renews under one identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseIdentity {
    /// Unique per orchestrator process (uuid v4 unless pinned by config).
    pub replica_id: String,
    /// Lease time-to-live; renewal runs at `ttl/3`.
    pub ttl: Duration,
}

/// RAII lease ownership for one session (mirrors `proxy::ConnGuard`).
///
/// Precondition: the caller already won
/// [`SessionStore::try_claim_lease`]. On construction a renewal task starts
/// (every `ttl/3`) that also drains the throttled durable-activity flush —
/// a foreign replica's idle sweep therefore never sees stale activity for a
/// session this replica is actively serving. On drop the renewal stops, the
/// activity is force-flushed and the lease is released best-effort (a lost
/// release is covered by expiry).
pub struct LeaseGuard {
    table: crate::registry::RoutingTable,
    session_id: String,
    replica_id: String,
    cancel: tokio_util::sync::CancellationToken,
}

impl LeaseGuard {
    /// Start heartbeating an already-claimed lease.
    pub fn start(
        table: crate::registry::RoutingTable,
        session_id: String,
        replica_id: String,
        ttl: Duration,
    ) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let t = table.clone();
        let id = session_id.clone();
        let rid = replica_id.clone();
        let c = cancel.clone();
        let tick = (ttl / 3).max(Duration::from_millis(50));
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            iv.tick().await; // first tick fires immediately — skip it
            loop {
                tokio::select! {
                    _ = c.cancelled() => break,
                    _ = iv.tick() => {}
                }
                // Durable activity flush first (throttled ≥1s internally),
                // then the renewal — a renewal gap must never leave the
                // sweep-facing activity clock behind.
                if let Err(e) = t.flush_activity(&id, false).await {
                    tracing::warn!(session = %id, error = %e, "lease heartbeat: activity flush failed");
                }
                match t.store().renew_lease(&id, &rid, ttl).await {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        // Lost to a takeover (renewal gap exceeded the TTL —
                        // GC pause / Pg outage). Stop heartbeating; the
                        // in-flight proxy keeps pumping (accepted MVS4-A
                        // race, design §6 MVS4/R3 — the new owner's sweep
                        // re-checks durable activity before hibernating).
                        tracing::warn!(
                            session = %id, replica = %rid,
                            "lease lost mid-flight (taken over by another replica)"
                        );
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(session = %id, error = %e, "lease renewal failed; retrying next tick");
                    }
                }
            }
        });
        Self {
            table,
            session_id,
            replica_id,
            cancel,
        }
    }

    /// The session this guard owns.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let table = self.table.clone();
        let id = self.session_id.clone();
        let rid = self.replica_id.clone();
        // Detached best-effort: force-flush activity (so a foreign sweep
        // sees the true idle clock immediately) and release the lease (so
        // another replica can own the session without waiting for expiry).
        tokio::spawn(async move {
            if let Err(e) = table.flush_activity(&id, true).await {
                tracing::warn!(session = %id, error = %e, "lease release: activity flush failed");
            }
            if let Err(e) = table.store().release_lease(&id, &rid).await {
                tracing::warn!(session = %id, error = %e, "lease release failed (expiry covers it)");
            }
        });
    }
}

// ─── File backend (MVS2 default, single replica) ─────────────────────────────

/// Whole-file JSON session store: `<dir>/sessions.json`, atomic
/// `write(<uuid>.tmp) → chmod 600 → rename`. Every mutation serializes under
/// one operation lock and rewrites the whole file (sessions are few and the
/// file is small — same cost profile as MVS2, where every CAS transition
/// persisted the table).
pub struct FileSessionStore {
    path: PathBuf,
    /// Durable mirror of the file (source of truth for reads in this
    /// backend; the routing-table cache adds the process-local atomics).
    inner: RwLock<HashMap<String, PersistedEntry>>,
    /// Serializes mutations + file writes (concurrent `write(tmp) → rename`
    /// must not interleave; tmp names are uuid-suffixed as a second line of
    /// defense — MVS2 regression).
    op_lock: Mutex<()>,
}

impl FileSessionStore {
    /// Empty store persisting to `<dir>/sessions.json` (dir created on the
    /// first write).
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(REGISTRY_FILE),
            inner: RwLock::new(HashMap::new()),
            op_lock: Mutex::new(()),
        }
    }

    /// Open an existing store: missing file = empty store; a malformed file
    /// is an error (better to refuse than silently drop live sessions).
    pub async fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let store = Self::new(dir);
        if !store.path.exists() {
            return Ok(store);
        }
        let bytes = tokio::fs::read(&store.path).await?;
        let file: RegistryFile = serde_json::from_slice(&bytes)
            .map_err(|e| OrchestratorError::Persist(format!("{}: {e}", store.path.display())))?;
        {
            let mut map = store.inner.write().await;
            for p in file.sessions {
                map.insert(p.spec.session_id.clone(), p);
            }
        }
        Ok(store)
    }

    /// Registry file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mutate the in-memory map, then atomically persist the whole file.
    /// The op lock spans mutation + write so file contents are monotonic.
    async fn commit<T>(
        &self,
        f: impl FnOnce(&mut HashMap<String, PersistedEntry>) -> Result<T>,
    ) -> Result<T> {
        let _guard = self.op_lock.lock().await;
        let out = {
            let mut map = self.inner.write().await;
            f(&mut map)?
        };
        self.persist_locked().await?;
        Ok(out)
    }

    /// Whole-file atomic write (caller holds `op_lock`). Unix: best-effort
    /// chmod 600 (the file may contain injected env secrets — MVS2
    /// limitation, D5 notes Secret Manager for production).
    async fn persist_locked(&self) -> Result<()> {
        let sessions: Vec<PersistedEntry> = {
            let map = self.inner.read().await;
            let mut v: Vec<_> = map.values().cloned().collect();
            v.sort_by(|a, b| a.spec.session_id.cmp(&b.spec.session_id));
            v
        };
        let file = RegistryFile { sessions };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|e| OrchestratorError::Persist(e.to_string()))?;
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

    /// Apply a CAS-style mutation to one entry (existence → expected-state →
    /// FSM validation, mirroring the MVS2 in-memory order). The closure
    /// returns `Err` for illegal transitions (surfaced to the caller),
    /// `Ok(None)` for a CAS miss, `Ok(Some(next))` for a hit.
    async fn cas_entry(
        &self,
        id: &str,
        mutate: impl FnOnce(&PersistedEntry) -> Result<Option<PersistedEntry>>,
    ) -> Result<Option<PersistedEntry>> {
        self.commit(|map| {
            let Some(entry) = map.get(id) else {
                return Ok(None);
            };
            match mutate(entry)? {
                Some(next) => {
                    map.insert(id.to_string(), next.clone());
                    Ok(Some(next))
                }
                None => Ok(None),
            }
        })
        .await
    }
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn load_all(&self) -> Result<Vec<StoredSession>> {
        let map = self.inner.read().await;
        let mut v: Vec<_> = map.values().cloned().map(StoredSession::new).collect();
        v.sort_by(|a, b| a.entry.spec.session_id.cmp(&b.entry.spec.session_id));
        Ok(v)
    }

    async fn get(&self, id: &str) -> Result<Option<StoredSession>> {
        Ok(self
            .inner
            .read()
            .await
            .get(id)
            .cloned()
            .map(StoredSession::new))
    }

    async fn insert(&self, entry: &PersistedEntry) -> Result<()> {
        let id = entry.spec.session_id.clone();
        self.commit(move |map| {
            if map.contains_key(&id) {
                return Err(OrchestratorError::AlreadyExists(id.clone()));
            }
            map.insert(id.clone(), entry.clone());
            Ok(())
        })
        .await
    }

    async fn cas_transition(
        &self,
        id: &str,
        expected: SessionState,
        to: SessionState,
        new_handle: Option<ContainerHandle>,
        last_error: Option<String>,
    ) -> Result<Option<PersistedEntry>> {
        self.cas_entry(id, |entry| {
            if entry.state != expected {
                return Ok(None); // CAS miss
            }
            // Illegal transitions must surface as Err, not as a miss (same
            // order as the MVS2 in-memory CAS).
            validate_transition(expected, to)?;
            let mut next = entry.clone();
            next.state = to;
            if new_handle.is_some() {
                next.handle = new_handle;
            }
            next.last_error = last_error;
            next.updated_at = Utc::now();
            Ok(Some(next))
        })
        .await
    }

    async fn cas_set_archived(
        &self,
        id: &str,
        manifest: Option<ArchiveManifest>,
    ) -> Result<Option<PersistedEntry>> {
        self.cas_entry(id, |entry| {
            let setting = manifest.is_some();
            if setting {
                if entry.state != SessionState::Hibernating || entry.archived.is_some() {
                    return Ok(None);
                }
            } else if entry.archived.is_none() {
                return Ok(None);
            }
            let mut next = entry.clone();
            if setting {
                next.handle = None;
                next.updated_at = Utc::now();
            }
            next.archived = manifest;
            Ok(Some(next))
        })
        .await
    }

    async fn force_update(&self, entry: &PersistedEntry) -> Result<()> {
        let id = entry.spec.session_id.clone();
        self.commit(move |map| {
            if map.contains_key(&id) {
                map.insert(id.clone(), entry.clone());
            }
            Ok(())
        })
        .await
    }

    async fn set_last_error(&self, id: &str, msg: &str) -> Result<()> {
        let msg = msg.to_string();
        self.commit(move |map| {
            if let Some(entry) = map.get(id) {
                let mut next = entry.clone();
                next.last_error = Some(msg);
                map.insert(id.to_string(), next);
            }
            Ok(())
        })
        .await
    }

    async fn touch_activity(&self, _id: &str, _activity_ms: u64) -> Result<()> {
        Ok(()) // file mode: activity lives only in the replica's atomics
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.commit(move |map| {
            map.remove(&id);
            Ok(())
        })
        .await
    }

    async fn flush(&self) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        self.persist_locked().await
    }

    fn store_path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn supports_leasing(&self) -> bool {
        false // single replica owns everything implicitly
    }

    async fn try_claim_lease(
        &self,
        id: &str,
        _replica_id: &str,
        ttl: Duration,
    ) -> Result<ClaimOutcome> {
        let exists = self.inner.read().await.contains_key(id);
        Ok(if exists {
            ClaimOutcome::Owned {
                lease_expires_at: Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default(),
            }
        } else {
            ClaimOutcome::NotFound
        })
    }

    async fn renew_lease(
        &self,
        id: &str,
        _replica_id: &str,
        ttl: Duration,
    ) -> Result<Option<DateTime<Utc>>> {
        let exists = self.inner.read().await.contains_key(id);
        Ok(exists.then(|| Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default()))
    }

    async fn release_lease(&self, _id: &str, _replica_id: &str) -> Result<()> {
        Ok(())
    }

    async fn release_all_leases(&self, _replica_id: &str) -> Result<()> {
        Ok(())
    }

    async fn list_owned_ids(&self, _replica_id: &str) -> Result<Vec<String>> {
        let map = self.inner.read().await;
        let mut v: Vec<String> = map.keys().cloned().collect();
        v.sort();
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsm::SessionEntry;
    use crate::runner::SessionSpec;

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

    fn persisted(id: &str) -> PersistedEntry {
        SessionEntry::new_creating(test_spec(id)).to_persisted()
    }

    async fn tmp_store() -> (tempfile::TempDir, FileSessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FileSessionStore::new(dir.path());
        (dir, s)
    }

    #[tokio::test]
    async fn insert_get_roundtrip_and_duplicate() {
        let (_d, s) = tmp_store().await;
        s.insert(&persisted("a")).await.unwrap();
        assert_eq!(
            s.get("a").await.unwrap().unwrap().entry.state,
            SessionState::Creating
        );
        let dup = s.insert(&persisted("a")).await;
        assert!(matches!(dup, Err(OrchestratorError::AlreadyExists(_))));
        assert!(s.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn insert_is_durable_immediately() {
        let (d, s) = tmp_store().await;
        s.insert(&persisted("a")).await.unwrap();
        // No explicit flush: a reopen sees the row (mid-spawn crash safety).
        let s2 = FileSessionStore::open(d.path()).await.unwrap();
        assert_eq!(s2.load_all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cas_transition_order_existence_then_state_then_validate() {
        let (_d, s) = tmp_store().await;
        s.insert(&persisted("a")).await.unwrap();
        // Unknown id → None (even for an illegal pair).
        assert!(s
            .cas_transition(
                "zz",
                SessionState::Running,
                SessionState::Creating,
                None,
                None
            )
            .await
            .unwrap()
            .is_none());
        // State mismatch → None (even when the pair is illegal — MVS2 order).
        assert!(s
            .cas_transition(
                "a",
                SessionState::Running,
                SessionState::Hibernating,
                None,
                None
            )
            .await
            .unwrap()
            .is_none());
        // Expected matches but transition illegal → Err.
        let r = s
            .cas_transition(
                "a",
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
        // Legal hit merges handle + bumps updated_at.
        let before = s.get("a").await.unwrap().unwrap().entry.updated_at;
        let out = s
            .cas_transition(
                "a",
                SessionState::Creating,
                SessionState::Running,
                Some(handle(41000)),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.state, SessionState::Running);
        assert_eq!(out.handle.unwrap().host_port, 41000);
        assert!(out.updated_at >= before);
    }

    #[tokio::test]
    async fn cas_set_archived_claim_release() {
        let (_d, s) = tmp_store().await;
        s.insert(&persisted("a")).await.unwrap();
        s.cas_transition(
            "a",
            SessionState::Creating,
            SessionState::Running,
            Some(handle(1)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        s.cas_transition(
            "a",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let m = ArchiveManifest {
            session_id: "a".into(),
            archived_at: Utc::now().to_rfc3339(),
            volumes: vec![],
        };
        // Claim from Hibernating: hit; clears the handle.
        let out = s
            .cas_set_archived("a", Some(m.clone()))
            .await
            .unwrap()
            .unwrap();
        assert!(out.archived.is_some() && out.handle.is_none());
        // Second claim: miss.
        assert!(s.cas_set_archived("a", Some(m)).await.unwrap().is_none());
        // Release: hit; second release: miss.
        assert!(s.cas_set_archived("a", None).await.unwrap().is_some());
        assert!(s.cas_set_archived("a", None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn force_update_and_set_last_error_and_remove() {
        let (_d, s) = tmp_store().await;
        s.insert(&persisted("a")).await.unwrap();
        let mut e = s.get("a").await.unwrap().unwrap().entry;
        e.state = SessionState::Crashed;
        s.force_update(&e).await.unwrap();
        assert_eq!(
            s.get("a").await.unwrap().unwrap().entry.state,
            SessionState::Crashed
        );
        s.set_last_error("a", "boom").await.unwrap();
        assert_eq!(
            s.get("a")
                .await
                .unwrap()
                .unwrap()
                .entry
                .last_error
                .as_deref(),
            Some("boom")
        );
        s.remove("a").await.unwrap();
        assert!(s.get("a").await.unwrap().is_none());
        s.remove("a").await.unwrap(); // idempotent
    }

    #[tokio::test]
    async fn open_missing_file_is_empty_and_malformed_errors() {
        let d = tempfile::tempdir().unwrap();
        let s = FileSessionStore::open(d.path()).await.unwrap();
        assert!(s.load_all().await.unwrap().is_empty());
        std::fs::write(d.path().join(REGISTRY_FILE), "{oops").unwrap();
        assert!(FileSessionStore::open(d.path()).await.is_err());
    }

    #[tokio::test]
    async fn legacy_sessions_json_loads() {
        // Pre-MVS3-C file shape (no `archived` field) must open unchanged.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(REGISTRY_FILE),
            r#"{ "sessions": [{
                "spec": {
                    "session_id": "legacy", "image": "img",
                    "state_volume": "sv", "workspace_volume": "wv",
                    "env": [], "bind_host": "127.0.0.1", "container_port": 8787,
                    "provider_config": null, "created_at": "2026-09-10T00:00:00Z"
                },
                "handle": null, "state": "Hibernating", "last_error": null,
                "updated_at": "2026-09-10T00:00:00Z"
            }] }"#,
        )
        .unwrap();
        let s = FileSessionStore::open(d.path()).await.unwrap();
        let all = s.load_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].entry.archived.is_none());
        assert!(!s.supports_leasing());
    }

    #[tokio::test]
    async fn file_leases_are_noop_owned() {
        let (_d, s) = tmp_store().await;
        s.insert(&persisted("a")).await.unwrap();
        let ttl = Duration::from_secs(30);
        assert!(matches!(
            s.try_claim_lease("a", "r1", ttl).await.unwrap(),
            ClaimOutcome::Owned { .. }
        ));
        // Single replica: any id "owns" — claims never conflict.
        assert!(matches!(
            s.try_claim_lease("a", "r2", ttl).await.unwrap(),
            ClaimOutcome::Owned { .. }
        ));
        assert!(s.renew_lease("a", "r1", ttl).await.unwrap().is_some());
        assert!(s.release_lease("a", "r1").await.is_ok());
        assert_eq!(s.list_owned_ids("r1").await.unwrap(), vec!["a".to_string()]);
        assert!(matches!(
            s.try_claim_lease("gone", "r1", ttl).await.unwrap(),
            ClaimOutcome::NotFound
        ));
    }

    #[tokio::test]
    async fn concurrent_inserts_serialize_no_tmp_race() {
        // MVS2 regression: shared tmp name races → ENOENT. uuid tmp names +
        // op lock must keep 20 concurrent commits consistent.
        let (d, s) = tmp_store().await;
        let s = std::sync::Arc::new(s);
        let mut joins = Vec::new();
        for i in 0..20 {
            let s = s.clone();
            joins.push(tokio::spawn(async move {
                s.insert(&persisted(&format!("c{i}"))).await.unwrap();
            }));
        }
        for j in joins {
            j.await.unwrap();
        }
        assert_eq!(s.load_all().await.unwrap().len(), 20);
        let s2 = FileSessionStore::open(d.path()).await.unwrap();
        assert_eq!(s2.load_all().await.unwrap().len(), 20);
        // No leftover tmp files.
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
}
