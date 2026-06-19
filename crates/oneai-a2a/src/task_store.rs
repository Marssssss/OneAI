//! A2A Task Store — in-memory task lifecycle management.
//!
//! The TaskStore manages the lifecycle of A2A Tasks within the server.
//! It provides concurrent access via `Arc<RwLock<HashMap>>` and enforces
//! the A2A TaskState transition rules.
//!
//! ## Usage
//! ```ignore
//! let store = TaskStore::new();
//!
//! // Create a task
//! let task = store.create_task("task-001", Message::user_text("Analyze this code"))?;
//!
//! // Transition to Working state
//! store.transition_task("task-001", TaskState::Working)?;
//!
//! // Get the task
//! let task = store.get_task("task-001")?;
//!
//! // Cancel a task
//! store.transition_task("task-001", TaskState::Canceled)?;
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::types::{Task, TaskState, Message};
use crate::error::{A2AError, Result};

// ─── TaskStore ────────────────────────────────────────────────────────────────

/// In-memory task store with concurrent access.
///
/// Manages A2A Task lifecycle: creation, retrieval, state transitions,
/// and cancellation. All operations are thread-safe via `RwLock`.
///
/// SQLite persistence is deferred to a later phase — the in-memory
/// store is sufficient for P4-1.
pub struct TaskStore {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
}

impl TaskStore {
    /// Create a new empty task store.
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new task with the given ID and initial message.
    ///
    /// The task starts in `Submitted` state with the given message
    /// added to the history.
    pub async fn create_task(&self, id: &str, message: Message) -> Result<Task> {
        let mut task = Task::new(id);
        task.add_message(message);

        let mut tasks = self.tasks.write().await;
        if tasks.contains_key(id) {
            return Err(A2AError::Server(format!("Task '{}' already exists", id)));
        }
        tasks.insert(id.to_string(), task.clone());
        Ok(task)
    }

    /// Get a task by ID.
    pub async fn get_task(&self, id: &str) -> Result<Task> {
        let tasks = self.tasks.read().await;
        tasks.get(id).cloned().ok_or_else(|| A2AError::TaskNotFound(id.to_string()))
    }

