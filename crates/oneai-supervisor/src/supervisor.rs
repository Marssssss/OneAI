//! The in-process supervisor — orchestrates supervised instances.
//!
//! Holds the [`SupervisorRunner`] factory, the durable [`InstanceRegistry`],
//! and the live in-memory instance handles. All long-lived `AgentLoop` work
//! happens through [`InstanceHandle::run_turn`]; this layer schedules it,
//! records lifecycle transitions in the registry, and (for `rpc_stream`)
//! bridges the agent's [`AgentLoopObserver`] callbacks to an [`EventSink`].

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::debug;

use oneai_agent::{
    AgentLoopObserver, AgentLoopResult, ParadigmKind, SubAgentKind, SubAgentSummary,
    ToolCallRequest,
};
use oneai_core::{ContextAccounting, InterruptPoint, ResumeSignal, ToolOutput};
use oneai_trace::{EventKind, SpanKind, SpanStatus, TraceContext};

use crate::error::{Result, SupervisorError};
use crate::registry::{InstanceRegistry, InstanceSpec, InstanceStatus};
use crate::runner::{paradigm_to_string, InstanceHandle, SupervisorRunner, TurnSummary};

// ─── Events (mirror of oneai-studio::StudioEvent, for rpc_stream) ────────────

/// Frontend-facing tool-call representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallView {
    pub id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
}

/// One lifecycle event pushed during a streaming `rpc_stream` turn.
///
/// Each variant corresponds to an [`AgentLoopObserver`] callback, serialized
/// as JSON for the connected client (mirrors `oneai-studio::StudioEvent`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Event {
    IterationStart {
        iteration: usize,
        paradigm: String,
    },
    DirectAnswer {
        text: String,
    },
    ToolCalls {
        calls: Vec<ToolCallView>,
    },
    ToolResult {
        call_id: String,
        tool_name: String,
        success: bool,
        output_summary: String,
    },
    Delegate {
        task: String,
        agent_type: String,
    },
    ParadigmSwitch {
        paradigm: String,
    },
    CheckpointSaved {
        iteration: usize,
        checkpoint_id: String,
    },
    TraceEvent {
        kind: String,
        name: String,
        attributes: serde_json::Value,
    },
    Thinking {
        text: String,
    },
    StreamChunk {
        text: String,
    },
    LoopComplete {
        result_summary: String,
    },
    Error {
        message: String,
    },
}

/// A sink that receives [`Event`]s during a streaming turn. Implemented by the
/// server to forward JSON lines to the connected client; the in-proc `Supervisor`
/// also provides a collecting impl for tests.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}

/// An [`EventSink`] that buffers every event in memory (tests / in-proc use).
pub struct CollectingSink {
    inner: std::sync::Mutex<Vec<Event>>,
}

impl CollectingSink {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.inner.lock().unwrap().clone()
    }
}

impl Default for CollectingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for CollectingSink {
    fn emit(&self, event: Event) {
        self.inner.lock().unwrap().push(event);
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}…", &s[..max_len])
    }
}

/// An [`AgentLoopObserver`] that bridges every callback to an [`EventSink`].
struct StreamingObserver {
    sink: Arc<dyn EventSink>,
}

impl StreamingObserver {
    fn new(sink: Arc<dyn EventSink>) -> Self {
        Self { sink }
    }

    fn emit(&self, event: Event) {
        self.sink.emit(event);
    }
}

