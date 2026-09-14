//! MVS4-A multi-replica integration tests: two `OrchestratorState`s sharing
//! ONE leasing store + FakeRunner (no docker, no Pg — the lease/CAS
//! semantics under test are the store contract, which `pg_store_tests.rs`
//! verifies against real Postgres separately).
//!
//! Scenarios mirror the real-machine acceptance (deploy/docker/mvs4a_verify.mjs):
//! shared-truth reads, initial-lease ownership, WS-upgrade 409 on a foreign
//! fresh lease, takeover of an orphaned session after the owner "dies"
//! (container untouched), simultaneous reconcile (exactly one probe + one
//! owner per entry, no false Crashed), sweep/deep-archive ownership gating.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::{ensure_secret_env, test_config, test_spec, FakeRunner, TEST_SECRET};
use tokio::sync::Mutex;

use oneai_orchestrator::archive::{ArchiveManifest, DeepArchive, VolumeArchiveStore};
use oneai_orchestrator::config::QuotaConfig;
use oneai_orchestrator::error::{OrchestratorError, Result};
use oneai_orchestrator::fsm::{
    counts_toward_quota, validate_transition, PersistedEntry, SessionEntry, SessionState,
};
use oneai_orchestrator::idle::sweep_once;
use oneai_orchestrator::quota::QuotaReason;
use oneai_orchestrator::registry::RoutingTable;
use oneai_orchestrator::runner::{ContainerHandle, ContainerRunner};
use oneai_orchestrator::server::OrchestratorState;
use oneai_orchestrator::store::{
    ClaimOutcome, LeaseIdentity, LeaseInfo, SessionStore, StoredSession,
};

// ─── In-memory leasing store (Pg semantics, no Pg) ───────────────────────────

#[derive(Clone)]
struct Row {
    entry: PersistedEntry,
    lease: Option<LeaseInfo>,
    activity_ms: u64,
}

/// Mirrors `PgSessionStore`'s contract exactly (CAS order, GREATEST
/// activity, server-"clock" lease CAS on `Utc::now()`), minus the network.
#[derive(Default)]
pub struct MemLeaseStore {
    inner: Mutex<std::collections::HashMap<String, Row>>,
}

impl MemLeaseStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn to_stored(row: &Row) -> StoredSession {
        StoredSession {
            entry: row.entry.clone(),
            lease: row.lease.clone(),
            last_activity_ms: row.activity_ms,
        }
    }

    /// Test hook: pin a lease directly (e.g. an already-expired foreign
    /// lease) without going through the claim CAS.
    pub async fn set_lease(&self, id: &str, owner: &str, expires_at: DateTime<Utc>) {
        let mut map = self.inner.lock().await;
        if let Some(row) = map.get_mut(id) {
            row.lease = Some(LeaseInfo {
                owner_replica: owner.to_string(),
                expires_at,
            });
        }
    }

    pub async fn lease_of(&self, id: &str) -> Option<LeaseInfo> {
        self.inner
            .lock()
            .await
            .get(id)
            .and_then(|r| r.lease.clone())
    }
}

#[async_trait]
impl SessionStore for MemLeaseStore {
    async fn load_all(&self) -> Result<Vec<StoredSession>> {
        let map = self.inner.lock().await;
        let mut v: Vec<_> = map.values().map(Self::to_stored).collect();
        v.sort_by(|a, b| a.entry.spec.session_id.cmp(&b.entry.spec.session_id));
        Ok(v)
    }

    async fn get(&self, id: &str) -> Result<Option<StoredSession>> {
        Ok(self.inner.lock().await.get(id).map(Self::to_stored))
    }

    async fn insert(&self, entry: &PersistedEntry) -> Result<()> {
        let mut map = self.inner.lock().await;
        let id = &entry.spec.session_id;
        if map.contains_key(id) {
            return Err(OrchestratorError::AlreadyExists(id.clone()));
        }
        map.insert(
            id.clone(),
            Row {
                entry: entry.clone(),
                lease: None,
                activity_ms: 0,
            },
        );
        Ok(())
    }

