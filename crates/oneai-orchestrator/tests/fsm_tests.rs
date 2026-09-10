//! FSM scenario tests against `OrchestratorState` with the in-memory
//! FakeRunner — no docker required.

mod common;

use std::sync::atomic::Ordering;

use common::{test_spec, test_state};

use oneai_orchestrator::error::OrchestratorError;
use oneai_orchestrator::fsm::{SessionEntry, SessionState};
use oneai_orchestrator::runner::ContainerRunner as _;

#[tokio::test]
async fn create_session_happy_path() {
    let dir = tempfile::tempdir().unwrap();
    let (st, runner) = test_state(dir.path()).await;

    let snap = st
        .create_session(Some("s1".into()), vec![])
        .await
        .expect("create");
    assert_eq!(snap.session_id, "s1");
    assert_eq!(snap.state, SessionState::Running);
    assert!(snap.host_port.unwrap() >= 41000);

    let log = runner.call_log().await;
    assert!(log.iter().any(|c| c == "spawn:s1"));
    // Entry persisted to disk as Running.
    assert!(dir.path().join("sessions.json").exists());
    let on_disk = oneai_orchestrator::RoutingTable::load(dir.path())
        .await
        .unwrap();
    assert_eq!(
        on_disk.get("s1").await.unwrap().state,
        SessionState::Running
    );
}

#[tokio::test]
async fn create_session_generated_id() {
    let dir = tempfile::tempdir().unwrap();
    let (st, _runner) = test_state(dir.path()).await;
    let snap = st.create_session(None, vec![]).await.expect("create");
    assert_eq!(snap.session_id.len(), 32); // uuid simple
    assert_eq!(snap.state, SessionState::Running);
}

#[tokio::test]
async fn create_session_invalid_id() {
    let dir = tempfile::tempdir().unwrap();
    let (st, _runner) = test_state(dir.path()).await;
    let err = st.create_session(Some("bad id!".into()), vec![]).await;
    assert!(matches!(err, Err(OrchestratorError::InvalidSessionId(_))));
}

#[tokio::test]
async fn create_session_duplicate_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let (st, _runner) = test_state(dir.path()).await;
    st.create_session(Some("dup".into()), vec![]).await.unwrap();
    let err = st.create_session(Some("dup".into()), vec![]).await;
    assert!(matches!(err, Err(OrchestratorError::AlreadyExists(_))));
}

#[tokio::test]
async fn create_session_spawn_failure_marks_failed() {
    let dir = tempfile::tempdir().unwrap();
    let (st, runner) = test_state(dir.path()).await;
    runner.spawn_should_fail.store(true, Ordering::Relaxed);
    let err = st.create_session(Some("f1".into()), vec![]).await;
    assert!(matches!(err, Err(OrchestratorError::Runner(_))));
    let entry = st.table.get("f1").await.unwrap();
    assert_eq!(entry.state, SessionState::Failed);
    assert!(entry.last_error.as_deref().unwrap().contains("fake spawn"));
}

#[tokio::test]
async fn hibernate_then_resume_restarts_container() {
    let dir = tempfile::tempdir().unwrap();
    let (st, runner) = test_state(dir.path()).await;
    st.create_session(Some("h1".into()), vec![]).await.unwrap();

    // Simulate the idle sweep: CAS Running → Hibernating + stop.
    st.table
        .cas_transition(
            "h1",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let entry = st.table.get("h1").await.unwrap();
    runner.stop(entry.handle.as_ref().unwrap()).await.unwrap();

    // Resume: same container restarted (NOT re-spawned).
    st.resume_session("h1").await.expect("resume");
    let entry = st.table.get("h1").await.unwrap();
    assert_eq!(entry.state, SessionState::Running);
    assert_eq!(runner.count_calls("start:").await, 1);
    assert_eq!(runner.count_calls("spawn:").await, 1); // still just the create
}

#[tokio::test]
async fn crashed_then_resume_respawns_on_same_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let (st, runner) = test_state(dir.path()).await;
    st.create_session(Some("c1".into()), vec![]).await.unwrap();
    let volumes = {
        let e = st.table.get("c1").await.unwrap();
        (e.spec.state_volume.clone(), e.spec.workspace_volume.clone())
    };

    // Simulate container death + orchestrator noticing.
    runner.kill("oneai-orch-c1").await;
    st.table
        .cas_transition(
            "c1",
            SessionState::Running,
            SessionState::Crashed,
            None,
            Some("killed".into()),
        )
        .await
        .unwrap()
        .unwrap();

    st.resume_session("c1").await.expect("resume");
    let entry = st.table.get("c1").await.unwrap();
    assert_eq!(entry.state, SessionState::Running);
    // Crash path re-spawns (new container, same volumes — G3).
    assert_eq!(runner.count_calls("spawn:").await, 2);
    assert_eq!(&entry.spec.state_volume, &volumes.0);
    assert_eq!(&entry.spec.workspace_volume, &volumes.1);
}