impl AgentLoopObserver for StreamingObserver {
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind) {
        self.emit(Event::IterationStart {
            iteration,
            paradigm: paradigm_to_string(paradigm),
        });
    }

    fn on_direct_answer(&self, text: &str) {
        self.emit(Event::DirectAnswer {
            text: text.to_string(),
        });
    }

    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        self.emit(Event::ToolCalls {
            calls: calls
                .iter()
                .map(|c| ToolCallView {
                    id: c.id.clone(),
                    tool_name: c.name.clone(),
                    args: c.args.clone(),
                })
                .collect(),
        });
    }

    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &ToolOutput) {
        self.emit(Event::ToolResult {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            success: output.success,
            output_summary: if output.success {
                truncate(&output.content, 200)
            } else {
                output.error.clone().unwrap_or_default()
            },
        });
    }

    fn on_delegate(&self, task: &str, agent_type: &SubAgentKind) {
        self.emit(Event::Delegate {
            task: task.to_string(),
            agent_type: agent_type.name().to_string(),
        });
    }

    fn on_delegate_complete(&self, summary: &SubAgentSummary) {
        self.emit(Event::TraceEvent {
            kind: "DelegateComplete".to_string(),
            name: "agent.delegate_complete".to_string(),
            attributes: serde_json::json!({
                "completed": summary.completed,
                "summary": truncate(&summary.summary, 200),
            }),
        });
    }

    fn on_paradigm_switch(&self, paradigm: ParadigmKind) {
        self.emit(Event::ParadigmSwitch {
            paradigm: paradigm_to_string(paradigm),
        });
    }

    fn on_checkpoint(&self, iteration: usize) {
        self.emit(Event::CheckpointSaved {
            iteration,
            checkpoint_id: format!("checkpoint_iter_{}", iteration),
        });
    }

    fn on_complete(&self, result: &AgentLoopResult) {
        self.emit(Event::LoopComplete {
            result_summary: format!(
                "Completed: {} iterations, paradigm {}",
                result.iterations,
                paradigm_to_string(result.active_paradigm)
            ),
        });
    }

    fn on_stream_chunk(&self, text: &str) {
        self.emit(Event::StreamChunk {
            text: text.to_string(),
        });
    }

    fn on_thinking(&self, text: &str) {
        self.emit(Event::Thinking {
            text: text.to_string(),
        });
    }

    fn on_token_usage(&self, prompt_tokens: u32, completion_tokens: u32) {
        self.emit(Event::TraceEvent {
            kind: "TokenUsage".to_string(),
            name: "llm.token_usage".to_string(),
            attributes: serde_json::json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            }),
        });
    }

    fn on_context_accounting(&self, accounting: &ContextAccounting) {
        self.emit(Event::TraceEvent {
            kind: "ContextAccounting".to_string(),
            name: "agent.context_accounting".to_string(),
            attributes: serde_json::json!({
                "total_tokens": accounting.total_tokens,
                "context_window_size": accounting.context_window_size,
                "utilization_pct": accounting.utilization_pct,
            }),
        });
    }

    fn on_interrupt(&self, point: &InterruptPoint) {
        self.emit(Event::TraceEvent {
            kind: "Interrupt".to_string(),
            name: "agent.interrupt".to_string(),
            attributes: serde_json::json!({
                "id": point.id,
                "iteration": point.iteration,
                "reason": format!("{:?}", point.reason),
            }),
        });
    }

    fn on_resume(&self, signal: &ResumeSignal) {
        self.emit(Event::TraceEvent {
            kind: "Resume".to_string(),
            name: "agent.resume".to_string(),
            attributes: serde_json::json!({
                "interrupt_id": signal.interrupt_id,
                "feedback": signal.feedback,
                "action": format!("{:?}", signal.action),
            }),
        });
    }

    fn on_plan_update(&self, plan: Option<&oneai_agent::plan_state::PlanState>) {
        self.emit(Event::TraceEvent {
            kind: "PlanUpdate".to_string(),
            name: "agent.plan_update".to_string(),
            attributes: serde_json::json!({
                "has_plan": plan.is_some(),
            }),
        });
    }

    fn on_reflection(&self, summary: &str) {
        self.emit(Event::TraceEvent {
            kind: "Reflection".to_string(),
            name: "agent.reflection".to_string(),
            attributes: serde_json::json!({ "summary": summary }),
        });
    }
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
        let sink: Arc<dyn EventSink> = Arc::new(CollectingSink::new());
        let observer: Arc<dyn AgentLoopObserver> = Arc::new(StreamingObserver::new(sink.clone()));
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

    /// Run one turn, streaming live events to `sink`. Returns the final summary.
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
        let observer: Arc<dyn AgentLoopObserver> = Arc::new(StreamingObserver::new(sink));
        let span = self.trace_span("supervisor.rpc_stream");
        let result = handle.run_turn(task, observer).await;
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
        // Expect at least: IterationStart, two StreamChunks, DirectAnswer, LoopComplete.
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::IterationStart { paradigm, .. } if paradigm == "react")));
        assert!(
            events
                .iter()
                .filter(|e| matches!(e, Event::StreamChunk { .. }))
                .count()
                >= 2
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::DirectAnswer { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::LoopComplete { .. })));
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
