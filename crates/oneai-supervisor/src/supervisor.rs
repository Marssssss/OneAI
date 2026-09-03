//! The in-process supervisor — orchestrates supervised instances.
//!
//! Holds the [`SupervisorRunner`] factory, the durable [`InstanceRegistry`],
//! and the live in-memory instance handles. All long-lived `AgentLoop` work
//! happens through [`InstanceHandle::run_turn`]; this layer schedules it,
//! records lifecycle transitions in the registry, and (for `rpc_stream`)
//! bridges the agent's `EngineYield` stream (via a per-call
//! [`oneai_bus::InProcessBus`] + [`oneai_agent::BusObserver`]) to an
//! [`EventSink`] that forwards each yield's JSON to the connected client.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::debug;

use oneai_agent::{AgentLoopObserver, BusObserver};
use oneai_bus::{EngineBus, InProcessBus};
use oneai_trace::{EventKind, SpanKind, SpanStatus, TraceContext};

use crate::error::{Result, SupervisorError};
use crate::registry::{InstanceRegistry, InstanceSpec, InstanceStatus};
use crate::runner::{InstanceHandle, SupervisorRunner, TurnSummary};

// ─── EventSink (carries serialized EngineYield JSON) ─────────────────────────

/// A sink that receives serialized `EngineYield` JSON values during a
/// streaming `rpc_stream` turn. Implemented by the server to forward each
/// value as a `StreamLine::event` line to the connected client; the in-proc
/// `Supervisor` tests provide a collecting impl.
pub trait EventSink: Send + Sync {
    /// Receive one `EngineYield` as an already-serialized JSON value.
    fn emit(&self, yield_json: serde_json::Value);
}

/// An [`EventSink`] that buffers every yield value in memory (tests / in-proc).
pub struct CollectingSink {
    inner: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl CollectingSink {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<serde_json::Value> {
        self.inner.lock().unwrap().clone()
    }
}

impl Default for CollectingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for CollectingSink {
    fn emit(&self, yield_json: serde_json::Value) {
        self.inner.lock().unwrap().push(yield_json);
    }
}

/// Spawn a forwarder that drains `bus`'s yield stream into `sink` as
/// serialized JSON values. The caller must have already obtained the
/// `receiver` via `bus.subscribe_yields()` BEFORE constructing the
/// `BusObserver` — otherwise early yields (emitted before the spawned task
/// subscribes) hit a broadcast with zero receivers and are lost. Returns
/// immediately; the task ends when the yield channel closes (bus dropped).
fn spawn_yield_forwarder(
    mut rx: tokio::sync::broadcast::Receiver<oneai_bus::EngineYield>,
    sink: Arc<dyn EventSink>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(yield_) = rx.recv().await {
            let value = serde_json::to_value(&yield_).unwrap_or(serde_json::Value::Null);
            sink.emit(value);
        }
    })
}

/// Build a per-turn bus + `BusObserver` and (for `rpc_stream`) a forwarder
/// that pumps yields to `sink`. The returned observer drives the turn; the
/// forwarder (if any) lives until the bus drops.
///
/// The yield subscription is taken BEFORE the `BusObserver` is constructed so
/// no early emit is lost to a zero-receiver broadcast (the same race P3 fixed
/// in `bridge_connection`).
fn wire_turn(sink: Option<Arc<dyn EventSink>>) -> (Arc<dyn EngineBus>, Arc<dyn AgentLoopObserver>) {
    let bus: Arc<dyn EngineBus> = Arc::new(InProcessBus::default());
    if let Some(sink) = sink {
        // Subscribe BEFORE spawning so the receiver exists before any emit.
        let rx = bus.subscribe_yields();
        // Detach the forwarder — it runs until the yield channel closes
        // (bus dropped at turn end), independent of this call's scope.
        spawn_yield_forwarder(rx, sink);
    }
    // turn_id is informational here (the wire `StreamLine` carries its own id);
    // BusObserver tags each yield with it so a frontend can correlate.
    let turn_id = format!("sup_{}", uuid::Uuid::new_v4());
    let observer: Arc<dyn AgentLoopObserver> = Arc::new(BusObserver::new(bus.clone(), turn_id));
    (bus, observer)
}

// ─── Supervisor ──────────────────────────────────────────────────────────────

/// The in-process supervisor orchestrator.
pub struct Supervisor {
    runner: Arc<dyn SupervisorRunner>,
    registry: Arc<InstanceRegistry>,
    instances: RwLock<HashMap<String, Arc<dyn InstanceHandle>>>,
    trace: Option<TraceContext>,
}

impl Supervisor {
    /// Build a supervisor over `runner` + `registry`, with optional OTEL trace.
    pub fn new(
        runner: Arc<dyn SupervisorRunner>,
        registry: Arc<InstanceRegistry>,
        trace: Option<TraceContext>,
    ) -> Self {
        Self {
            runner,
            registry,
            instances: RwLock::new(HashMap::new()),
            trace,
        }
    }

