//! Routing-table persistence + startup reconcile tests (FakeRunner, no docker).

mod common;

use common::{test_spec, FakeRunner};

use oneai_orchestrator::fsm::{SessionEntry, SessionState};
use oneai_orchestrator::registry::RoutingTable;
use oneai_orchestrator::runner::ContainerHandle;

fn handle(name: &str, port: u16) -> ContainerHandle {
    ContainerHandle {
        container_id: format!("cid-{name}"),
        container_name: name.to_string(),
        host_port: port,
    }
}

/// Persist a table with one session in each interesting state, then reload
/// with `load_and_reconcile` and assert the outcome.
#[tokio::test]
async fn reconcile_after_restart() {
    let dir = tempfile::tempdir().unwrap();

    // ── Build + persist the pre-restart table ──
    let t = RoutingTable::new(dir.path());
    // alive: Running with a container that survives the restart
    t.insert_new(SessionEntry::new_creating(test_spec("alive")))
        .await
        .unwrap();
    t.cas_transition(
        "alive",
        SessionState::Creating,
        SessionState::Running,
        Some(handle("oneai-orch-alive", 43001)),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    // dead: Running but the container is gone
    t.insert_new(SessionEntry::new_creating(test_spec("dead")))
        .await
        .unwrap();
    t.cas_transition(
        "dead",
        SessionState::Creating,
        SessionState::Running,
        Some(handle("oneai-orch-dead", 43002)),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    // hibernated: stopped by design — must stay Hibernating
    t.insert_new(SessionEntry::new_creating(test_spec("bernie")))
        .await
        .unwrap();
    t.cas_transition(
        "bernie",
        SessionState::Creating,
        SessionState::Running,
        Some(handle("oneai-orch-bernie", 43003)),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    t.cas_transition(
        "bernie",
        SessionState::Running,
        SessionState::Hibernating,
        None,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    // failed: terminal-ish, kept for API visibility
    t.insert_new(SessionEntry::new_creating(test_spec("failio")))
        .await
        .unwrap();
    t.cas_transition(
        "failio",
        SessionState::Creating,
        SessionState::Failed,
        None,
        Some("spawn exploded".into()),
    )
    .await
    .unwrap()
    .unwrap();

    // ── Simulate orchestrator restart ──
    let runner = FakeRunner::new();
    // Only the "alive" container survived (e.g. orchestrator process died,
    // docker kept running).
    runner.mark_running("oneai-orch-alive").await;

    let t2 = RoutingTable::load_and_reconcile(dir.path(), runner.as_ref())
        .await
        .expect("reconcile");

    let alive = t2.get("alive").await.unwrap();
    assert_eq!(alive.state, SessionState::Running); // re-mounted
    assert_eq!(alive.handle.as_ref().unwrap().host_port, 43001);

    let dead = t2.get("dead").await.unwrap();
    assert_eq!(dead.state, SessionState::Crashed);
    assert_eq!(dead.last_error.as_deref(), Some("orchestrator_restart"));

    let bernie = t2.get("bernie").await.unwrap();
    assert_eq!(bernie.state, SessionState::Hibernating); // untouched

    let failio = t2.get("failio").await.unwrap();
    assert_eq!(failio.state, SessionState::Failed); // untouched
    assert_eq!(failio.last_error.as_deref(), Some("spawn exploded"));

    // Reconciled state is persisted again.
    let t3 = RoutingTable::load(dir.path()).await.unwrap();
    assert_eq!(t3.get("dead").await.unwrap().state, SessionState::Crashed);
}

#[tokio::test]
async fn reconcile_creating_without_handle_crashes() {
    let dir = tempfile::tempdir().unwrap();
    let t = RoutingTable::new(dir.path());
    // Mid-spawn crash: persisted as Creating with no handle.
    t.insert_new(SessionEntry::new_creating(test_spec("midspawn")))
        .await
        .unwrap();
    t.persist().await.unwrap(); // insert_new doesn't auto-persist

    let runner = FakeRunner::new();
    let t2 = RoutingTable::load_and_reconcile(dir.path(), runner.as_ref())
        .await
        .unwrap();
    let e = t2.get("midspawn").await.unwrap();
    assert_eq!(e.state, SessionState::Crashed);
    assert_eq!(e.last_error.as_deref(), Some("orchestrator_restart"));
}

#[tokio::test]
async fn crashed_session_resumes_via_state() {
    // After reconcile marks a session Crashed, OrchestratorState::resume
    // must re-spawn it onto the same volumes (G3, full-circle check).
    let dir = tempfile::tempdir().unwrap();
    let t = RoutingTable::new(dir.path());
    t.insert_new(SessionEntry::new_creating(test_spec("zombie")))
        .await
        .unwrap();
    t.cas_transition(
        "zombie",
        SessionState::Creating,
        SessionState::Running,
        Some(handle("oneai-orch-zombie", 43100)),
        None,
    )
    .await
    .unwrap()
    .unwrap();

    let runner = FakeRunner::new(); // container did NOT survive
    let _t2 = RoutingTable::load_and_reconcile(dir.path(), runner.as_ref())
        .await
        .unwrap();

    common::ensure_secret_env();
    let st = oneai_orchestrator::server::OrchestratorState::new(
        common::test_config(dir.path()),
        runner.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        st.table.get("zombie").await.unwrap().state,
        SessionState::Crashed
    );

    st.resume_session("zombie").await.expect("resume");
    let e = st.table.get("zombie").await.unwrap();
    assert_eq!(e.state, SessionState::Running);
    assert_eq!(runner.count_calls("spawn:").await, 1);
    assert_eq!(e.spec.state_volume, "oneai-orch-zombie-state");
}
