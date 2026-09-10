//! Shared test helpers: in-memory `FakeRunner` + state factory.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Once};

use async_trait::async_trait;
use tokio::sync::Mutex;

use oneai_orchestrator::config::OrchestratorConfig;
use oneai_orchestrator::error::{OrchestratorError, Result};
use oneai_orchestrator::runner::{ContainerHandle, ContainerRunner, SessionSpec};
use oneai_orchestrator::server::OrchestratorState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeContainerState {
    Running,
    Stopped,
    Gone,
}

/// In-memory `ContainerRunner`: deterministic ports, killable containers,
/// full call log for assertions. No docker required.
pub struct FakeRunner {
    /// Call log: "spawn:<session_id>", "stop:<name>", "start:<name>",
    /// "health:<name>", "commit:<name>", "destroy:<name>:<volumes>".
    pub calls: Mutex<Vec<String>>,
    states: Mutex<HashMap<String, FakeContainerState>>,
    fixed_ports: Mutex<HashMap<String, u16>>,
    next_port: AtomicU16,
    /// When set, every spawn fails.
    pub spawn_should_fail: AtomicBool,
}

impl FakeRunner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            states: Mutex::new(HashMap::new()),
            fixed_ports: Mutex::new(HashMap::new()),
            next_port: AtomicU16::new(41000),
            spawn_should_fail: AtomicBool::new(false),
        })
    }

    /// Pin the host port spawn/start return for a session id (e.g. to point
    /// at a real echo ws server in proxy tests).
    pub async fn set_fixed_port(&self, session_id: &str, port: u16) {
        self.fixed_ports
            .lock()
            .await
            .insert(session_id.to_string(), port);
    }

    /// Externally kill a container (simulates `docker kill`).
    pub async fn kill(&self, container_name: &str) {
        self.states
            .lock()
            .await
            .insert(container_name.to_string(), FakeContainerState::Gone);
    }

    /// Mark a container name as running WITHOUT a spawn call (used to
    /// pre-populate state for registry reconcile tests).
    pub async fn mark_running(&self, container_name: &str) {
        self.states
            .lock()
            .await
            .insert(container_name.to_string(), FakeContainerState::Running);
    }

    pub async fn call_log(&self) -> Vec<String> {
        self.calls.lock().await.clone()
    }

    pub async fn count_calls(&self, prefix: &str) -> usize {
        self.calls
            .lock()
            .await
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }

    async fn session_id_of(name: &str) -> String {
        name.strip_prefix("oneai-orch-").unwrap_or(name).to_string()
    }

    async fn port_for(&self, session_id: &str) -> u16 {
        if let Some(p) = self.fixed_ports.lock().await.get(session_id) {
            return *p;
        }
        self.next_port.fetch_add(1, Ordering::Relaxed)
    }
}

#[async_trait]
impl ContainerRunner for FakeRunner {
    async fn spawn(&self, spec: &SessionSpec) -> Result<ContainerHandle> {
        self.calls
            .lock()
            .await
            .push(format!("spawn:{}", spec.session_id));
        if self.spawn_should_fail.load(Ordering::Relaxed) {
            return Err(OrchestratorError::Runner("fake spawn failure".into()));
        }
        let port = self.port_for(&spec.session_id).await;
        let name = spec.container_name();
        self.states
            .lock()
            .await
            .insert(name.clone(), FakeContainerState::Running);
        Ok(ContainerHandle {
            container_id: format!("fake-{name}"),
            container_name: name,
            host_port: port,
        })
    }

    async fn stop(&self, handle: &ContainerHandle) -> Result<()> {
        self.calls
            .lock()
            .await
            .push(format!("stop:{}", handle.container_name));
        self.states
            .lock()
            .await
            .insert(handle.container_name.clone(), FakeContainerState::Stopped);
        Ok(())
    }

    async fn start(&self, handle: &ContainerHandle) -> Result<ContainerHandle> {
        self.calls
            .lock()
            .await
            .push(format!("start:{}", handle.container_name));
        let session_id = Self::session_id_of(&handle.container_name).await;
        let port = self.port_for(&session_id).await;
        self.states
            .lock()
            .await
            .insert(handle.container_name.clone(), FakeContainerState::Running);
        Ok(ContainerHandle {
            container_id: handle.container_id.clone(),
            container_name: handle.container_name.clone(),
            host_port: port,
        })
    }

    async fn health(&self, handle: &ContainerHandle) -> Result<bool> {
        self.calls
            .lock()
            .await
            .push(format!("health:{}", handle.container_name));
        let st = self
            .states
            .lock()
            .await
            .get(&handle.container_name)
            .copied()
            .unwrap_or(FakeContainerState::Gone);
        Ok(st == FakeContainerState::Running)
    }

    async fn commit(&self, handle: &ContainerHandle, tag: &str) -> Result<()> {
        self.calls
            .lock()
            .await
            .push(format!("commit:{}:{tag}", handle.container_name));
        Ok(())
    }

    async fn destroy(&self, handle: &ContainerHandle, remove_volumes: bool) -> Result<()> {
        self.calls.lock().await.push(format!(
            "destroy:{}:{remove_volumes}",
            handle.container_name
        ));
        self.states
            .lock()
            .await
            .insert(handle.container_name.clone(), FakeContainerState::Gone);
        Ok(())
    }
}

/// Build a test spec.
pub fn test_spec(id: &str) -> SessionSpec {
    SessionSpec {
        session_id: id.into(),
        image: "fake-image:test".into(),
        state_volume: oneai_orchestrator::runner::state_volume_name(id),
        workspace_volume: oneai_orchestrator::runner::workspace_volume_name(id),
        env: vec![],
        bind_host: "127.0.0.1".into(),
        container_port: 8787,
        provider_config: None,
        created_at: chrono::Utc::now(),
    }
}

/// Test bearer secret value (env is process-global; all tests share it).
pub const TEST_SECRET: &str = "orch-test-secret";

static SECRET_ONCE: Once = Once::new();

pub fn ensure_secret_env() {
    SECRET_ONCE.call_once(|| {
        std::env::set_var(
            oneai_orchestrator::config::ORCHESTRATOR_SECRET_ENV,
            TEST_SECRET,
        );
    });
}

/// Test config rooted at `dir` with fast timeouts.
pub fn test_config(dir: &Path) -> OrchestratorConfig {
    // OrchestratorConfig is #[non_exhaustive] — build via Default + mutation.
    let mut c = OrchestratorConfig::default();
    c.listen = "127.0.0.1:0".into();
    c.registry_dir = dir.to_path_buf();
    c.resume_timeout_secs = 3;
    c.idle_timeout_secs = 3600;
    c
}

/// Ready-to-use orchestrator state backed by a FakeRunner.
pub async fn test_state(dir: &Path) -> (Arc<OrchestratorState>, Arc<FakeRunner>) {
    ensure_secret_env();
    let runner = FakeRunner::new();
    let state = OrchestratorState::new(test_config(dir), runner.clone())
        .await
        .expect("state");
    (state, runner)
}
