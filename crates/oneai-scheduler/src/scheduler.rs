//! In-memory task scheduler — tokio timer-based one-shot and periodic scheduling.
//!
//! The InMemoryScheduler uses tokio timers for scheduling tasks.
//! This is the core-layer scheduler; platform-specific implementations
//! (Android WorkManager, HarmonyOS WorkScheduler, desktop daemon) will
//! be provided in Phase 6.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use oneai_core::{ScheduledTask, TaskHandle};
use oneai_core::error::Result;
use oneai_core::traits::TaskScheduler;

/// In-memory task scheduler using tokio timers.
///
/// Supports:
/// - One-shot tasks (execute once after a delay)
/// - Periodic tasks (execute repeatedly at an interval)
/// - Task cancellation
///
/// All scheduling is in-memory and does not survive process restarts.
/// For persistent scheduling, use a platform-specific implementation.
pub struct InMemoryScheduler {
    /// Running tasks keyed by task ID.
    tasks: Arc<RwLock<HashMap<String, JoinHandle<()>>>>,

    /// Task metadata (for tracking what each task does).
    task_info: Arc<RwLock<HashMap<String, ScheduledTask>>>,
}

impl InMemoryScheduler {
    /// Create a new in-memory scheduler.
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            task_info: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the number of active tasks.
    pub async fn active_task_count(&self) -> usize {
        self.tasks.read().await.len()
    }

    /// Get info about a scheduled task.
    pub async fn get_task_info(&self, task_id: &str) -> Option<ScheduledTask> {
        self.task_info.read().await.get(task_id).cloned()
    }
}

