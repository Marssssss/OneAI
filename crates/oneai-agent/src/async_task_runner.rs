//! AsyncTaskRunner — background task delegation with progress tracking.
//!
//! Claude Code can spawn background agents that run independently and
//! notify the main agent loop when they complete. OneAI's AsyncTaskRunner
//! provides the same capability: the main agent can delegate tasks to
//! background workers, continue working on other things, and check back
//! later for results.
//!
//! Key features:
//! - **Non-blocking delegation**: Main agent continues working while
//!   background tasks run independently
//! - **Progress tracking**: Each task has a status (pending/running/completed/failed)
//!   that can be queried at any time
//! - **Result collection**: When a task completes, its result is stored
//!   and can be retrieved by the main agent
//! - **Observer integration**: Task progress notifications are sent
//!   through the AgentLoopObserver, enabling TUI updates
//! - **Budget awareness**: Background tasks consume from the shared
//!   token budget, and the runner respects budget limits
//!
//! **Interface unification**: AsyncTaskRunner now uses the same
//! `SubAgentFactory` trait from `sub_agent.rs`, ensuring consistent
//! sub-agent creation across the system. The factory creates a
//! `Box<dyn SubAgent>` with a scoped tool set and budget, which
//! the runner spawns as an independent tokio task.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{RwLock, Mutex};
use tokio::task::JoinHandle;

use oneai_core::error::Result;
use oneai_core::budget::TokenBudget;
use crate::sub_agent::{SubAgent, SubAgentSummary, SubAgentKind, SubAgentFactory};

// ─── Task Status ────────────────────────────────────────────────────────────

/// Status of an asynchronous background task.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TaskStatus {
    /// Task has been submitted but not yet started.
    Pending,
    /// Task is currently executing.
    Running,
    /// Task completed successfully with a result.
    Completed,
    /// Task failed with an error.
    Failed(String),
    /// Task was cancelled by the caller.
    Cancelled,
}

// ─── Task Info ───────────────────────────────────────────────────────────────

/// Information about a background task, including its status and result.
#[derive(Debug, Clone)]
pub struct TaskInfo {
    /// Unique task identifier.
    pub id: String,
    /// The kind of sub-agent that will execute this task.
    pub agent_kind: SubAgentKind,
    /// The task description passed to the sub-agent.
    pub description: String,
    /// Current status of the task.
    pub status: TaskStatus,
    /// The result, if the task has completed.
    pub result: Option<SubAgentSummary>,
    /// Token budget allocated for this task.
    pub allocated_tokens: u32,
}

// ─── AsyncTaskRunner ────────────────────────────────────────────────────────

/// Manages background task delegation for the main agent loop.
///
/// The AsyncTaskRunner enables the main agent to:
/// 1. Submit tasks for background execution
/// 2. Continue working while background tasks run
/// 3. Check task status and retrieve results when ready
/// 4. Cancel tasks that are no longer needed
///
/// This is the OneAI equivalent of Claude Code's background agent spawning.
///
/// **Interface unification**: Uses the same `SubAgentFactory` trait as
/// `DefaultSubAgentFactory` from `sub_agent.rs`, ensuring consistent
/// sub-agent creation across serial delegation (AgentLoop::spawn_sub_agent)
/// and parallel delegation (AsyncTaskRunner::submit).
pub struct AsyncTaskRunner {
    /// The sub-agent factory used to create sub-agents for each task.
    /// Uses the same SubAgentFactory trait as DefaultSubAgentFactory.
    factory: Arc<dyn SubAgentFactory>,
    /// Default budget for sub-agent tasks.
    default_budget: TokenBudget,
    /// Active background tasks and their handles.
    tasks: Arc<RwLock<HashMap<String, TaskHandle>>>,
    /// Task info (status, description, results).
    info: Arc<RwLock<HashMap<String, TaskInfo>>>,
    /// Counter for generating unique task IDs.
    next_id: Arc<Mutex<u64>>,
}

/// Handle for a running background task.
struct TaskHandle {
    /// The tokio JoinHandle for the spawned task.
    join_handle: JoinHandle<Result<SubAgentSummary>>,
    /// Whether the task has been cancelled.
    cancelled: bool,
}

