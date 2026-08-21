//! AsyncTaskRunner — fire-and-auto-notify background delegation (Phase 2A).
//!
//! Modelled on opencode's `task` tool with `background=true` (and codex's
//! actor `Result` messaging): the parent delegates a subtask to a background
//! sub-agent and **continues immediately** — it does NOT block, does NOT
//! poll, and does NOT collect. When the background sub-agent finishes, the
//! runner injects its result back into the parent conversation as a synthetic
//! message and re-triggers a parent turn (via the [`BackgroundCompletionSink`],
//! which the production wiring backs with an engine-bus
//! `Directive::UserMessage`). The parent is then re-activated with the result
//! in context.
//!
//! This is the **only** correct non-blocking shape: the host (not the LLM)
//! owns the rendezvous. An LLM-polled model (`task_status` / `collect_results`)
//! was the first attempt and failed at runtime — the parent iterated faster
//! than its children completed and re-delegated in a tight loop (see the plan
//! doc + `memory/phase2a-background-delegation.md`). Blocking the parent on
//! in-flight tasks (`await_in_flight`) collapses back to synchronous `delegate`,
//! so it was removed too.
//!
//! Consequences of the fire-and-notify model:
//! - **No DAG `depends_on` scheduling.** The model sequences across turns:
//!   delegate A → (turn ends) → A's result is injected → new turn → delegate B
//!   with A's result in context. `depends_on` is accepted on the `DelegateTask`
//!   (shared with blocking `delegate`) but ignored here.
//! - **No `&self`-from-spawn problem.** The spawned sub-agent task holds only
//!   `Arc` clones (factory, sink, tasks map, progress sender, cancel token) —
//!   all `Send + Sync` — and calls `sink.notify(...)`, never a `&self` runner
//!   method. (The earlier reaper needed `&self`, which the `!Sync`
//!   `UnboundedSender` made non-`Send` across a spawned await; no reaper ⇒ no
//!   issue.)
//! - **Cross-turn survival.** The spawned `tokio::spawn` tasks outlive the
//!   per-turn `AgentLoop` (dropping a `JoinHandle` does not abort the task).
//!   They hold long-lived `Arc`s (the app provider via the factory, the bus via
//!   the sink), so they complete and self-report even after the parent's turn
//!   ends.
//!
//! **Scope**: a background task lives until it finishes (or the app/runtime
//! shuts down). Multi-session routing is a known limitation — the sink
//! submits a `Directive::UserMessage`, which the pump routes to the *active*
//! session; if the user switched sessions while a background task ran, the
//! result lands on the wrong session. A session-targeted directive variant is
//! the follow-up.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex, RwLock, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AgentLoopObserver, DelegateProgressEvent, DelegateTask, DelegationPolicy};
use crate::sub_agent::{DelegationSpec, SubAgentFactory, SubAgentKind, SubAgentSummary};
use oneai_core::error::Result;

// ─── Task Status ────────────────────────────────────────────────────────────

/// Status of an asynchronous background task.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TaskStatus {
    /// Task is currently executing (its sub-agent loop is running).
    Running,
    /// Task completed successfully; its result was injected back to the parent.
    Completed,
    /// Task failed with an error; the failure was injected back to the parent.
    Failed(String),
    /// Task was cancelled (graceful, via its child cancel token).
    Cancelled,
}

// ─── Task Info ───────────────────────────────────────────────────────────────

/// Information about a background task (for UI / cancel / diagnostics).
#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub id: String,
    pub call_id: String,
    pub agent_kind: SubAgentKind,
    pub description: String,
    pub status: TaskStatus,
    pub allocated_tokens: u32,
}

/// Internal record for a background task held by the runner.
struct BackgroundTask {
    id: String,
    call_id: String,
    kind: SubAgentKind,
    description: String,
    status: TaskStatus,
    join_handle: Option<JoinHandle<()>>,
    child_cancel: Option<CancellationToken>,
}