    async fn insert_if_under_quota(
        &self,
        entry: &PersistedEntry,
        max_concurrent: Option<usize>,
    ) -> Result<Option<usize>> {
        // ONE mutex hold for count + insert — mirrors PgSessionStore's
        // single-transaction arbitration (the default trait impl's two-step
        // would race across the two OrchestratorStates in the quota test).
        let mut map = self.inner.lock().await;
        let id = &entry.spec.session_id;
        if map.contains_key(id) {
            return Err(OrchestratorError::AlreadyExists(id.clone()));
        }
        if let Some(max) = max_concurrent {
            let count = map
                .values()
                .filter(|r| {
                    counts_toward_quota(r.entry.state)
                        && r.entry.spec.tenant_id == entry.spec.tenant_id
                })
                .count();
            if count >= max {
                return Ok(Some(count));
            }
        }
        map.insert(
            id.clone(),
            Row {
                entry: entry.clone(),
                lease: None,
                activity_ms: 0,
            },
        );
        Ok(None)
    }

    async fn cas_transition(
        &self,
        id: &str,
        expected: SessionState,
        to: SessionState,
        new_handle: Option<ContainerHandle>,
        last_error: Option<String>,
    ) -> Result<Option<PersistedEntry>> {
        let mut map = self.inner.lock().await;
        let Some(row) = map.get_mut(id) else {
            return Ok(None);
        };
        if row.entry.state != expected {
            return Ok(None);
        }
        validate_transition(expected, to)?;
        row.entry.state = to;
        if new_handle.is_some() {
            row.entry.handle = new_handle;
        }
        row.entry.last_error = last_error;
        row.entry.updated_at = Utc::now();
        Ok(Some(row.entry.clone()))
    }

    async fn cas_set_archived(
        &self,
        id: &str,
        manifest: Option<ArchiveManifest>,
    ) -> Result<Option<PersistedEntry>> {
        let mut map = self.inner.lock().await;
        let Some(row) = map.get_mut(id) else {
            return Ok(None);
        };
        let setting = manifest.is_some();
        if setting {
            if row.entry.state != SessionState::Hibernating || row.entry.archived.is_some() {
                return Ok(None);
            }
            row.entry.handle = None;
            row.entry.updated_at = Utc::now();
        } else if row.entry.archived.is_none() {
            return Ok(None);
        }
        row.entry.archived = manifest;
        Ok(Some(row.entry.clone()))
    }

    async fn force_update(&self, entry: &PersistedEntry) -> Result<()> {
        let mut map = self.inner.lock().await;
        if let Some(row) = map.get_mut(&entry.spec.session_id) {
            row.entry = entry.clone();
        }
        Ok(())
    }

    async fn set_last_error(&self, id: &str, msg: &str) -> Result<()> {
        let mut map = self.inner.lock().await;
        if let Some(row) = map.get_mut(id) {
            row.entry.last_error = Some(msg.to_string());
        }
        Ok(())
    }