impl Default for InMemoryScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskScheduler for InMemoryScheduler {
    /// Schedule a one-shot task with a delay.
    ///
    /// The task payload is logged and the task is tracked, but
    /// actual execution of the payload requires a platform-specific
    /// callback. In the core layer, we just schedule the timer.
    async fn schedule_one_shot(&self, task: ScheduledTask, delay: std::time::Duration) -> Result<TaskHandle> {
        let task_id = task.id.clone();
        let task_name = task.name.clone();

        tracing::info!(
            "Scheduling one-shot task '{}' (id: {}) with delay {:?}",
            task_name, task_id, delay
        );

        // Store task info
        self.task_info.write().await.insert(task_id.clone(), task.clone());

        let task_id_for_spawn = task_id.clone();
        let task_name_for_spawn = task_name.clone();

        // Spawn a task that waits for the delay and then logs completion
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            tracing::info!(
                "One-shot task '{}' (id: {}) executed",
                task_name_for_spawn, task_id_for_spawn
            );
        });

        self.tasks.write().await.insert(task_id.clone(), handle);

        Ok(TaskHandle {
            task_id: task_id.clone(),
            platform_handle: format!("tokio_{}", task_id),
        })
    }

    /// Schedule a periodic task with an interval.
    ///
    /// The task will be executed repeatedly at the given interval.
    async fn schedule_periodic(&self, task: ScheduledTask, interval: std::time::Duration) -> Result<TaskHandle> {
        let task_id = task.id.clone();
        let task_name = task.name.clone();

        tracing::info!(
            "Scheduling periodic task '{}' (id: {}) with interval {:?}",
            task_name, task_id, interval
        );

        // Store task info
        self.task_info.write().await.insert(task_id.clone(), task.clone());

        let task_id_for_spawn = task_id.clone();
        let task_name_for_spawn = task_name.clone();

        // Spawn a task that runs periodically
        let handle = tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval);
            // First tick completes immediately, skip it
            interval_timer.tick().await;

            loop {
                interval_timer.tick().await;
                tracing::info!(
                    "Periodic task '{}' (id: {}) ticked",
                    task_name_for_spawn, task_id_for_spawn
                );
            }
        });

        self.tasks.write().await.insert(task_id.clone(), handle);

        Ok(TaskHandle {
            task_id: task_id.clone(),
            platform_handle: format!("tokio_periodic_{}", task_id),
        })
    }

    /// Cancel a scheduled task.
    ///
    /// Aborts the tokio task associated with the given handle.
    async fn cancel(&self, handle: &TaskHandle) -> Result<()> {
        let task_id = &handle.task_id;

        tracing::info!("Cancelling task '{}'", task_id);

        // Remove from task info
        self.task_info.write().await.remove(task_id);

        // Abort the tokio task
        if let Some(join_handle) = self.tasks.write().await.remove(task_id) {
            join_handle.abort();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::TaskScheduler;
    use std::collections::HashMap;

    fn make_task(id: &str, name: &str) -> ScheduledTask {
        ScheduledTask {
            id: id.to_string(),
            name: name.to_string(),
            payload: "test_payload".to_string(),
            metadata: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_schedule_one_shot() {
        let scheduler = InMemoryScheduler::new();

        let task = make_task("task1", "Test One-Shot");
        let handle = scheduler.schedule_one_shot(task, std::time::Duration::from_millis(100)).await.unwrap();

        assert_eq!(handle.task_id, "task1");
        assert_eq!(handle.platform_handle, "tokio_task1");

        // Task should be tracked
        assert_eq!(scheduler.active_task_count().await, 1);
        let info = scheduler.get_task_info("task1").await;
        assert!(info.is_some());
        assert_eq!(info.unwrap().name, "Test One-Shot");
    }

    #[tokio::test]
    async fn test_schedule_periodic() {
        let scheduler = InMemoryScheduler::new();

        let task = make_task("periodic1", "Test Periodic");
        let handle = scheduler.schedule_periodic(task, std::time::Duration::from_millis(100)).await.unwrap();

        assert_eq!(handle.task_id, "periodic1");
        assert_eq!(scheduler.active_task_count().await, 1);

        // Wait a bit for the periodic task to tick
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    #[tokio::test]
    async fn test_cancel_task() {
        let scheduler = InMemoryScheduler::new();

        let task = make_task("cancel1", "Test Cancel");
        let handle = scheduler.schedule_one_shot(task, std::time::Duration::from_secs(10)).await.unwrap();

        assert_eq!(scheduler.active_task_count().await, 1);

        // Cancel the task
        scheduler.cancel(&handle).await.unwrap();

        // Task should be removed
        assert_eq!(scheduler.active_task_count().await, 0);
        let info = scheduler.get_task_info("cancel1").await;
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn test_cancel_periodic_task() {
        let scheduler = InMemoryScheduler::new();

        let task = make_task("cancel_periodic", "Test Cancel Periodic");
        let handle = scheduler.schedule_periodic(task, std::time::Duration::from_millis(50)).await.unwrap();

        // Wait for a tick
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Cancel
        scheduler.cancel(&handle).await.unwrap();
        assert_eq!(scheduler.active_task_count().await, 0);
    }

    #[tokio::test]
    async fn test_cancel_nonexistent_task() {
        let scheduler = InMemoryScheduler::new();

        let handle = TaskHandle {
            task_id: "nonexistent".to_string(),
            platform_handle: "tokio_nonexistent".to_string(),
        };

        // Cancel should succeed even if task doesn't exist
        let result = scheduler.cancel(&handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_multiple_tasks() {
        let scheduler = InMemoryScheduler::new();

        let task1 = make_task("task1", "Task 1");
        let task2 = make_task("task2", "Task 2");
        let task3 = make_task("task3", "Task 3");

        scheduler.schedule_one_shot(task1, std::time::Duration::from_millis(100)).await.unwrap();
        scheduler.schedule_periodic(task2, std::time::Duration::from_millis(100)).await.unwrap();
        scheduler.schedule_one_shot(task3, std::time::Duration::from_millis(100)).await.unwrap();

        assert_eq!(scheduler.active_task_count().await, 3);
    }
}