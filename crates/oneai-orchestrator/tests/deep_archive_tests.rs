//! Deep-hibernation volume archive tests (MVS3-C) — FakeRunner +
//! FakeArchiveStore, no docker required.
//!
//! Covers the sweep tier (export → claim → destroy), the data-loss red line
//! (archive failure keeps local volumes), the resume tier (restore → spawn →
//! marker release), misconfiguration surfacing, and concurrent-claim
//! idempotency.

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use common::{test_spec, FakeRunner};
use oneai_orchestrator::archive::{
    ArchiveManifest, DeepArchive, VolumeArchive, VolumeArchiveStore,
};
use oneai_orchestrator::error::{OrchestratorError, Result};
use oneai_orchestrator::fsm::{SessionEntry, SessionState};
use oneai_orchestrator::idle::sweep_once;
use oneai_orchestrator::registry::RoutingTable;
use oneai_orchestrator::runner::ContainerRunner as _;
use oneai_orchestrator::server::OrchestratorState;

/// In-memory `VolumeArchiveStore`: call log + failure injection.
struct FakeArchiveStore {
    calls: Mutex<Vec<String>>,
    manifests: Mutex<HashMap<String, ArchiveManifest>>,
    fail_archive: AtomicBool,
    fail_restore: AtomicBool,
}