    /// Transition a task to a new state.
    ///
    /// Validates the transition using `TaskState::validate_transition()`.
    /// Returns the updated task.
    pub async fn transition_task(&self, id: &str, new_state: TaskState) -> Result<Task> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id).ok_or_else(|| A2AError::TaskNotFound(id.to_string()))?;

        task.transition_to(new_state).map_err(|e| {
            // Convert A2AError::InvalidStateTransition from the task's error type
            match e {
                A2AError::InvalidStateTransition { from, to } => {
                    A2AError::InvalidStateTransition { from, to }
                }
                other => other,
            }
        })?;

        Ok(task.clone())
    }

    /// Add a message to a task's history.
    pub async fn add_message(&self, id: &str, message: Message) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id).ok_or_else(|| A2AError::TaskNotFound(id.to_string()))?;
        task.add_message(message);
        Ok(())
    }

    /// Add an artifact to a task.
    pub async fn add_artifact(&self, id: &str, artifact: crate::types::Artifact) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id).ok_or_else(|| A2AError::TaskNotFound(id.to_string()))?;
        task.add_artifact(artifact);
        Ok(())
    }

    /// Cancel a task (transition to Canceled state).
    pub async fn cancel_task(&self, id: &str) -> Result<Task> {
        self.transition_task(id, TaskState::Canceled).await
    }

    /// Complete a task (transition to Completed state with optional artifact).
    pub async fn complete_task(&self, id: &str, artifact: Option<crate::types::Artifact>) -> Result<Task> {
        if let Some(art) = artifact {
            self.add_artifact(id, art).await?;
        }
        self.transition_task(id, TaskState::Completed).await
    }

    /// Fail a task (transition to Failed state with error message).
    pub async fn fail_task(&self, id: &str, error_message: &str) -> Result<Task> {
        self.add_message(id, Message::agent_text(error_message)).await?;
        self.transition_task(id, TaskState::Failed).await
    }

    /// Request input from the client (transition to InputRequired state).
    pub async fn require_input(&self, id: &str, message: Message) -> Result<Task> {
        self.add_message(id, message).await?;
        self.transition_task(id, TaskState::InputRequired).await
    }

    /// List all tasks in the store.
    pub async fn list_tasks(&self) -> Vec<Task> {
        let tasks = self.tasks.read().await;
        tasks.values().cloned().collect()
    }

    /// Remove a task from the store.
    pub async fn remove_task(&self, id: &str) -> Result<Task> {
        let mut tasks = self.tasks.write().await;
        tasks.remove(id).ok_or_else(|| A2AError::TaskNotFound(id.to_string()))
    }
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Artifact;

    #[tokio::test]
    async fn test_create_task() {
        let store = TaskStore::new();
        let task = store.create_task("task-001", Message::user_text("Hello")).await.unwrap();
        assert_eq!(task.id, "task-001");
        assert_eq!(task.status.state, TaskState::Submitted);
        assert_eq!(task.history.len(), 1);
    }

    #[tokio::test]
    async fn test_get_task() {
        let store = TaskStore::new();
        store.create_task("task-002", Message::user_text("Test")).await.unwrap();
        let task = store.get_task("task-002").await.unwrap();
        assert_eq!(task.id, "task-002");
    }

    #[tokio::test]
    async fn test_get_nonexistent_task() {
        let store = TaskStore::new();
        let result = store.get_task("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_duplicate_task_id() {
        let store = TaskStore::new();
        store.create_task("task-003", Message::user_text("First")).await.unwrap();
        let result = store.create_task("task-003", Message::user_text("Second")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_valid_state_transitions() {
        let store = TaskStore::new();
        store.create_task("task-004", Message::user_text("Work")).await.unwrap();

        // Submitted → Working
        let task = store.transition_task("task-004", TaskState::Working).await.unwrap();
        assert_eq!(task.status.state, TaskState::Working);

        // Working → Completed
        let task = store.transition_task("task-004", TaskState::Completed).await.unwrap();
        assert_eq!(task.status.state, TaskState::Completed);
    }

    #[tokio::test]
    async fn test_invalid_state_transition() {
        let store = TaskStore::new();
        store.create_task("task-005", Message::user_text("Skip")).await.unwrap();

        // Submitted → Completed is invalid
        let result = store.transition_task("task-005", TaskState::Completed).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cancel_task() {
        let store = TaskStore::new();
        store.create_task("task-006", Message::user_text("Cancel me")).await.unwrap();
        store.transition_task("task-006", TaskState::Working).await.unwrap();

        let task = store.cancel_task("task-006").await.unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn test_complete_task_with_artifact() {
        let store = TaskStore::new();
        store.create_task("task-007", Message::user_text("Compute")).await.unwrap();
        store.transition_task("task-007", TaskState::Working).await.unwrap();

        let artifact = Artifact::text("result", "42");
        let task = store.complete_task("task-007", Some(artifact)).await.unwrap();
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(task.artifacts.len(), 1);
    }

    #[tokio::test]
    async fn test_fail_task() {
        let store = TaskStore::new();
        store.create_task("task-008", Message::user_text("Fail")).await.unwrap();
        store.transition_task("task-008", TaskState::Working).await.unwrap();

        let task = store.fail_task("task-008", "Something went wrong").await.unwrap();
        assert_eq!(task.status.state, TaskState::Failed);
        // History: 1 (user create) + 1 (agent error msg from fail_task)
        assert_eq!(task.history.len(), 2);
    }

    #[tokio::test]
    async fn test_require_input() {
        let store = TaskStore::new();
        store.create_task("task-009", Message::user_text("Input?")).await.unwrap();
        store.transition_task("task-009", TaskState::Working).await.unwrap();

        let task = store.require_input("task-009", Message::agent_text("Need more info")).await.unwrap();
        assert_eq!(task.status.state, TaskState::InputRequired);
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let store = TaskStore::new();
        store.create_task("t1", Message::user_text("1")).await.unwrap();
        store.create_task("t2", Message::user_text("2")).await.unwrap();

        let tasks = store.list_tasks().await;
        assert_eq!(tasks.len(), 2);
    }

    #[tokio::test]
    async fn test_remove_task() {
        let store = TaskStore::new();
        store.create_task("task-010", Message::user_text("Remove")).await.unwrap();

        let removed = store.remove_task("task-010").await.unwrap();
        assert_eq!(removed.id, "task-010");

        let result = store.get_task("task-010").await;
        assert!(result.is_err());
    }
}