    fn trace_span(&self, name: &str) -> Option<String> {
        self.trace
            .as_ref()
            .map(|ctx| ctx.enter_span(SpanKind::AGENT, name, None))
    }

    fn trace_exit(&self, span: Option<String>, status: SpanStatus) {
        if let (Some(ctx), Some(span)) = (&self.trace, span) {
            ctx.exit_span(&span, status);
        }
    }

    fn log_event(&self, kind: EventKind, name: &str, attrs: HashMap<String, serde_json::Value>) {
        if let Some(ctx) = &self.trace {
            ctx.log_event(kind, name, attrs);
        }
    }

    /// Spawn a new supervised instance.
    pub async fn spawn(&self, spec: InstanceSpec) -> Result<String> {
        if !self.runner.has_provider() {
            return Err(SupervisorError::NoProvider);
        }
        let span = self.trace_span("supervisor.spawn");
        let handle = self.runner.spawn(&spec).await?;
        self.registry
            .register(spec.clone(), InstanceStatus::Idle)
            .await?;
        self.instances.write().await.insert(spec.id.clone(), handle);
        self.log_event(
            EventKind::Action,
            "supervisor.spawn",
            HashMap::from([("instance.id".to_string(), serde_json::json!(spec.id))]),
        );
        self.trace_exit(span, SpanStatus::Ok);
        debug!(instance = %spec.id, "supervisor: spawned");
        Ok(spec.id)
    }

    /// List all instances (durable registry snapshot).
    pub async fn list(&self) -> Vec<crate::registry::InstanceInfo> {
        self.registry.list().await
    }

    /// One instance's durable info.
    pub async fn status(&self, id: &str) -> Result<crate::registry::InstanceInfo> {
        self.registry
            .get(id)
            .await
            .ok_or_else(|| SupervisorError::InstanceNotFound(id.to_string()))
    }

    /// Stop an instance gracefully and unregister it.
    pub async fn stop(&self, id: &str) -> Result<()> {
        let handle = {
            let mut map = self.instances.write().await;
            map.remove(id)
                .ok_or_else(|| SupervisorError::InstanceNotFound(id.to_string()))?
        };
        self.registry
            .set_status(id, InstanceStatus::Stopping)
            .await?;
        handle.stop().await;
        self.registry
            .set_status(id, InstanceStatus::Stopped)
            .await?;
        self.registry.unregister(id).await?;
        Ok(())
    }

    /// Run one turn, returning the final summary (no streaming to a client).
    pub async fn rpc(&self, id: &str, task: &str) -> Result<TurnSummary> {
        let handle = {
            let map = self.instances.read().await;
            map.get(id)
                .cloned()
                .ok_or_else(|| SupervisorError::InstanceNotFound(id.to_string()))?
        };
        self.registry
            .set_status(id, InstanceStatus::Running)
            .await?;
        // Per-turn bus + BusObserver; no sink (no client to stream to).
        // Yields emit to the bus with zero subscribers → no-op.
        let (_bus, observer) = wire_turn(None);
        let span = self.trace_span("supervisor.rpc");
        let result = handle.run_turn(task, observer).await;
        self.trace_exit(
            span.clone(),
            if result.is_ok() {
                SpanStatus::Ok
            } else {
                SpanStatus::Error
            },
        );
        let summary = result?;
        self.registry.set_status(id, InstanceStatus::Idle).await?;
        self.registry.set_last_turn(id, summary.clone()).await?;
        Ok(summary)
    }

    /// Run one turn, streaming live `EngineYield`s to `sink`. Returns the final
    /// summary. Each yield is forwarded as serialized JSON (the wire
    /// `StreamLine.event` payload is an `EngineYield` value).
    pub async fn rpc_stream(
        &self,
        id: &str,
        task: &str,
        sink: Arc<dyn EventSink>,
    ) -> Result<TurnSummary> {
        let handle = {
            let map = self.instances.read().await;
            map.get(id)
                .cloned()
                .ok_or_else(|| SupervisorError::InstanceNotFound(id.to_string()))?
        };
        self.registry
            .set_status(id, InstanceStatus::Running)
            .await?;
        // Per-turn bus + BusObserver + a forwarder pumping yields to `sink`.
        let (bus, observer) = wire_turn(Some(sink));
        let span = self.trace_span("supervisor.rpc_stream");
        let result = handle.run_turn(task, observer).await;
        // Drop the bus so the forwarder task ends cleanly.
        drop(bus);
        self.trace_exit(
            span,
            if result.is_ok() {
                SpanStatus::Ok
            } else {
                SpanStatus::Error
            },
        );
        let summary = result?;
        self.registry.set_status(id, InstanceStatus::Idle).await?;
        self.registry.set_last_turn(id, summary.clone()).await?;
        Ok(summary)
    }

    /// Reconcile the registry after a daemon restart (live handles are gone).
    pub async fn recover_after_restart(&self) -> Result<()> {
        // Live in-memory handles are dead after a restart — drop them.
        self.instances.write().await.clear();
        self.registry.recover_after_restart().await
    }

    /// Reference to the durable registry (for the server).
    pub fn registry(&self) -> &Arc<InstanceRegistry> {
        &self.registry
    }
}