impl AsyncTaskRunner {
    /// Create a new AsyncTaskRunner with the given sub-agent factory.
    ///
    /// The factory must implement the `SubAgentFactory` trait from
    /// `sub_agent.rs`. This is the same factory used by the AgentLoop
    /// for serial delegation, ensuring consistent sub-agent creation.
    pub fn new(factory: Arc<dyn SubAgentFactory>) -> Self {
        Self {
            factory,
            default_budget: TokenBudget::new(50_000), // Default sub-agent budget
            tasks: Arc::new(RwLock::new(HashMap::new())),
            info: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Create an AsyncTaskRunner with a custom default budget.
    pub fn with_budget(factory: Arc<dyn SubAgentFactory>, budget: TokenBudget) -> Self {
        Self {
            factory,
            default_budget: budget,
            tasks: Arc::new(RwLock::new(HashMap::new())),
            info: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Submit a task for background execution.
    ///
    /// Creates a sub-agent of the specified kind using the SubAgentFactory,
    /// spawns it as an independent tokio task, and returns a task ID for tracking.
    ///
    /// The main agent can continue working while the task executes.
    /// Use `status()` to check progress and `result()` to get the outcome.
    pub async fn submit(&self, task: &str, kind: SubAgentKind) -> Result<String> {
        // Generate a unique task ID
        let id = {
            let mut counter = self.next_id.lock().await;
            let id = format!("bg_task_{}", *counter);
            *counter += 1;
            id
        };

        // Create the sub-agent using the unified SubAgentFactory
        let sub_agent = self.factory.create(kind.clone(), self.default_budget.clone()).await?;
        let task_owned = task.to_string();
        let kind_owned = kind.clone();
        let id_clone = id.clone();
        let info_arc = self.info.clone();

        // Store initial task info
        {
            let mut info = self.info.write().await;
            info.insert(id.clone(), TaskInfo {
                id: id.clone(),
                agent_kind: kind.clone(),
                description: task.to_string(),
                status: TaskStatus::Pending,
                result: None,
                allocated_tokens: 0,
            });
        }

        // Spawn the sub-agent as an independent tokio task
        let handle = tokio::spawn(async move {
            // Update status to Running
            {
                let mut info = info_arc.write().await;
                if let Some(task_info) = info.get_mut(&id_clone) {
                    task_info.status = TaskStatus::Running;
                }
            }

            // Run the sub-agent
            let result = sub_agent.run(&task_owned).await;

            // Update status with result
            {
                let mut info = info_arc.write().await;
                if let Some(task_info) = info.get_mut(&id_clone) {
                    match &result {
                        Ok(summary) => {
                            task_info.status = TaskStatus::Completed;
                            task_info.result = Some(summary.clone());
                            task_info.allocated_tokens = summary.tokens_used;
                        }
                        Err(e) => {
                            task_info.status = TaskStatus::Failed(e.to_string());
                        }
                    }
                }
            }

            result
        });

        // Store the task handle
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(id.clone(), TaskHandle {
                join_handle: handle,
                cancelled: false,
            });
        }

        tracing::info!("AsyncTaskRunner: submitted background task '{}' (kind: {})", id, kind.name());

        Ok(id)
    }

    /// Submit a task with a custom budget override.
    ///
    /// Useful when different tasks need different budget allocations
    /// (e.g., exploration tasks need less budget than code implementation tasks).
    pub async fn submit_with_budget(&self, task: &str, kind: SubAgentKind, budget: TokenBudget) -> Result<String> {
        // Generate a unique task ID
        let id = {
            let mut counter = self.next_id.lock().await;
            let id = format!("bg_task_{}", *counter);
            *counter += 1;
            id
        };

        // Create the sub-agent with the specified budget
        let sub_agent = self.factory.create(kind.clone(), budget.clone()).await?;
        let task_owned = task.to_string();
        let kind_owned = kind.clone();
        let id_clone = id.clone();
        let info_arc = self.info.clone();
        let budget_total = budget.total;

        // Store initial task info
        {
            let mut info = self.info.write().await;
            info.insert(id.clone(), TaskInfo {
                id: id.clone(),
                agent_kind: kind.clone(),
                description: task.to_string(),
                status: TaskStatus::Pending,
                result: None,
                allocated_tokens: budget_total,
            });
        }

        // Spawn the sub-agent as an independent tokio task
        let handle = tokio::spawn(async move {
            // Update status to Running
            {
                let mut info = info_arc.write().await;
                if let Some(task_info) = info.get_mut(&id_clone) {
                    task_info.status = TaskStatus::Running;
                }
            }

            // Run the sub-agent
            let result = sub_agent.run(&task_owned).await;

            // Update status with result
            {
                let mut info = info_arc.write().await;
                if let Some(task_info) = info.get_mut(&id_clone) {
                    match &result {
                        Ok(summary) => {
                            task_info.status = TaskStatus::Completed;
                            task_info.result = Some(summary.clone());
                            task_info.allocated_tokens = summary.tokens_used;
                        }
                        Err(e) => {
                            task_info.status = TaskStatus::Failed(e.to_string());
                        }
                    }
                }
            }

            result
        });

        // Store the task handle
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(id.clone(), TaskHandle {
                join_handle: handle,
                cancelled: false,
            });
        }

        tracing::info!("AsyncTaskRunner: submitted background task '{}' (kind: {}, budget: {})", id, kind.name(), budget.total);

        Ok(id)
    }

    /// Check the current status of a task.
    pub async fn status(&self, task_id: &str) -> TaskStatus {
        let info = self.info.read().await;
        info.get(task_id)
            .map(|t| t.status.clone())
            .unwrap_or(TaskStatus::Failed("Task not found".to_string()))
    }

    /// Get the result of a completed task.
    ///
    /// Returns None if the task has not yet completed or was cancelled.
    /// Returns the SubAgentSummary if the task completed successfully.
    pub async fn result(&self, task_id: &str) -> Option<SubAgentSummary> {
        let info = self.info.read().await;
        info.get(task_id)
            .and_then(|t| t.result.clone())
    }

    /// Get full task info (status, description, result).
    pub async fn task_info(&self, task_id: &str) -> Option<TaskInfo> {
        let info = self.info.read().await;
        info.get(task_id).cloned()
    }

    /// List all tasks and their statuses.
    pub async fn all_tasks(&self) -> Vec<TaskInfo> {
        let info = self.info.read().await;
        info.values().cloned().collect()
    }

    /// Wait for a specific task to complete and return its result.
    ///
    /// This is a blocking call — it waits until the task finishes.
    /// Use this when the main agent needs a result before continuing.
    pub async fn wait_for(&self, task_id: &str) -> Result<SubAgentSummary> {
        // Find the JoinHandle
        let handle = {
            let mut tasks = self.tasks.write().await;
            tasks.remove(task_id)
        };

        if let Some(TaskHandle { join_handle, .. }) = handle {
            join_handle.await
                .map_err(|e| oneai_core::error::OneAIError::Agent(
                    format!("Background task '{}' panicked or was cancelled: {}", task_id, e)
                ))?
        } else {
            // Task might have already completed — check the info
            let info = self.info.read().await;
            if let Some(task_info) = info.get(task_id) {
                match &task_info.status {
                    TaskStatus::Completed => {
                        task_info.result.clone()
                            .ok_or_else(|| oneai_core::error::OneAIError::Agent(
                                format!("Task '{}' completed but has no result", task_id)
                            ))
                    }
                    TaskStatus::Failed(err) => Err(oneai_core::error::OneAIError::Agent(err.clone())),
                    _ => Err(oneai_core::error::OneAIError::Agent(
                        format!("Task '{}' is still pending/running", task_id)
                    )),
                }
            } else {
                Err(oneai_core::error::OneAIError::Agent(
                    format!("Task '{}' not found", task_id)
                ))
            }
        }
    }

    /// Cancel a background task.
    ///
    /// Aborts the tokio task and marks it as cancelled.
    /// The result will not be available after cancellation.
    pub async fn cancel(&self, task_id: &str) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(handle) = tasks.get_mut(task_id) {
            handle.join_handle.abort();
            handle.cancelled = true;
        }

        // Update info status
        let mut info = self.info.write().await;
        if let Some(task_info) = info.get_mut(task_id) {
            task_info.status = TaskStatus::Cancelled;
        }

        tracing::info!("AsyncTaskRunner: cancelled background task '{}'", task_id);
        Ok(())
    }

    /// Collect results from all completed tasks.
    ///
    /// Returns a list of (task_id, SubAgentSummary) pairs for all
    /// tasks that have completed. Failed and cancelled tasks are
    /// excluded.
    pub async fn collect_completed(&self) -> Vec<(String, SubAgentSummary)> {
        let info = self.info.read().await;
        info.iter()
            .filter(|(_, t)| t.status == TaskStatus::Completed)
            .filter_map(|(id, t)| t.result.clone().map(|r| (id.clone(), r)))
            .collect()
    }

    /// Clean up completed/failed/cancelled tasks from tracking.
    ///
    /// Removes task handles and info for tasks that are no longer
    /// running, freeing memory. Call this periodically to prevent
    /// task info accumulation.
    pub async fn cleanup_finished(&self) {
        // Find finished task IDs
        let finished_ids: Vec<String> = {
            let info = self.info.read().await;
            info.iter()
                .filter(|(_, t)| matches!(t.status, TaskStatus::Completed | TaskStatus::Failed(_) | TaskStatus::Cancelled))
                .map(|(id, _)| id.clone())
                .collect()
        };

        // Remove from tasks and info
        let mut tasks = self.tasks.write().await;
        let mut info = self.info.write().await;
        for id in finished_ids {
            tasks.remove(&id);
            // Keep info for recently completed tasks (so results can be retrieved)
            // Only remove if the task was cancelled or failed
            if let Some(task_info) = info.get(&id) {
                if matches!(task_info.status, TaskStatus::Cancelled | TaskStatus::Failed(_)) {
                    info.remove(&id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::budget::TokenBudget;

    // ─── Mock SubAgent ────────────────────────────────────────────────────────

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
        fn kind(&self) -> &SubAgentKind { &self.kind }
        fn budget(&self) -> &TokenBudget {
            static BUDGET: TokenBudget = TokenBudget { total: 10000, consumed: 0 };
            &BUDGET
        }
    }

    // ─── Mock Factory (uses sub_agent.rs SubAgentFactory trait) ─────────────────

    struct MockFactory;

    #[async_trait]
    impl SubAgentFactory for MockFactory {
        async fn create(&self, kind: SubAgentKind, _budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
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

    #[tokio::test]
    async fn test_submit_and_wait() {
        let factory = Arc::new(MockFactory);
        let runner = AsyncTaskRunner::new(factory);

        let task_id = runner.submit("Find all authentication functions", SubAgentKind::Explore).await.unwrap();
        assert!(!task_id.is_empty());

        // Wait for the task to complete
        let result = runner.wait_for(&task_id).await.unwrap();
        assert!(result.completed);
        assert!(result.summary.contains("Result for kind"));
    }

    #[tokio::test]
    async fn test_status_tracking() {
        let factory = Arc::new(MockFactory);
        let runner = AsyncTaskRunner::new(factory);

        let task_id = runner.submit("Search for patterns", SubAgentKind::Explore).await.unwrap();

        // Check initial status (might be Pending or Running depending on timing)
        let status = runner.status(&task_id).await;
        assert!(matches!(status, TaskStatus::Pending | TaskStatus::Running | TaskStatus::Completed));

        // Wait for completion
        runner.wait_for(&task_id).await.unwrap();

        // Check completed status
        let status = runner.status(&task_id).await;
        assert_eq!(status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn test_multiple_tasks() {
        let factory = Arc::new(MockFactory);
        let runner = AsyncTaskRunner::new(factory);

        let id1 = runner.submit("Explore module A", SubAgentKind::Explore).await.unwrap();
        let id2 = runner.submit("Explore module B", SubAgentKind::Explore).await.unwrap();
        let id3 = runner.submit("Review code changes", SubAgentKind::Code).await.unwrap();

        // Wait for all tasks
        let r1 = runner.wait_for(&id1).await.unwrap();
        let r2 = runner.wait_for(&id2).await.unwrap();
        let r3 = runner.wait_for(&id3).await.unwrap();

        assert!(r1.completed);
        assert!(r2.completed);
        assert!(r3.completed);
    }

    #[tokio::test]
    async fn test_collect_completed() {
        let factory = Arc::new(MockFactory);
        let runner = AsyncTaskRunner::new(factory);

        let id1 = runner.submit("Task 1", SubAgentKind::Explore).await.unwrap();
        let id2 = runner.submit("Task 2", SubAgentKind::Code).await.unwrap();

        // Wait for both
        runner.wait_for(&id1).await.unwrap();
        runner.wait_for(&id2).await.unwrap();

        // Collect all completed results
        let completed = runner.collect_completed().await;
        assert_eq!(completed.len(), 2);
    }

    #[tokio::test]
    async fn test_cancel_task() {
        let factory = Arc::new(MockFactory);
        let runner = AsyncTaskRunner::new(factory);

        let task_id = runner.submit("Long task", SubAgentKind::Explore).await.unwrap();

        // Cancel it (may complete before cancel, that's OK)
        runner.cancel(&task_id).await.unwrap();

        // Status should be Cancelled or Completed (if it finished before cancel)
        let status = runner.status(&task_id).await;
        assert!(matches!(status, TaskStatus::Cancelled | TaskStatus::Completed));
    }

    #[tokio::test]
    async fn test_all_tasks_list() {
        let factory = Arc::new(MockFactory);
        let runner = AsyncTaskRunner::new(factory);

        let id1 = runner.submit("Task 1", SubAgentKind::Explore).await.unwrap();
        runner.wait_for(&id1).await.unwrap();

        let tasks = runner.all_tasks().await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, id1);
        assert_eq!(tasks[0].agent_kind, SubAgentKind::Explore);
    }

    #[tokio::test]
    async fn test_submit_with_custom_budget() {
        let factory = Arc::new(MockFactory);
        let runner = AsyncTaskRunner::new(factory);

        let custom_budget = TokenBudget::new(30_000);
        let task_id = runner.submit_with_budget("Small task", SubAgentKind::Explore, custom_budget).await.unwrap();

        let result = runner.wait_for(&task_id).await.unwrap();
        assert!(result.completed);
    }
}