#[tokio::test]
async fn concurrent_resume_operates_container_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let (st, runner) = test_state(dir.path()).await;
    st.create_session(Some("r1".into()), vec![]).await.unwrap();
    st.table
        .cas_transition(
            "r1",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    // 10 concurrent resume attempts — CAS admits exactly one operator.
    let mut joins = Vec::new();
    for _ in 0..10 {
        let st = st.clone();
        joins.push(tokio::spawn(async move { st.resume_session("r1").await }));
    }
    for j in joins {
        j.await.unwrap().unwrap();
    }
    assert_eq!(runner.count_calls("start:").await, 1);
    assert_eq!(
        st.table.get("r1").await.unwrap().state,
        SessionState::Running
    );
}

#[tokio::test]
async fn destroy_removes_entry_and_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let (st, runner) = test_state(dir.path()).await;
    st.create_session(Some("d1".into()), vec![]).await.unwrap();

    st.destroy_session("d1").await.expect("destroy");
    assert!(st.table.get("d1").await.is_none());
    let log = runner.call_log().await;
    assert!(log.iter().any(|c| c == "destroy:oneai-orch-d1:true"));
    // Tombstone also cleared from disk.
    let on_disk = oneai_orchestrator::RoutingTable::load(dir.path())
        .await
        .unwrap();
    assert!(on_disk.get("d1").await.is_none());

    // Idempotency: destroying an unknown session is NotFound.
    let err = st.destroy_session("d1").await;
    assert!(matches!(err, Err(OrchestratorError::NotFound(_))));
}

#[tokio::test]
async fn destroy_while_hibernating() {
    let dir = tempfile::tempdir().unwrap();
    let (st, _runner) = test_state(dir.path()).await;
    st.create_session(Some("d2".into()), vec![]).await.unwrap();
    st.table
        .cas_transition(
            "d2",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    st.destroy_session("d2")
        .await
        .expect("destroy from Hibernating");
    assert!(st.table.get("d2").await.is_none());
}

#[tokio::test]
async fn wait_until_running_times_out_for_stuck_creating() {
    let dir = tempfile::tempdir().unwrap();
    let (st, _runner) = test_state(dir.path()).await;
    // Manually park a session in Creating (spawn never completes).
    st.table
        .insert_new(SessionEntry::new_creating(test_spec("stuck")))
        .await
        .unwrap();
    let err = st
        .wait_until_running("stuck", std::time::Duration::from_millis(1200))
        .await;
    assert!(matches!(err, Err(OrchestratorError::ResumeTimeout(_))));
}

#[tokio::test]
async fn wait_until_running_returns_immediately_when_running() {
    let dir = tempfile::tempdir().unwrap();
    let (st, _runner) = test_state(dir.path()).await;
    st.create_session(Some("w1".into()), vec![]).await.unwrap();
    let entry = st
        .wait_until_running("w1", std::time::Duration::from_secs(1))
        .await
        .expect("running");
    assert_eq!(entry.state, SessionState::Running);
}

#[tokio::test]
async fn running_with_dead_upstream_detected_and_resumed() {
    // FakeRunner health is state-based (no TCP), so the session reaches
    // Running even though nothing listens on its fake port — exactly the
    // `docker kill` situation. check_upstream_liveness must catch it via the
    // real TCP probe, then resume_session must bring it back.
    let dir = tempfile::tempdir().unwrap();
    let (st, _runner) = test_state(dir.path()).await;
    st.create_session(Some("k1".into()), vec![]).await.unwrap();
    assert_eq!(
        st.table.get("k1").await.unwrap().state,
        SessionState::Running
    );

    let entry = st.check_upstream_liveness("k1").await.unwrap();
    assert_eq!(entry.state, SessionState::Crashed);
    assert!(entry
        .last_error
        .as_deref()
        .unwrap()
        .contains("upstream unreachable"));

    // And the crash is resumable (new container on same volumes).
    st.resume_session("k1")
        .await
        .expect("resume after detected crash");
    assert_eq!(
        st.table.get("k1").await.unwrap().state,
        SessionState::Running
    );
}

#[tokio::test]
async fn resume_notifies_waiters() {
    let dir = tempfile::tempdir().unwrap();
    let (st, _runner) = test_state(dir.path()).await;
    st.create_session(Some("n1".into()), vec![]).await.unwrap();
    st.table
        .cas_transition(
            "n1",
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    // Waiter blocks until a concurrent resume lands Running.
    let st_w = st.clone();
    let waiter = tokio::spawn(async move {
        st_w.wait_until_running("n1", std::time::Duration::from_secs(5))
            .await
    });
    // Give the waiter a moment to park, then resume.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    st.resume_session("n1").await.unwrap();
    let entry = waiter.await.unwrap().expect("waiter sees Running");
    assert_eq!(entry.state, SessionState::Running);
}