    async fn touch_activity(&self, id: &str, activity_ms: u64) -> Result<()> {
        let mut map = self.inner.lock().await;
        if let Some(row) = map.get_mut(id) {
            row.activity_ms = row.activity_ms.max(activity_ms); // GREATEST
        }
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<()> {
        self.inner.lock().await.remove(id);
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn supports_leasing(&self) -> bool {
        true
    }

    async fn try_claim_lease(
        &self,
        id: &str,
        replica_id: &str,
        ttl: Duration,
    ) -> Result<ClaimOutcome> {
        let mut map = self.inner.lock().await;
        let Some(row) = map.get_mut(id) else {
            return Ok(ClaimOutcome::NotFound);
        };
        let now = Utc::now();
        let free = match &row.lease {
            None => true,
            Some(l) => l.owner_replica == replica_id || l.expires_at < now,
        };
        if !free {
            let l = row.lease.clone().unwrap();
            return Ok(ClaimOutcome::HeldByOther {
                owner_replica: l.owner_replica,
                lease_expires_at: l.expires_at,
            });
        }
        let expires_at = now + chrono::Duration::from_std(ttl).unwrap();
        row.lease = Some(LeaseInfo {
            owner_replica: replica_id.to_string(),
            expires_at,
        });
        Ok(ClaimOutcome::Owned {
            lease_expires_at: expires_at,
        })
    }

    async fn renew_lease(
        &self,
        id: &str,
        replica_id: &str,
        ttl: Duration,
    ) -> Result<Option<DateTime<Utc>>> {
        let mut map = self.inner.lock().await;
        let Some(row) = map.get_mut(id) else {
            return Ok(None);
        };
        match &row.lease {
            Some(l) if l.owner_replica == replica_id => {
                let expires_at = Utc::now() + chrono::Duration::from_std(ttl).unwrap();
                row.lease = Some(LeaseInfo {
                    owner_replica: replica_id.to_string(),
                    expires_at,
                });
                Ok(Some(expires_at))
            }
            _ => Ok(None),
        }
    }

    async fn release_lease(&self, id: &str, replica_id: &str) -> Result<()> {
        let mut map = self.inner.lock().await;
        if let Some(row) = map.get_mut(id) {
            if row
                .lease
                .as_ref()
                .is_some_and(|l| l.owner_replica == replica_id)
            {
                row.lease = None;
            }
        }
        Ok(())
    }

    async fn release_all_leases(&self, replica_id: &str) -> Result<()> {
        let mut map = self.inner.lock().await;
        for row in map.values_mut() {
            if row
                .lease
                .as_ref()
                .is_some_and(|l| l.owner_replica == replica_id)
            {
                row.lease = None;
            }
        }
        Ok(())
    }

    async fn list_owned_ids(&self, replica_id: &str) -> Result<Vec<String>> {
        let map = self.inner.lock().await;
        let now = Utc::now();
        let mut v: Vec<String> = map
            .iter()
            .filter(|(_, r)| {
                r.lease
                    .as_ref()
                    .is_some_and(|l| l.owner_replica == replica_id && l.expires_at > now)
            })
            .map(|(id, _)| id.clone())
            .collect();
        v.sort();
        Ok(v)
    }
}

// ─── Fake archive store (deep-archive race test) ─────────────────────────────

struct FakeArchive {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl VolumeArchiveStore for FakeArchive {
    async fn archive(
        &self,
        session_id: &str,
        _volumes: &[String],
        _docker_bin: &str,
    ) -> Result<ArchiveManifest> {
        self.calls
            .lock()
            .await
            .push(format!("archive:{session_id}"));
        Ok(ArchiveManifest {
            session_id: session_id.to_string(),
            archived_at: Utc::now().to_rfc3339(),
            volumes: vec![],
        })
    }
    async fn restore(&self, _m: &ArchiveManifest, _docker_bin: &str) -> Result<()> {
        Ok(())
    }
    async fn has_archive(&self, _session_id: &str) -> bool {
        false
    }
    async fn remove(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }
}

// ─── Fixtures ────────────────────────────────────────────────────────────────

fn lease_cfg(
    dir: &Path,
    replica_id: &str,
    ttl: u64,
) -> oneai_orchestrator::config::OrchestratorConfig {
    let mut c = test_config(dir);
    c.replica_id = replica_id.to_string();
    c.lease_ttl_secs = ttl;
    c
}

async fn replica(
    dir: &Path,
    id: &str,
    ttl: u64,
    store: Arc<dyn SessionStore>,
    runner: Arc<FakeRunner>,
) -> Arc<OrchestratorState> {
    ensure_secret_env();
    OrchestratorState::with_session_store(lease_cfg(dir, id, ttl), runner, None, store)
        .await
        .expect("state")
}

fn identity(id: &str, ttl: Duration) -> LeaseIdentity {
    LeaseIdentity {
        replica_id: id.to_string(),
        ttl,
    }
}

fn handle_for(id: &str, port: u16) -> ContainerHandle {
    ContainerHandle {
        container_id: format!("cid-{id}"),
        container_name: format!("oneai-orch-{id}"),
        host_port: port,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// A creates → the row is in the shared truth with A as initial owner; B
/// (never told about the session) reads it through the store, lists agree.
#[tokio::test]
async fn create_via_a_is_shared_truth_for_b() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    let a = replica(dir.path(), "rep-a", 30, store.clone(), runner.clone()).await;
    let b = replica(dir.path(), "rep-b", 30, store.clone(), runner.clone()).await;

    let snap = a.create_session(Some("s1".into()), vec![]).await.unwrap();
    assert_eq!(snap.state, SessionState::Running);

    // Creator is the initial owner (fresh lease).
    let lease = store.lease_of("s1").await.expect("lease claimed");
    assert_eq!(lease.owner_replica, "rep-a");
    assert!(lease.expires_at > Utc::now());

    // B sees the session with the same durable fields; lists converge.
    let e = b.table.get("s1").await.expect("visible via store");
    assert_eq!(e.state, SessionState::Running);
    assert_eq!(
        e.handle.as_ref().unwrap().host_port,
        snap.host_port.unwrap()
    );
    let la: Vec<String> = a
        .table
        .list()
        .await
        .iter()
        .map(|s| s.session_id.clone())
        .collect();
    let lb: Vec<String> = b
        .table
        .list()
        .await
        .iter()
        .map(|s| s.session_id.clone())
        .collect();
    assert_eq!(la, lb);
}

/// WS upgrade on B while A holds a fresh lease → 409 + X-Oneai-Owner-Replica
/// (the frontend/LB stickiness contract).
#[tokio::test]
async fn ws_upgrade_on_non_owner_returns_409() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    let a = replica(dir.path(), "rep-a", 30, store.clone(), runner.clone()).await;
    let b = replica(dir.path(), "rep-b", 30, store.clone(), runner.clone()).await;
    a.create_session(Some("s1".into()), vec![]).await.unwrap();

    // Serve B's real router on an ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve = tokio::spawn(async move {
        axum::serve(listener, oneai_orchestrator::routes::router(b))
            .await
            .ok();
    });

    let url = format!("ws://127.0.0.1:{port}/v1/sessions/s1/ws?token={TEST_SECRET}");
    let err = tokio_tungstenite::connect_async(&url)
        .await
        .expect_err("non-owner must refuse the upgrade");
    let resp = match err {
        tokio_tungstenite::tungstenite::Error::Http(r) => r,
        other => panic!("expected HTTP refusal, got {other:?}"),
    };
    assert_eq!(resp.status().as_u16(), 409);
    assert_eq!(
        resp.headers()
            .get("x-oneai-owner-replica")
            .and_then(|v| v.to_str().ok()),
        Some("rep-a")
    );

    serve.abort();
}

/// Owner "dies" (lease stops being renewed); after expiry another replica's
/// reconcile takes the session over WITHOUT touching the container.
#[tokio::test]
async fn orphaned_session_taken_over_after_lease_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    let a = replica(dir.path(), "rep-a", 1, store.clone(), runner.clone()).await;
    a.create_session(Some("s1".into()), vec![]).await.unwrap();
    assert_eq!(store.lease_of("s1").await.unwrap().owner_replica, "rep-a");
    drop(a); // "kill -9": no release_all_leases, lease simply stops renewing

