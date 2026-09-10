//! Real-docker smoke test — `#[ignore]`d by default (requires a local docker
//! daemon + the `oneai-engine:mvs1` image; run with
//! `cargo test -p oneai-orchestrator --test e2e_docker -- --ignored`).
//!
//! The full 10-session acceptance lives in `deploy/docker/mvs2_verify.mjs`;
//! this covers the single-container lifecycle through the real DockerRunner.

mod common;

use common::TEST_SECRET;
use oneai_orchestrator::docker::DockerRunner;
use oneai_orchestrator::fsm::SessionState;

#[tokio::test]
#[ignore = "requires local docker + oneai-engine:mvs1 image"]
async fn docker_runner_full_lifecycle() {
    common::ensure_secret_env();
    let dir = tempfile::tempdir().unwrap();

    let mut config = common::test_config(dir.path());
    if let Ok(img) = std::env::var("ONEAI_E2E_IMAGE") {
        config.image = img;
    }
    // Provider config (real LLM keys) is optional for the lifecycle smoke.
    let cfg_path = dirs::home_dir().map(|h| h.join(".oneai").join("config.toml"));
    config.provider_config = cfg_path.filter(|p| p.exists());
    // Engine boot can take a few seconds inside the container.
    config.resume_timeout_secs = 90;

    let runner = std::sync::Arc::new(DockerRunner::with_bin(
        &config.docker_bin,
        &config.container_bind_host,
    ));
    let st = oneai_orchestrator::server::OrchestratorState::new(config, runner.clone())
        .await
        .expect("state (is ONEAI_ORCHESTRATOR_SECRET set? see ensure_secret_env)");
    let _ = TEST_SECRET;

    let id = format!("e2e{}", std::process::id());
    let snap = st
        .create_session(Some(id.clone()), vec![])
        .await
        .expect("create_session with real docker");
    assert_eq!(snap.state, SessionState::Running);
    let port = snap.host_port.expect("dynamic host port");

    // Engine ws endpoint reachable through the published port.
    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("container engine ws reachable");
    use futures::{SinkExt, StreamExt};
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/list"}"#.into(),
    ))
    .await
    .unwrap();
    // Any response frame proves the engine speaks JSON-RPC.
    let _resp = tokio::time::timeout(std::time::Duration::from_secs(30), ws.next())
        .await
        .expect("engine response")
        .unwrap()
        .unwrap();
    ws.close(None).await.ok();

    // Hibernate → resume (stop/start, volumes preserved).
    use oneai_orchestrator::runner::ContainerRunner as _;
    st.table
        .cas_transition(
            &id,
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let handle = st.table.get(&id).await.unwrap().handle.clone().unwrap();
    runner.stop(&handle).await.unwrap();
    st.resume_session(&id).await.expect("resume from hibernate");
    assert_eq!(
        st.table.get(&id).await.unwrap().state,
        SessionState::Running
    );

    // Teardown.
    st.destroy_session(&id).await.expect("destroy");
    assert!(st.table.get(&id).await.is_none());
}