impl FakeArchiveStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            manifests: Mutex::new(HashMap::new()),
            fail_archive: AtomicBool::new(false),
            fail_restore: AtomicBool::new(false),
        })
    }

    async fn call_log(&self) -> Vec<String> {
        self.calls.lock().await.clone()
    }

    async fn count_calls(&self, prefix: &str) -> usize {
        self.calls
            .lock()
            .await
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

#[async_trait]
impl VolumeArchiveStore for FakeArchiveStore {
    async fn archive(
        &self,
        session_id: &str,
        volume_names: &[String],
        _docker_bin: &str,
    ) -> Result<ArchiveManifest> {
        self.calls
            .lock()
            .await
            .push(format!("archive:{session_id}"));
        if self.fail_archive.load(Ordering::Relaxed) {
            return Err(OrchestratorError::Runner("fake archive failure".into()));
        }
        let manifest = ArchiveManifest {
            session_id: session_id.to_string(),
            volumes: volume_names
                .iter()
                .map(|v| VolumeArchive {
                    volume_name: v.clone(),
                    archive_file: format!("{session_id}/{v}.tar.gz"),
                    size_bytes: 1,
                })
                .collect(),
            archived_at: "2026-09-13T00:00:00+00:00".into(),
        };
        self.manifests
            .lock()
            .await
            .insert(session_id.to_string(), manifest.clone());
        Ok(manifest)
    }

    async fn restore(&self, manifest: &ArchiveManifest, _docker_bin: &str) -> Result<()> {
        self.calls
            .lock()
            .await
            .push(format!("restore:{}", manifest.session_id));
        if self.fail_restore.load(Ordering::Relaxed) {
            return Err(OrchestratorError::Runner("fake restore failure".into()));
        }
        Ok(())
    }

    async fn has_archive(&self, session_id: &str) -> bool {
        self.manifests.lock().await.contains_key(session_id)
    }

    async fn remove(&self, session_id: &str) -> Result<()> {
        self.calls.lock().await.push(format!("remove:{session_id}"));
        self.manifests.lock().await.remove(session_id);
        Ok(())
    }
}

/// Bring a session to Hibernating through the REAL idle sweep (Running →
/// stop → Hibernating), the same path production takes.
async fn hibernate(table: &RoutingTable, runner: &FakeRunner, id: &str) {
    table
        .insert_new(SessionEntry::new_creating(test_spec(id)))
        .await
        .unwrap();
    table
        .cas_transition(
            id,
            SessionState::Creating,
            SessionState::Running,
            Some(oneai_orchestrator::runner::ContainerHandle {
                container_id: "cid".into(),
                container_name: format!("oneai-orch-{id}"),
                host_port: 41000,
            }),
            None,
        )
        .await
        .unwrap();
    // idle_ms > 0 with timeout 0 → candidate; the sweep CASes + stops.
    sweep_once(table, runner, None, Duration::from_secs(0)).await;
    assert_eq!(
        table.get(id).await.unwrap().state,
        SessionState::Hibernating
    );
}

fn deep(store: &Arc<FakeArchiveStore>, timeout: Duration) -> DeepArchive {
    DeepArchive {
        store: store.clone(),
        timeout,
        docker_bin: "docker".into(),
    }
}

#[tokio::test]
async fn sweep_exports_claims_and_destroys() {
    let dir = tempfile::tempdir().unwrap();
    let table = RoutingTable::new(dir.path());
    let runner = FakeRunner::new();
    let store = FakeArchiveStore::new();
    hibernate(&table, &runner, "s1").await;

    sweep_once(
        &table,
        runner.as_ref(),
        Some(&deep(&store, Duration::from_secs(0))),
        Duration::from_secs(3600),
    )
    .await;

    let entry = table.get("s1").await.unwrap();
    assert_eq!(entry.state, SessionState::Hibernating, "state unchanged");
    let manifest = entry.archived.as_ref().expect("archived marker set");
    assert_eq!(manifest.volumes.len(), 2, "state + workspace volumes");
    assert!(entry.handle.is_none(), "stale handle cleared");

    let log = store.call_log().await;
    assert_eq!(log, vec!["archive:s1"]);
    let calls = runner.call_log().await;
    assert!(
        calls.iter().any(|c| c == "destroy:oneai-orch-s1:true"),
        "volumes removed only after a confirmed archive: {calls:?}"
    );
    // destroy ran AFTER the archive call (ordering is the red line).
    let arch_idx = calls.iter().position(|c| c == "stop:oneai-orch-s1");
    let dest_idx = calls.iter().position(|c| c == "destroy:oneai-orch-s1:true");
    assert!(arch_idx.is_some() && dest_idx.unwrap() > arch_idx.unwrap());
    // Marker persisted to sessions.json.
    let on_disk = RoutingTable::load(dir.path()).await.unwrap();
    assert!(on_disk.get("s1").await.unwrap().archived.is_some());
}

#[tokio::test]
async fn archive_failure_keeps_volumes_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let table = RoutingTable::new(dir.path());
    let runner = FakeRunner::new();
    let store = FakeArchiveStore::new();
    hibernate(&table, &runner, "s1").await;

    store.fail_archive.store(true, Ordering::Relaxed);
    sweep_once(
        &table,
        runner.as_ref(),
        Some(&deep(&store, Duration::from_secs(0))),
        Duration::from_secs(3600),
    )
    .await;

    // RED LINE: nothing destroyed, no marker, diagnostics recorded.
    let entry = table.get("s1").await.unwrap();
    assert_eq!(entry.state, SessionState::Hibernating);
    assert!(entry.archived.is_none());
    assert!(entry.handle.is_some(), "container handle kept");
    assert!(
        entry
            .last_error
            .as_deref()
            .unwrap()
            .contains("deep-archive failed"),
        "{:?}",
        entry.last_error
    );
    assert_eq!(runner.count_calls("destroy:").await, 0);

    // Next sweep with a healthy store completes the archive.
    store.fail_archive.store(false, Ordering::Relaxed);
    sweep_once(
        &table,
        runner.as_ref(),
        Some(&deep(&store, Duration::from_secs(0))),
        Duration::from_secs(3600),
    )
    .await;
    assert!(table.get("s1").await.unwrap().archived.is_some());
    assert_eq!(runner.count_calls("destroy:").await, 1);
}

#[tokio::test]
async fn future_timeout_is_not_a_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let table = RoutingTable::new(dir.path());
    let runner = FakeRunner::new();
    let store = FakeArchiveStore::new();
    hibernate(&table, &runner, "s1").await;

    // Deep timeout far in the future → the fresh Hibernating entry stays local.
    sweep_once(
        &table,
        runner.as_ref(),
        Some(&deep(&store, Duration::from_secs(86400))),
        Duration::from_secs(3600),
    )
    .await;
    assert!(table.get("s1").await.unwrap().archived.is_none());
    assert_eq!(runner.count_calls("destroy:").await, 0);
}