impl TaskStatus {
    /// Human-readable label for context injection / UI. `Failed` carries its
    /// detail inline so the model sees why a background task died.
    pub fn label(&self) -> String {
        match self {
            TaskStatus::Running => "Running".to_string(),
            TaskStatus::Completed => "Completed".to_string(),
            TaskStatus::Failed(detail) => format!("Failed ({detail})"),
            TaskStatus::Cancelled => "Cancelled".to_string(),
        }
    }
}

impl BackgroundTask {
    fn to_info(&self) -> TaskInfo {
        TaskInfo {
            id: self.id.clone(),
            call_id: self.call_id.clone(),
            agent_kind: self.kind.clone(),
            description: self.description.clone(),
            status: self.status.clone(),
            allocated_tokens: 0,
        }
    }
}

type ProgressItem = (String, SubAgentKind, DelegateProgressEvent);

// ─── BackgroundCompletionSink ────────────────────────────────────────────────

/// The host-side rendezvous point: when a background sub-agent finishes, the
/// runner calls `notify` to inject its result back into the parent's
/// conversation and re-trigger a parent turn. The production implementation
/// emits an engine-bus `DelegateComplete` (so the frontend marks the sub-agent
/// card done) and then submits a `Directive::UserMessage` carrying the result
/// (which re-activates the parent); tests use a recording sink.
///
/// This is what makes the design "auto-notify" rather than LLM-polled: the LLM
/// never calls a `collect_results` tool; the host pushes the result when it's
/// ready. `turn_id` is the parent turn that launched the task, so the
/// `DelegateComplete` yield lands on the right sub-agent card across turns.
#[async_trait::async_trait]
pub trait BackgroundCompletionSink: Send + Sync {
    async fn notify(&self, turn_id: &str, task_id: &str, summary: SubAgentSummary);
}

/// A sink that discards notifications. For the legacy
/// `with_parallel_delegation*` builders (which pre-date the fire-and-notify
/// sink param) and for tests that only exercise submission, not delivery.
pub struct NoopCompletionSink;
#[async_trait::async_trait]
impl BackgroundCompletionSink for NoopCompletionSink {
    async fn notify(&self, _turn_id: &str, _task_id: &str, _summary: SubAgentSummary) {}
}

// ─── AsyncTaskRunner ────────────────────────────────────────────────────────

/// Spawns non-blocking background sub-agents and auto-notifies the parent on
/// completion. Constructed per turn (via `AgentLoop::with_background_delegation`)
/// and stored as `Arc<AsyncTaskRunner>`. The spawned sub-agent tasks outlive the
/// per-turn loop and self-report via the [`BackgroundCompletionSink`].
pub struct AsyncTaskRunner {
    factory: Arc<dyn SubAgentFactory>,
    sem: Arc<Semaphore>,
    pool: Option<Arc<AtomicU32>>,
    parent_cancel: CancellationToken,
    sink: Arc<dyn BackgroundCompletionSink>,
    /// The parent turn that owns this runner (it's per-turn). Threaded into
    /// `notify` so the sink can emit a `DelegateComplete` yield that the
    /// frontend matches onto the right sub-agent card even after the
    /// delegating turn has ended.
    turn_id: String,
    /// Optional engine bus for DIRECT progress emission. When present, the
    /// spawned sub-agent's `ForwardingObserver` emits `DelegateProgress`
    /// straight to the bus (sync) so live status keeps flowing to the
    /// frontend EVEN AFTER the parent turn ends — without it, progress only
    /// moves while the parent loop is iterating (`drain_progress`), and a
    /// long-running background sub-agent looks frozen. `None` for non-bus
    /// builds (foreground-style / tests).
    bus: Option<Arc<dyn oneai_bus::EngineBus>>,
    tasks: Arc<RwLock<HashMap<String, BackgroundTask>>>,
    progress_rx: Mutex<mpsc::UnboundedReceiver<ProgressItem>>,
    progress_tx: mpsc::UnboundedSender<ProgressItem>,
    next_id: Arc<Mutex<u64>>,
}

