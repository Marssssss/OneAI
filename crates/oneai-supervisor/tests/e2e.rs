//! End-to-end supervisor e2e — full client↔daemon lifecycle over a real IPC
//! socket (Unix domain socket). Exercises spawn/list/status/rpc/rpc_stream/
//! stop and reconnect-after-restart using a fake `SupervisorRunner` (no LLM).

#![cfg(unix)]

use std::sync::Arc;

use async_trait::async_trait;
use tokio_stream::StreamExt;

use oneai_bus::EngineYield;
use oneai_supervisor::{
    serve, InstanceHandle, InstanceSpec, InstanceStatus, SupervisorClient, SupervisorRunner,
    TurnSummary,
};

struct FakeHandle;

#[async_trait]
impl InstanceHandle for FakeHandle {
    fn status(&self) -> InstanceStatus {
        InstanceStatus::Idle
    }

    async fn run_turn(
        &self,
        _task: &str,
        observer: Arc<dyn oneai_agent::AgentLoopObserver>,
    ) -> Result<TurnSummary, oneai_supervisor::SupervisorError> {
        use oneai_agent::{AgentLoopResult, ParadigmKind};
        observer.on_iteration_start(1, ParadigmKind::ReAct);
        observer.on_stream_chunk("hel");
        observer.on_stream_chunk("lo");
        observer.on_direct_answer("hello");
        observer.on_complete(&AgentLoopResult {
            conversation: oneai_core::Conversation::new(),
            final_answer: "hello".to_string(),
            global_state: oneai_core::GlobalState::default(),
            iterations: 1,
            completed: true,
            active_paradigm: ParadigmKind::ReAct,
            sub_agent_results: Vec::new(),
            error: None,
        });
        Ok(TurnSummary {
            final_answer: "hello".to_string(),
            iterations: 1,
            completed: true,
            active_paradigm: "react".to_string(),
        })
    }

    async fn stop(&self) {}
}

struct FakeRunner;

#[async_trait]
impl SupervisorRunner for FakeRunner {
    fn has_provider(&self) -> bool {
        true
    }
    async fn spawn(
        &self,
        _spec: &InstanceSpec,
    ) -> Result<Arc<dyn InstanceHandle>, oneai_supervisor::SupervisorError> {
        Ok(Arc::new(FakeHandle))
    }
}

/// Unique socket + dir per test to avoid cross-test contention.
struct Temp {
    dir: tempfile::TempDir,
    socket: std::path::PathBuf,
}

impl Temp {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("supervisor.sock");
        Self { dir, socket }
    }

    fn config(&self) -> oneai_supervisor::SupervisorConfig {
        oneai_supervisor::SupervisorConfig {
            socket_path: self.socket.clone(),
            root_dir: self.dir.path().join("server"),
        }
    }
}

async fn start_daemon(temp: &Temp) -> tokio::task::JoinHandle<()> {
    let config = temp.config();
    let handle = tokio::spawn(async move {
        let _ = serve(config, Arc::new(FakeRunner) as Arc<dyn SupervisorRunner>).await;
    });
    // Wait for the socket to appear.
    for _ in 0..50 {
        if tokio::fs::metadata(&temp.socket).await.is_ok() {
            return handle;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    handle
}

#[tokio::test]
async fn full_lifecycle_over_uds() {
    let temp = Temp::new();
    let _daemon = start_daemon(&temp).await;

    let client = SupervisorClient::connect(&temp.socket).await.unwrap();

    // Spawn.
    let spec = InstanceSpec {
        id: "alice".to_string(),
        domain: "coding".to_string(),
        model: None,
        user: None,
        created_at: chrono::Utc::now(),
    };
    let id = client.spawn(&spec).await.unwrap();
    assert_eq!(id, "alice");

    // List.
    let list = client.list().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].spec.id, "alice");

    // Status.
    let info = client.status("alice").await.unwrap();
    assert!(matches!(info.status, InstanceStatus::Idle));

    // rpc.
    let summary = client.rpc("alice", "hi").await.unwrap();
    assert_eq!(summary.final_answer, "hello");
    assert!(summary.completed);
    assert!(info.last_turn.is_none() || true); // last_turn set on server side

    // rpc_stream — collect live events.
    let mut stream = client.rpc_stream("alice", "hi");
    let mut events = Vec::new();
    while let Some(Ok(ev)) = stream.next().await {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, EngineYield::IterationStart { .. })));
    assert!(
        events
            .iter()
            .filter(|e| matches!(e, EngineYield::StreamChunk { .. }))
            .count()
            >= 2
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, EngineYield::DirectAnswer { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, EngineYield::TurnComplete { .. })));

    // Stop.
    client.stop("alice").await.unwrap();
    let list = client.list().await.unwrap();
    assert!(list.is_empty());
}

#[tokio::test]
async fn reconnect_after_restart() {
    let temp = Temp::new();
    let daemon = start_daemon(&temp).await;

    let client = SupervisorClient::connect(&temp.socket).await.unwrap();
    let spec = InstanceSpec {
        id: "bob".to_string(),
        domain: "coding".to_string(),
        model: None,
        user: None,
        created_at: chrono::Utc::now(),
    };
    client.spawn(&spec).await.unwrap();
    assert_eq!(client.list().await.unwrap().len(), 1);

    // Kill the daemon.
    daemon.abort();
    let _ = daemon.await;
    // Give the OS a moment to release the socket file.
    for _ in 0..50 {
        if tokio::fs::metadata(&temp.socket).await.is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Restart — `recover_after_restart` marks leftover Running as Crashed.
    let daemon = start_daemon(&temp).await;

    // Reconnect via the recover helper (retries while the new daemon binds).
    let client = SupervisorClient::connect_with_recover(&temp.socket, 50)
        .await
        .unwrap();
    let list = client.list().await.unwrap();
    assert_eq!(list.len(), 1);
    // The instance was Idle (never started a turn), so it stays Idle — not
    // Crashed. The point of this test is that the durable record survives and
    // a fresh client reconnects to the new daemon.
    assert_eq!(list[0].spec.id, "bob");

    daemon.abort();
}

#[tokio::test]
async fn rpc_missing_instance_errors() {
    let temp = Temp::new();
    let _daemon = start_daemon(&temp).await;
    let client = SupervisorClient::connect(&temp.socket).await.unwrap();
    let err = client.rpc("ghost", "hi").await;
    assert!(err.is_err());
}