/// State wired with a FakeArchiveStore (config passes validation via a
/// dummy archive_dir; the injected store is what the server actually uses).
async fn archive_state(
    dir: &Path,
    store: Arc<FakeArchiveStore>,
) -> (Arc<OrchestratorState>, Arc<FakeRunner>) {
    common::ensure_secret_env();
    let mut config = common::test_config(dir);
    config.deep_archive_timeout_secs = 1;
    config.archive_dir = Some(dir.join("archive"));
    let runner = FakeRunner::new();
    let state = OrchestratorState::with_archive_store(config, runner.clone(), Some(store))
        .await
        .expect("state");
    (state, runner)
}

#[tokio::test]
async fn resume_restores_spawn_and_releases_marker() {
    let dir = tempfile::tempdir().unwrap();
    let store = FakeArchiveStore::new();
    let (st, runner) = archive_state(dir.path(), store.clone()).await;

    st.create_session(Some("s1".into()), vec![]).await.unwrap();
    // Hibernate + deep-archive through the real sweep.
    st.table
        .cas_transition(
            "s1",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap();
    let entry = st.table.get("s1").await.unwrap();
    if let Some(h) = &entry.handle {
        runner.stop(h).await.unwrap();
    }
    sweep_once(
        &st.table,
        runner.as_ref(),
        Some(&deep(&store, Duration::from_secs(0))),
        Duration::from_secs(3600),
    )
    .await;
    assert!(st.table.get("s1").await.unwrap().archived.is_some());

    // Resume: restore → spawn → Running → marker cleared + archive removed.
    st.resume_session("s1").await.expect("resume");
    let entry = st.table.get("s1").await.unwrap();
    assert_eq!(entry.state, SessionState::Running);
    assert!(entry.archived.is_none(), "marker released after success");
    assert!(entry.handle.is_some());
    let log = store.call_log().await;
    assert!(log.contains(&"restore:s1".to_string()), "{log:?}");
    assert!(log.contains(&"remove:s1".to_string()), "{log:?}");
    let calls = runner.call_log().await;
    // Archived entries have no handle → the resume must SPAWN (not start).
    assert!(calls.iter().any(|c| c == "spawn:s1"), "{calls:?}");
}

#[tokio::test]
async fn restore_failure_keeps_archive_and_marks_crashed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FakeArchiveStore::new();
    let (st, runner) = archive_state(dir.path(), store.clone()).await;

    st.create_session(Some("s1".into()), vec![]).await.unwrap();
    st.table
        .cas_transition(
            "s1",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap();
    let entry = st.table.get("s1").await.unwrap();
    if let Some(h) = &entry.handle {
        runner.stop(h).await.unwrap();
    }
    sweep_once(
        &st.table,
        runner.as_ref(),
        Some(&deep(&store, Duration::from_secs(0))),
        Duration::from_secs(3600),
    )
    .await;

    store.fail_restore.store(true, Ordering::Relaxed);
    let err = st.resume_session("s1").await.unwrap_err();
    assert!(err.to_string().contains("archive restore failed"), "{err}");
    let entry = st.table.get("s1").await.unwrap();
    assert_eq!(entry.state, SessionState::Crashed);
    assert!(
        entry.archived.is_some(),
        "marker survives a failed restore — the archive is still the source of truth"
    );
    assert_eq!(store.count_calls("remove:").await, 0, "archive NOT deleted");

    // A later resume with a healthy store recovers (Crashed + marker →
    // restore → spawn).
    store.fail_restore.store(false, Ordering::Relaxed);
    st.resume_session("s1").await.expect("second resume");
    let entry = st.table.get("s1").await.unwrap();
    assert_eq!(entry.state, SessionState::Running);
    assert!(entry.archived.is_none());
}

#[tokio::test]
async fn archived_without_store_surfaces_diagnostic() {
    // Entry carries the marker but the orchestrator lost its archive config
    // (e.g. archive_dir removed from the toml) → loud Crashed, never a
    // silent spawn on fresh empty volumes.
    let dir = tempfile::tempdir().unwrap();
    common::ensure_secret_env();
    let runner = FakeRunner::new();
    let st = OrchestratorState::new(common::test_config(dir.path()), runner.clone())
        .await
        .expect("state");
    st.table
        .insert_new(SessionEntry::new_creating(test_spec("s1")))
        .await
        .unwrap();
    st.table
        .cas_transition(
            "s1",
            SessionState::Creating,
            SessionState::Running,
            Some(oneai_orchestrator::runner::ContainerHandle {
                container_id: "cid".into(),
                container_name: "oneai-orch-s1".into(),
                host_port: 41000,
            }),
            None,
        )
        .await
        .unwrap();
    st.table
        .cas_transition(
            "s1",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap();
    st.table
        .cas_set_archived(
            "s1",
            Some(ArchiveManifest {
                session_id: "s1".into(),
                volumes: vec![],
                archived_at: "2026-09-13T00:00:00+00:00".into(),
            }),
        )
        .await
        .unwrap();

    let err = st.resume_session("s1").await.unwrap_err();
    assert!(err.to_string().contains("no archive store"), "{err}");
    let entry = st.table.get("s1").await.unwrap();
    assert_eq!(entry.state, SessionState::Crashed);
    assert!(entry.archived.is_some());
    assert_eq!(
        runner.count_calls("spawn:").await,
        0,
        "must NOT spawn empty"
    );
}

#[tokio::test]
async fn concurrent_sweeps_archive_once() {
    let dir = tempfile::tempdir().unwrap();
    let table = RoutingTable::new(dir.path());
    let runner = FakeRunner::new();
    let store = FakeArchiveStore::new();
    hibernate(&table, &runner, "s1").await;

    // Two racing deep-archive passes: the registry CAS admits one claimer;
    // destroy(remove_volumes) must run exactly once.
    let (t1, t2) = (table.clone(), table.clone());
    let (r1, r2) = (runner.clone(), runner.clone());
    let (s1, s2) = (store.clone(), store.clone());
    let pass = |t: RoutingTable, r: Arc<FakeRunner>, s: Arc<FakeArchiveStore>| async move {
        sweep_once(
            &t,
            r.as_ref(),
            Some(&deep(&s, Duration::from_secs(0))),
            Duration::from_secs(3600),
        )
        .await;
    };
    tokio::join!(pass(t1, r1, s1), pass(t2, r2, s2));

    assert_eq!(runner.count_calls("destroy:").await, 1);
    assert!(table.get("s1").await.unwrap().archived.is_some());
}

#[tokio::test]
async fn destroy_session_removes_archive_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = FakeArchiveStore::new();
    let (st, runner) = archive_state(dir.path(), store.clone()).await;

    st.create_session(Some("s1".into()), vec![]).await.unwrap();
    st.table
        .cas_transition(
            "s1",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap();
    let entry = st.table.get("s1").await.unwrap();
    if let Some(h) = &entry.handle {
        runner.stop(h).await.unwrap();
    }
    sweep_once(
        &st.table,
        runner.as_ref(),
        Some(&deep(&store, Duration::from_secs(0))),
        Duration::from_secs(3600),
    )
    .await;
    assert!(store.has_archive("s1").await);

    st.destroy_session("s1").await.expect("destroy");
    assert!(
        !store.has_archive("s1").await,
        "archive files die with the session"
    );
    assert!(store.call_log().await.contains(&"remove:s1".to_string()));
    // The archived entry had no handle — the synthetic-handle path must
    // still sweep the canonically-named volumes.
    let calls = runner.call_log().await;
    assert!(
        calls.iter().any(|c| c == "destroy:oneai-orch-s1:true"),
        "{calls:?}"
    );
}

#[tokio::test]
async fn config_rejects_deep_archive_without_idle_or_dir() {
    let mut c = common::test_config(Path::new("/tmp/oneai-orch-test-unused"));
    c.deep_archive_timeout_secs = 60;
    // no archive_dir → error
    assert!(c.validate().is_err());
    c.archive_dir = Some(Path::new("/srv/oneai-archive").to_path_buf());
    // test_config sets idle 3600 → ok
    assert!(c.validate().is_ok());
    c.idle_timeout_secs = 0;
    let err = c.validate().unwrap_err();
    assert!(err.to_string().contains("idle_timeout_secs"), "{err}");
}