impl AsyncTaskRunner {
    /// Create a runner. `sink` receives each finished sub-agent's summary and
    /// is responsible for emitting a `DelegateComplete` yield + injecting the
    /// result back into the parent conversation + re-triggering a parent turn.
    /// `turn_id` is the parent turn launching the tasks.
    pub fn new(
        factory: Arc<dyn SubAgentFactory>,
        policy: DelegationPolicy,
        parent_cancel: CancellationToken,
        sink: Arc<dyn BackgroundCompletionSink>,
        turn_id: String,
        bus: Option<Arc<dyn oneai_bus::EngineBus>>,
    ) -> Self {
        let sem = Arc::new(Semaphore::new(policy.max_concurrent.max(1)));
        let pool = policy
            .budget_pool
            .as_ref()
            .map(|b| Arc::new(AtomicU32::new(b.total)));
        let (progress_tx, progress_rx) = mpsc::unbounded_channel::<ProgressItem>();
        Self {
            factory,
            sem,
            pool,
            parent_cancel,
            sink,
            turn_id,
            bus,
            tasks: Arc::new(RwLock::new(HashMap::new())),
            progress_rx: Mutex::new(progress_rx),
            progress_tx,
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Submit a background task. Spawns the sub-agent as an independent tokio
    /// task and returns the task id **immediately** — the parent never awaits
    /// the sub-agent. When the sub-agent finishes, its summary is pushed to the
    /// [`BackgroundCompletionSink`], which re-activates the parent.
    ///
    /// `depends_on` is accepted (the `DelegateTask` is shared with blocking
    /// `delegate`) but ignored — background tasks are independent; the model
    /// sequences across turns via the auto-notification.
    pub async fn submit_delegate(&self, task: DelegateTask) -> Result<String> {
        let id = self.assign_id(&task.id).await;
        let kind = task.agent_type.clone();
        let description = task.task.clone();
        let call_id = task.call_id.clone();
        let spec = DelegationSpec {
            system_prompt: task.system_prompt_override.clone(),
            tools: task.tools_override.clone(),
            seed_messages: task.seed_messages.clone(),
        };
        let budget = {
            // Per-kind floor: the parent's `budget_tokens` is advisory; raise
            // it to the kind's minimum for expensive kinds (code-gen +
            // thinking) so a well-intentioned-but-too-small budget doesn't
            // starve the sub-agent to death mid-task (and the budget-pool
            // deduction below reflects the REAL cost, not the under-stated
            // request). `build()` re-applies the floor — a no-op here.
            let floor = kind.min_budget_tokens();
            if task.budget.total < floor {
                tracing::info!(
                    kind = kind.name(),
                    requested = task.budget.total,
                    floor,
                    "background delegate budget below kind floor; raising"
                );
                oneai_core::budget::TokenBudget::new(floor)
            } else {
                task.budget.clone()
            }
        };
        let child_cancel = self.parent_cancel.child_token();

        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(
                id.clone(),
                BackgroundTask {
                    id: id.clone(),
                    call_id: call_id.clone(),
                    kind: kind.clone(),
                    description: description.clone(),
                    status: TaskStatus::Running,
                    join_handle: None,
                    child_cancel: Some(child_cancel.clone()),
                },
            );
        }

        let factory = self.factory.clone();
        let sem = self.sem.clone();
        let pool = self.pool.clone();
        let progress_tx = self.progress_tx.clone();
        let tasks_arc = self.tasks.clone();
        let sink = self.sink.clone();
        let id_clone = id.clone();
        let kind_clone = kind.clone();
        let turn_id = self.turn_id.clone();
        let bus = self.bus.clone();

        let handle = tokio::spawn(async move {
            use crate::agent_loop::ForwardingObserver;
            let outcome: std::result::Result<SubAgentSummary, oneai_core::error::OneAIError> =
                async {
                    // Concurrency cap.
                    let _permit = match sem.acquire_owned().await {
                        Ok(p) => p,
                        Err(e) => {
                            return Err(oneai_core::error::OneAIError::Agent(format!(
                                "Background semaphore closed: {e}"
                            )));
                        }
                    };
                    // Budget pool gate (conservative sum-of-budgets; no refund).
                    if let Some(p) = &pool {
                        let need = budget.total;
                        let prev = p.fetch_sub(need, Ordering::Relaxed);
                        if prev < need {
                            p.store(0, Ordering::Relaxed);
                            return Ok(crate::sub_agent::SubAgentSummary {
                                completed: false,
                                summary: "[budget pool exhausted]".to_string(),
                                key_findings: Vec::new(),
                                budget_exceeded: true,
                                agent_kind: kind_clone.clone(),
                                tokens_used: 0,
                            });
                        }
                    }
                    let forwarder = ForwardingObserver::new(
                        id_clone.clone(),
                        kind_clone.clone(),
                        progress_tx,
                        turn_id.clone(),
                        bus.clone(),
                    );
                    let sub_agent = factory
                        .create_with_spec(kind_clone.clone(), budget, spec)
                        .await?;
                    let summary = sub_agent
                        .run_with_observer(&description, Some(&forwarder), Some(child_cancel))
                        .await?;
                    Ok(summary)
                }
                .await;

            // Record the terminal status (for UI / cancel / diagnostics).
            let summary_opt: Option<SubAgentSummary> = match &outcome {
                Ok(s) => Some(s.clone()),
                Err(_) => None,
            };
            {
                let mut tasks = tasks_arc.write().await;
                if let Some(t) = tasks.get_mut(&id_clone) {
                    t.status = match &outcome {
                        Ok(s) if s.completed => TaskStatus::Completed,
                        Ok(s) => TaskStatus::Failed(s.summary.clone()),
                        Err(e) => TaskStatus::Failed(e.to_string()),
                    };
                }
            }

            // ─── Fire-and-auto-notify ────────────────────────────────────
            // Push the result (success OR failure) to the sink, which injects
            // it into the parent conversation + re-triggers a parent turn. The
            // parent never polls; it is woken here.
            if let Some(summary) = summary_opt {
                sink.notify(&turn_id, &id_clone, summary).await;
            } else {
                // The sub-agent errored before producing a summary — still
                // notify so the parent isn't left waiting.
                sink.notify(
                    &turn_id,
                    &id_clone,
                    SubAgentSummary {
                        completed: false,
                        summary: match &outcome {
                            Err(e) => format!("[background sub-agent failed: {e}]"),
                            _ => "[background sub-agent failed]".to_string(),
                        },
                        key_findings: Vec::new(),
                        budget_exceeded: false,
                        agent_kind: kind_clone.clone(),
                        tokens_used: 0,
                    },
                )
                .await;
            }
        });

        {
            let mut tasks = self.tasks.write().await;
            if let Some(t) = tasks.get_mut(&id) {
                t.join_handle = Some(handle);
            }
        }

        tracing::info!(
            "AsyncTaskRunner: submitted background task '{}' (kind: {})",
            id,
            kind.name()
        );
        Ok(id)
    }

    /// Assign a unique id. If the model's id is free, use it; on collision,
    /// append `_2`, `_3`, … so re-submission doesn't overwrite a live task.
    async fn assign_id(&self, model_id: &str) -> String {
        let base = if model_id.is_empty() {
            let mut counter = self.next_id.lock().await;
            let id = format!("bg_task_{}", *counter);
            *counter += 1;
            return id;
        } else {
            model_id.to_string()
        };
        let tasks = self.tasks.read().await;
        if !tasks.contains_key(&base) {
            return base;
        }
        let mut n = 2;
        loop {
            let candidate = format!("{}_{}", base, n);
            if !tasks.contains_key(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Check the current status of a task.
    pub async fn status(&self, task_id: &str) -> TaskStatus {
        let tasks = self.tasks.read().await;
        tasks
            .get(task_id)
            .map(|t| t.status.clone())
            .unwrap_or(TaskStatus::Failed("Task not found".to_string()))
    }

    /// Get full task info.
    pub async fn task_info(&self, task_id: &str) -> Option<TaskInfo> {
        let tasks = self.tasks.read().await;
        tasks.get(task_id).map(|t| t.to_info())
    }

    /// List all tasks and their info.
    pub async fn all_tasks(&self) -> Vec<TaskInfo> {
        let tasks = self.tasks.read().await;
        tasks.values().map(|t| t.to_info()).collect()
    }

    /// Snapshot of (id, status) — for UI / the schema gate's liveness check.
    pub async fn status_snapshot(&self) -> Vec<(String, TaskStatus)> {
        let tasks = self.tasks.read().await;
        tasks
            .iter()
            .map(|(id, t)| (id.clone(), t.status.clone()))
            .collect()
    }

    /// Snapshot of (id, kind, description, status) — richer enough for the
    /// parent's `[Background tasks]` context block (the model needs to see what
    /// each in-flight task is, not just a bare id). Order is insertion order
    /// (HashMap iteration is unspecified, so callers must not rely on it).
    pub async fn snapshot_with_meta(&self) -> Vec<(String, SubAgentKind, String, TaskStatus)> {
        let tasks = self.tasks.read().await;
        tasks
            .values()
            .map(|t| {
                (
                    t.id.clone(),
                    t.kind.clone(),
                    t.description.clone(),
                    t.status.clone(),
                )
            })
            .collect()
    }

    /// Whether any task is still running.
    pub async fn has_in_flight(&self) -> bool {
        let tasks = self.tasks.read().await;
        tasks
            .values()
            .any(|t| matches!(t.status, TaskStatus::Running))
    }

    /// Forward buffered sub-agent progress events onto the observer. Called by
    /// the AgentLoop each iteration so the UI isn't blind during a background
    /// delegation.
    pub async fn drain_progress(&self, observer: &dyn AgentLoopObserver) {
        let mut rx = self.progress_rx.lock().await;
        while let Ok((id, kind, ev)) = rx.try_recv() {
            observer.on_delegate_progress(&id, &kind, &ev);
        }
    }

    /// Cancel a background task gracefully (child cancel token — the sub-agent
    /// stops at its next iteration boundary). Marked `Cancelled`. The sink is
    /// NOT notified (the parent asked to cancel; no result to inject).
    pub async fn cancel(&self, task_id: &str) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.get_mut(task_id) {
            if let Some(token) = &t.child_cancel {
                token.cancel();
            }
            if let Some(handle) = &t.join_handle {
                handle.abort(); // hard backstop if the loop doesn't observe the token
            }
            t.status = TaskStatus::Cancelled;
        } else {
            return Err(oneai_core::error::OneAIError::Agent(format!(
                "Background task '{}' not found",
                task_id
            )));
        }
        tracing::info!("AsyncTaskRunner: cancelled background task '{}'", task_id);
        Ok(())
    }

    /// Cancel all in-flight tasks (e.g. on app shutdown).
    pub async fn cancel_all(&self) {
        let ids: Vec<String> = {
            let tasks = self.tasks.read().await;
            tasks
                .iter()
                .filter(|(_, t)| t.status == TaskStatus::Running)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            let _ = self.cancel(&id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sub_agent::{SubAgent, SubAgentKind};
    use async_trait::async_trait;
    use oneai_core::budget::TokenBudget;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ─── Mock SubAgent ───────────────────────────────────────────────────────

    struct MockSubAgent {
        kind: SubAgentKind,
        response: String,
    }

    #[async_trait]
    impl SubAgent for MockSubAgent {
        async fn run(&self, task: &str) -> Result<SubAgentSummary> {
            Ok(SubAgentSummary {
                completed: true,
                summary: self.response.clone(),
                key_findings: vec![task.to_string()],
                budget_exceeded: false,
                agent_kind: self.kind.clone(),
                tokens_used: 500,
            })
        }
        async fn run_with_observer(
            &self,
            task: &str,
            observer: Option<&dyn AgentLoopObserver>,
            cancel: Option<CancellationToken>,
        ) -> Result<SubAgentSummary> {
            if let Some(tok) = cancel {
                if tok.is_cancelled() {
                    return Ok(SubAgentSummary {
                        completed: false,
                        summary: "[cancelled]".to_string(),
                        key_findings: vec![],
                        budget_exceeded: false,
                        agent_kind: self.kind.clone(),
                        tokens_used: 0,
                    });
                }
            }
            if let Some(obs) = observer {
                obs.on_iteration_start(1, crate::agent_loop::ParadigmKind::ReAct);
            }
            self.run(task).await
        }
        fn kind(&self) -> &SubAgentKind {
            &self.kind
        }
        fn budget(&self) -> &TokenBudget {
            static BUDGET: TokenBudget = TokenBudget {
                total: 10000,
                consumed: 0,
                charge_prompt: true,
            };
            &BUDGET
        }
    }

    struct MockFactory;
    #[async_trait]
    impl SubAgentFactory for MockFactory {
        async fn create(
            &self,
            kind: SubAgentKind,
            _budget: TokenBudget,
        ) -> Result<Box<dyn SubAgent>> {
            Ok(Box::new(MockSubAgent {
                kind: kind.clone(),
                response: format!("Result for kind {}", kind.name()),
            }))
        }
        fn available_kinds(&self) -> Vec<SubAgentKind> {
            vec![SubAgentKind::Explore, SubAgentKind::Code]
        }
        fn is_available(&self, kind: &SubAgentKind) -> bool {
            matches!(kind, SubAgentKind::Explore | SubAgentKind::Code)
        }
    }

    /// A recording sink — captures every `notify` so tests assert the
    /// fire-and-auto-notify path without a real bus.
    struct RecordingSink {
        notifications: Arc<std::sync::Mutex<Vec<(String, String, SubAgentSummary)>>>,
    }
    #[async_trait]
    impl BackgroundCompletionSink for RecordingSink {
        async fn notify(&self, turn_id: &str, task_id: &str, summary: SubAgentSummary) {
            self.notifications.lock().unwrap().push((
                turn_id.to_string(),
                task_id.to_string(),
                summary,
            ));
        }
    }

    fn make_runner(sink: Arc<dyn BackgroundCompletionSink>) -> Arc<AsyncTaskRunner> {
        Arc::new(AsyncTaskRunner::new(
            Arc::new(MockFactory),
            DelegationPolicy::default(),
            CancellationToken::new(),
            sink,
            "test_turn".to_string(),
            None,
        ))
    }

    fn delegate_task(id: &str, task: &str, kind: SubAgentKind) -> DelegateTask {
        DelegateTask {
            id: id.to_string(),
            task: task.to_string(),
            agent_type: kind,
            budget: TokenBudget::new(5000),
            depends_on: Vec::new(),
            call_id: format!("call_{}", id),
            custom_role: None,
            system_prompt_override: None,
            tools_override: None,
            inherit_context: false,
            inherit_last_n: 0,
            seed_messages: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_submit_returns_immediately_and_notifies() {
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = make_runner(Arc::new(RecordingSink {
            notifications: notifications.clone(),
        }));

        let id = runner
            .submit_delegate(delegate_task("a", "Find auth code", SubAgentKind::Explore))
            .await
            .unwrap();
        assert_eq!(id, "a");

        // Wait for the spawned task to finish + notify.
        for _ in 0..100 {
            if runner.status(&id).await == TaskStatus::Completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(runner.status(&id).await, TaskStatus::Completed);

        // The sink was auto-notified with the completed summary.
        let notes = notifications.lock().unwrap();
        assert_eq!(notes.len(), 1, "sink should be auto-notified once");
        assert_eq!(notes[0].1, "a");
        assert!(notes[0].2.completed);
        assert!(notes[0].2.summary.contains("Result for kind"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_id_collision_dedup() {
        let runner = make_runner(Arc::new(RecordingSink {
            notifications: Arc::new(std::sync::Mutex::new(Vec::new())),
        }));
        let id1 = runner
            .submit_delegate(delegate_task("dup", "t1", SubAgentKind::Explore))
            .await
            .unwrap();
        let id2 = runner
            .submit_delegate(delegate_task("dup", "t2", SubAgentKind::Explore))
            .await
            .unwrap();
        assert_eq!(id1, "dup");
        assert_eq!(id2, "dup_2");
        assert_eq!(runner.all_tasks().await.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_assigned_id_when_model_omits() {
        let runner = make_runner(Arc::new(RecordingSink {
            notifications: Arc::new(std::sync::Mutex::new(Vec::new())),
        }));
        let id = runner
            .submit_delegate(delegate_task("", "task", SubAgentKind::Explore))
            .await
            .unwrap();
        assert!(id.starts_with("bg_task_"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_cancel_no_notify() {
        let notifications = Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = make_runner(Arc::new(RecordingSink {
            notifications: notifications.clone(),
        }));
        let id = runner
            .submit_delegate(delegate_task("x", "task", SubAgentKind::Explore))
            .await
            .unwrap();
        runner.cancel(&id).await.unwrap();
        let status = runner.status(&id).await;
        assert!(matches!(
            status,
            TaskStatus::Cancelled | TaskStatus::Completed
        ));
        // If it was cancelled before completing, no notify fired.
        if status == TaskStatus::Cancelled {
            assert!(notifications.lock().unwrap().is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_drain_progress_forwards() {
        use std::sync::Arc as StdArc;
        struct CountingObserver {
            n: StdArc<AtomicUsize>,
        }
        impl AgentLoopObserver for CountingObserver {
            fn on_delegate_progress(
                &self,
                _id: &str,
                _kind: &crate::sub_agent::SubAgentKind,
                _ev: &DelegateProgressEvent,
            ) {
                self.n.fetch_add(1, Ordering::Relaxed);
            }
            fn on_iteration_start(&self, _: usize, _: crate::agent_loop::ParadigmKind) {}
            fn on_direct_answer(&self, _: &str) {}
            fn on_tool_calls(&self, _: &[crate::agent_loop::ToolCallRequest]) {}
            fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
            fn on_delegate(&self, _: &str, _: &str, _: &crate::sub_agent::SubAgentKind) {}
            fn on_paradigm_switch(&self, _: crate::agent_loop::ParadigmKind) {}
            fn on_checkpoint(&self, _: usize) {}
            fn on_complete(&self, _: &crate::agent_loop::AgentLoopResult) {}
        }
        let counter = StdArc::new(AtomicUsize::new(0));
        let obs = CountingObserver { n: counter.clone() };
        let runner = make_runner(Arc::new(RecordingSink {
            notifications: Arc::new(std::sync::Mutex::new(Vec::new())),
        }));
        runner
            .submit_delegate(delegate_task("p", "task", SubAgentKind::Explore))
            .await
            .unwrap();
        for _ in 0..100 {
            if runner.status("p").await == TaskStatus::Completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        runner.drain_progress(&obs).await;
        assert!(counter.load(Ordering::Relaxed) >= 1, "progress forwarded");
    }
}