    // Container is still alive (docker kept running — the MVS2 restart model).
    tokio::time::sleep(Duration::from_millis(1200)).await;
    runner.mark_running("oneai-orch-s1").await;

    let _b = replica(dir.path(), "rep-b", 30, store.clone(), runner.clone()).await;
    // replica() runs reconcile at construction with B's identity → takeover.
    let lease = store.lease_of("s1").await.expect("lease");
    assert_eq!(lease.owner_replica, "rep-b");
    let e = _b.table.get("s1").await.unwrap();
    assert_eq!(e.state, SessionState::Running, "kept Running (re-mounted)");
    // Zero container operations: only the lease moved.
    assert_eq!(runner.count_calls("stop:").await, 0);
    assert_eq!(runner.count_calls("start:").await, 0);
    assert_eq!(runner.count_calls("spawn:").await, 1); // A's original create only
}

/// Reconcile must NOT probe or steal a session whose lease is live and
/// foreign — trust the owner's own health handling.
#[tokio::test]
async fn reconcile_skips_live_foreign_lease() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    // Seed a Running entry owned by "other" with a fresh lease.
    store
        .insert(&SessionEntry::new_creating(test_spec("s1")).to_persisted())
        .await
        .unwrap();
    store
        .cas_transition(
            "s1",
            SessionState::Creating,
            SessionState::Running,
            Some(handle_for("s1", 43001)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .set_lease("s1", "other", Utc::now() + chrono::Duration::seconds(60))
        .await;
    // The container is actually DEAD — but that's the owner's problem, not ours.

    let _self_replica = replica(dir.path(), "rep-b", 30, store.clone(), runner.clone()).await;
    let e = _self_replica.table.get("s1").await.unwrap();
    assert_eq!(e.state, SessionState::Running, "untouched");
    assert_eq!(runner.count_calls("health:").await, 0, "never probed");
    assert_eq!(store.lease_of("s1").await.unwrap().owner_replica, "other");
}

/// Two replicas reconcile SIMULTANEOUSLY against 5 orphaned Running
/// entries: every entry ends with exactly one owner, still Running, and
/// exactly one health probe each (claim-before-probe is the mutex).
#[tokio::test]
async fn simultaneous_reconcile_single_probe_and_owner() {
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    for i in 0..5 {
        let id = format!("sr{i}");
        store
            .insert(&SessionEntry::new_creating(test_spec(&id)).to_persisted())
            .await
            .unwrap();
        store
            .cas_transition(
                &id,
                SessionState::Creating,
                SessionState::Running,
                Some(handle_for(&id, 43200 + i)),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        // Expired lease from the dead previous owner.
        store
            .set_lease(
                &id,
                "dead-replica",
                Utc::now() - chrono::Duration::seconds(5),
            )
            .await;
        runner.mark_running(&format!("oneai-orch-{id}")).await;
    }

    let runner_ref: &dyn ContainerRunner = runner.as_ref();
    let id_b = identity("rep-b", Duration::from_secs(30));
    let id_c = identity("rep-c", Duration::from_secs(30));
    let (rb, rc) = tokio::join!(
        RoutingTable::reconcile_with_store(store.clone(), runner_ref, Some(&id_b)),
        RoutingTable::reconcile_with_store(store.clone(), runner_ref, Some(&id_c)),
    );
    rb.unwrap();
    rc.unwrap();

    let mut owners = std::collections::HashMap::new();
    for i in 0..5 {
        let id = format!("sr{i}");
        let row = store.get(&id).await.unwrap().unwrap();
        assert_eq!(row.entry.state, SessionState::Running, "no false Crashed");
        let owner = &row.lease.as_ref().unwrap().owner_replica;
        assert!(owner == "rep-b" || owner == "rep-c");
        *owners.entry(owner.clone()).or_insert(0) += 1;
    }
    assert_eq!(owners.values().sum::<i32>(), 5);
    // Claim-before-probe: exactly one probe per entry (5 total, not 10).
    assert_eq!(runner.count_calls("health:").await, 5);
}

/// A dead orphan whose container did NOT survive: the claimer marks it
/// Crashed and releases the lease (so the resume-triggering replica owns it).
#[tokio::test]
async fn reconcile_dead_orphan_crashes_and_frees_lease() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new(); // no mark_running → health false
    store
        .insert(&SessionEntry::new_creating(test_spec("s1")).to_persisted())
        .await
        .unwrap();
    store
        .cas_transition(
            "s1",
            SessionState::Creating,
            SessionState::Running,
            Some(handle_for("s1", 43001)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .set_lease(
            "s1",
            "dead-replica",
            Utc::now() - chrono::Duration::seconds(5),
        )
        .await;

    let b = replica(dir.path(), "rep-b", 30, store.clone(), runner.clone()).await;
    assert_eq!(
        b.table.get("s1").await.unwrap().state,
        SessionState::Crashed
    );
    assert_eq!(
        b.table.get("s1").await.unwrap().last_error.as_deref(),
        Some("orchestrator_restart")
    );
    assert!(store.lease_of("s1").await.is_none(), "lease released");
}

/// Idle sweep on a leasing store: a foreign-owned fresh lease vetoes the
/// sweep (the owner sweeps); after expiry the sweeper claims + acts.
#[tokio::test]
async fn idle_sweep_respects_foreign_lease() {
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    let table = RoutingTable::with_store(store.clone());
    table
        .insert_new(SessionEntry::new_creating(test_spec("s1")))
        .await
        .unwrap();
    table
        .cas_transition(
            "s1",
            SessionState::Creating,
            SessionState::Running,
            Some(handle_for("s1", 1)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    // Ancient activity → idle candidate on any replica; the lease gate is
    // what's tested. Backdate BOTH clocks explicitly: the durable store copy
    // (what a foreign replica's sweep reads) and the local atomic (what
    // list_idle_candidates' idle_ms() consults — merge_stored is monotonic
    // upward-only, so the durable 1ms never propagates BACKWARD into a cache
    // entry born "now"; relying on ≥1ms of wall-clock elapsing between
    // insert and sweep made this test timing-fragile).
    store.touch_activity("s1", 1).await.unwrap();
    {
        let e = table.get("s1").await.unwrap();
        e.last_activity_ms
            .store(1, std::sync::atomic::Ordering::Relaxed);
        e.last_flushed_activity_ms
            .store(1, std::sync::atomic::Ordering::Relaxed);
    }

    // Foreign fresh lease: B must skip.
    store
        .set_lease("s1", "rep-a", Utc::now() + chrono::Duration::seconds(60))
        .await;
    sweep_once(
        &table,
        runner.as_ref(),
        None,
        Duration::from_secs(0),
        Some(&identity("rep-b", Duration::from_secs(30))),
    )
    .await;
    assert_eq!(runner.count_calls("stop:").await, 0);
    assert_eq!(table.get("s1").await.unwrap().state, SessionState::Running);

    // Expired lease: B claims and hibernates.
    store
        .set_lease("s1", "rep-a", Utc::now() - chrono::Duration::seconds(1))
        .await;
    sweep_once(
        &table,
        runner.as_ref(),
        None,
        Duration::from_secs(0),
        Some(&identity("rep-b", Duration::from_secs(30))),
    )
    .await;
    assert_eq!(runner.count_calls("stop:").await, 1);
    assert_eq!(
        table.get("s1").await.unwrap().state,
        SessionState::Hibernating
    );
    assert_eq!(store.lease_of("s1").await.unwrap().owner_replica, "rep-b");
}

/// Two concurrent deep-archive sweeps race for one Hibernating session:
/// exactly ONE export runs (the lease claim prevents concurrent tars into
/// the same deterministic layout) and exactly one destroy follows.
#[tokio::test]
async fn deep_archive_race_exactly_one_export_and_destroy() {
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    let table = RoutingTable::with_store(store.clone());
    table
        .insert_new(SessionEntry::new_creating(test_spec("s1")))
        .await
        .unwrap();
    table
        .cas_transition(
            "s1",
            SessionState::Creating,
            SessionState::Running,
            Some(handle_for("s1", 1)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    table
        .cas_transition(
            "s1",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    // Backdate updated_at past the deep-archive timeout (candidates are
    // updated_at-based so they survive restarts).
    let mut e = store.get("s1").await.unwrap().unwrap().entry;
    e.updated_at = Utc::now() - chrono::Duration::seconds(600);
    store.force_update(&e).await.unwrap();

    let archive = Arc::new(FakeArchive {
        calls: Mutex::new(Vec::new()),
    });
    let deep = DeepArchive {
        store: archive.clone(),
        timeout: Duration::from_secs(60),
        docker_bin: "docker".into(),
    };
    let (t1, t2) = (table.clone(), table.clone());
    let (r1, r2) = (runner.clone(), runner.clone());
    let id_a = identity("rep-a", Duration::from_secs(30));
    let id_b = identity("rep-b", Duration::from_secs(30));
    tokio::join!(
        sweep_once(
            &t1,
            r1.as_ref(),
            Some(&deep),
            Duration::from_secs(3600),
            Some(&id_a)
        ),
        sweep_once(
            &t2,
            r2.as_ref(),
            Some(&deep),
            Duration::from_secs(3600),
            Some(&id_b)
        ),
    );

    assert_eq!(
        *archive.calls.lock().await,
        vec!["archive:s1"],
        "exactly one export"
    );
    assert_eq!(
        runner.count_calls("destroy:").await,
        1,
        "exactly one destroy"
    );
    let row = store.get("s1").await.unwrap().unwrap();
    assert!(row.entry.archived.is_some(), "marker set");
    assert!(row.entry.handle.is_none(), "stale handle cleared");
}

// ─── MVS4-B: tenant quotas across replicas ──────────────────────────────────

/// Two replicas, ONE shared store, same tenant, `max_concurrent_sessions=3`:
/// 8 interleaved creates must yield EXACTLY 3 Running sessions and 5
/// `QuotaExceeded{ConcurrentSessions}` rejections — the store arbitrates
/// count+insert as one unit, so no replica-pair race can overshoot the cap.
#[tokio::test]
async fn tenant_quota_concurrent_creates_exactly_max_across_replicas() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    ensure_secret_env();

    let quota_cfg = |id: &str| {
        let mut c = lease_cfg(dir.path(), id, 30);
        let mut q = QuotaConfig::default();
        q.max_concurrent_sessions = Some(3);
        c.quotas_tenants.insert("acme".into(), q);
        c
    };
    let a = OrchestratorState::with_session_store(
        quota_cfg("rep-a"),
        runner.clone(),
        None,
        store.clone(),
    )
    .await
    .unwrap();
    let b = OrchestratorState::with_session_store(
        quota_cfg("rep-b"),
        runner.clone(),
        None,
        store.clone(),
    )
    .await
    .unwrap();

    let mut joins = Vec::new();
    for i in 0..8 {
        let st = if i % 2 == 0 { a.clone() } else { b.clone() };
        joins.push(tokio::spawn(async move {
            st.create_session_for_tenant(Some(format!("q{i}")), "acme", vec![])
                .await
        }));
    }
    let results: Vec<_> = futures::future::join_all(joins)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    let created: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    let rejected: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert_eq!(created.len(), 3, "exactly max_concurrent winners");
    assert!(created.iter().all(|s| s.state == SessionState::Running));
    assert_eq!(rejected.len(), 5);
    for e in rejected {
        match e {
            OrchestratorError::QuotaExceeded {
                reason,
                tenant,
                limit,
                current,
                retry_after_secs,
                ..
            } => {
                assert_eq!(*reason, QuotaReason::ConcurrentSessions);
                assert_eq!(tenant, "acme");
                assert_eq!(*limit, 3);
                assert!(*current >= 3);
                assert!(
                    retry_after_secs.is_none(),
                    "concurrency rejects carry no Retry-After"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // A different tenant is untouched by acme's cap…
    let other = a
        .create_session_for_tenant(Some("other1".into()), "globex", vec![])
        .await
        .unwrap();
    assert_eq!(other.state, SessionState::Running);
    // …and an untagged session lands in the "default" bucket (no quota
    // configured for it here → unlimited).
    let untagged = a
        .create_session(Some("untagged".into()), vec![])
        .await
        .unwrap();
    assert_eq!(untagged.tenant_id, "");

    // Destroy frees a slot: the next acme create succeeds again.
    let victim = &created[0].session_id;
    a.destroy_session(victim).await.unwrap();
    let refill = a
        .create_session_for_tenant(Some("q-refill".into()), "acme", vec![])
        .await;
    assert!(refill.is_ok(), "slot freed by destroy");
}

/// Per-tenant create-rate buckets are per-replica (documented approximation):
/// exhausting the rate on A leaves B's bucket full.
#[tokio::test]
async fn create_rate_is_per_replica_approximation() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemLeaseStore::new();
    let runner = FakeRunner::new();
    ensure_secret_env();
    let rate_cfg = |id: &str| {
        let mut c = lease_cfg(dir.path(), id, 30);
        let mut q = QuotaConfig::default();
        q.create_rate_per_min = Some(2);
        c.quotas_tenants.insert("bursty".into(), q);
        c
    };
    let a = OrchestratorState::with_session_store(
        rate_cfg("rep-a"),
        runner.clone(),
        None,
        store.clone(),
    )
    .await
    .unwrap();
    let b = OrchestratorState::with_session_store(
        rate_cfg("rep-b"),
        runner.clone(),
        None,
        store.clone(),
    )
    .await
    .unwrap();

    a.create_session_for_tenant(Some("r1".into()), "bursty", vec![])
        .await
        .unwrap();
    a.create_session_for_tenant(Some("r2".into()), "bursty", vec![])
        .await
        .unwrap();
    let err = a
        .create_session_for_tenant(Some("r3".into()), "bursty", vec![])
        .await
        .unwrap_err();
    match err {
        OrchestratorError::QuotaExceeded {
            reason,
            retry_after_secs,
            ..
        } => {
            assert_eq!(reason, QuotaReason::CreateRate);
            assert!(retry_after_secs.is_some_and(|s| s >= 1));
        }
        other => panic!("unexpected: {other:?}"),
    }
    // B has its own bucket — the same tenant can still create there.
    b.create_session_for_tenant(Some("r4".into()), "bursty", vec![])
        .await
        .unwrap();
}