// `HashMap` is imported at the top of the file (used by `log_event`).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::InstanceStatus;
    use async_trait::async_trait;
    use oneai_agent::{AgentLoopResult, ParadigmKind};

    // A fake handle + runner for in-proc tests (no provider / no LLM).
    struct FakeHandle {
        status: std::sync::Mutex<InstanceStatus>,
        stopped: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl InstanceHandle for FakeHandle {
        fn status(&self) -> InstanceStatus {
            self.status.lock().unwrap().clone()
        }

        async fn run_turn(
            &self,
            _task: &str,
            observer: Arc<dyn AgentLoopObserver>,
        ) -> Result<TurnSummary> {
            *self.status.lock().unwrap() = InstanceStatus::Running;
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
            *self.status.lock().unwrap() = InstanceStatus::Idle;
            Ok(TurnSummary {
                final_answer: "hello".to_string(),
                iterations: 1,
                completed: true,
                active_paradigm: "react".to_string(),
            })
        }

        async fn stop(&self) {
            self.stopped
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    struct FakeRunner {
        handles: std::sync::Mutex<Vec<Arc<FakeHandle>>>,
    }

    #[async_trait]
    impl SupervisorRunner for FakeRunner {
        fn has_provider(&self) -> bool {
            true
        }
        async fn spawn(&self, _spec: &InstanceSpec) -> Result<Arc<dyn InstanceHandle>> {
            let h = Arc::new(FakeHandle {
                status: std::sync::Mutex::new(InstanceStatus::Idle),
                stopped: std::sync::atomic::AtomicBool::new(false),
            });
            self.handles.lock().unwrap().push(h.clone());
            Ok(h)
        }
    }

    async fn new_supervisor(dir: PathBuf) -> Supervisor {
        let registry = Arc::new(InstanceRegistry::new(dir).await.unwrap());
        let runner: Arc<dyn SupervisorRunner> = Arc::new(FakeRunner {
            handles: std::sync::Mutex::new(Vec::new()),
        });
        Supervisor::new(runner, registry, None)
    }

    use std::path::PathBuf;

    #[tokio::test]
    async fn in_proc_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let sup = new_supervisor(dir.path().to_path_buf()).await;

        let spec = InstanceSpec {
            id: "agent1".to_string(),
            domain: "coding".to_string(),
            model: None,
            user: None,
            created_at: chrono::Utc::now(),
        };
        let id = sup.spawn(spec).await.unwrap();
        assert_eq!(id, "agent1");
        assert_eq!(sup.list().await.len(), 1);

        let summary = sup.rpc("agent1", "hi").await.unwrap();
        assert_eq!(summary.final_answer, "hello");
        assert!(summary.completed);

        // status shows last turn + Idle.
        let info = sup.status("agent1").await.unwrap();
        assert!(matches!(info.status, InstanceStatus::Idle));
        assert!(info.last_turn.is_some());

        sup.stop("agent1").await.unwrap();
        assert!(sup.list().await.is_empty());
    }

    #[tokio::test]
    async fn rpc_stream_emits_events() {
        let dir = tempfile::tempdir().unwrap();
        let sup = new_supervisor(dir.path().to_path_buf()).await;
        sup.spawn(InstanceSpec {
            id: "s".to_string(),
            domain: "coding".to_string(),
            model: None,
            user: None,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

        let sink = Arc::new(CollectingSink::new());
        let summary = sup.rpc_stream("s", "hi", sink.clone()).await.unwrap();
        assert_eq!(summary.final_answer, "hello");

        let events = sink.events();
        // Each event is a serialized EngineYield — discriminate by `kind`.
        // Expect at least: iteration_start, two stream_chunk, direct_answer,
        // turn_complete.
        assert!(events
            .iter()
            .any(|e| e.get("kind").and_then(|v| v.as_str()) == Some("iteration_start")));
        assert!(
            events
                .iter()
                .filter(|e| e.get("kind").and_then(|v| v.as_str()) == Some("stream_chunk"))
                .count()
                >= 2
        );
        assert!(events
            .iter()
            .any(|e| e.get("kind").and_then(|v| v.as_str()) == Some("direct_answer")));
        assert!(events
            .iter()
            .any(|e| e.get("kind").and_then(|v| v.as_str()) == Some("turn_complete")));
    }

    #[tokio::test]
    async fn recover_marks_running_crashed() {
        let dir = tempfile::tempdir().unwrap();
        let sup = new_supervisor(dir.path().to_path_buf()).await;
        sup.spawn(InstanceSpec {
            id: "r".to_string(),
            domain: "coding".to_string(),
            model: None,
            user: None,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
        // Simulate a turn in flight persisted as Running.
        sup.registry()
            .set_status("r", InstanceStatus::Running)
            .await
            .unwrap();

        // Restart: live handles dropped, registry reconciled.
        sup.recover_after_restart().await.unwrap();
        let info = sup.status("r").await.unwrap();
        assert!(matches!(info.status, InstanceStatus::Crashed(_)));
    }
}
