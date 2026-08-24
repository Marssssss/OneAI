//! A2A Handler — processes A2A JSON-RPC protocol messages on the server side.
//!
//! The A2AHandler implements the server-side A2A protocol:
//! - `agent/getCard` → return cached AgentCard
//! - `tasks/send` → create Task, drive the [`A2ARunner`] (real AgentLoop when
//!   the CLI injects an App-backed runner; [`PlaceholderRunner`] ack by
//!   default), return the terminal Task
//! - `tasks/get` → retrieve Task from TaskStore
//! - `tasks/cancel` → transition Task to Canceled state
//! - `tasks/sendSubscribe` → non-streaming fallback returns the final Task;
//!   real SSE streaming is handled at the axum layer (see [`crate::server`])

use std::sync::Arc;

use crate::runner::{A2ARunner, PlaceholderRunner, TaskOutcome};
use crate::task_store::TaskStore;
use crate::types::{
    AgentCard, Artifact, CancelTaskParams, GetTaskParams, Message, Part, SendTaskParams, TaskState,
};

/// A2A JSON-RPC request handler.
///
/// Processes incoming A2A protocol messages and produces appropriate
/// JSON-RPC responses. Each method follows the A2A specification.
pub struct A2AHandler {
    /// Cached AgentCard for this agent.
    agent_card: AgentCard,
    /// Task store for managing task lifecycle.
    task_store: Arc<TaskStore>,
    /// The runner that drives a real agent turn on `tasks/send`. Defaults to
    /// [`PlaceholderRunner`] (pre-3.5 ack); the CLI injects an App-backed
    /// runner via [`A2AHandler::with_runner`].
    runner: Arc<dyn A2ARunner>,
}

impl A2AHandler {
    /// Create a new handler with an AgentCard and TaskStore.
    ///
    /// Defaults to a [`PlaceholderRunner`] (no real AgentLoop) — use
    /// [`A2AHandler::with_runner`] to inject an App-backed runner.
    pub fn new(agent_card: AgentCard, task_store: Arc<TaskStore>) -> Self {
        let runner: Arc<dyn A2ARunner> = Arc::new(PlaceholderRunner::new(agent_card.skills.len()));
        Self {
            agent_card,
            task_store,
            runner,
        }
    }

    /// Builder: inject the runner that drives a real agent turn on
    /// `tasks/send` / `tasks/sendSubscribe`.
    pub fn with_runner(mut self, runner: Arc<dyn A2ARunner>) -> Self {
        self.runner = runner;
        self
    }

    /// The runner currently wired into this handler.
    pub fn runner(&self) -> &Arc<dyn A2ARunner> {
        &self.runner
    }

    /// Extract the user's text from a `tasks/send` Message by concatenating
    /// its Text parts. Non-Text parts (File/Data) are rejected — the agent
    /// surface is text-in/text-out and the card advertises
    /// `default_input_modes: ["text/plain"]`.
    pub fn extract_text(message: &Message) -> Result<String, &'static str> {
        let mut text = String::new();
        for part in &message.parts {
            match part {
                Part::Text { text: t } => text.push_str(t),
                _ => {
                    return Err(
                        "only text parts are supported for tasks/send (File/Data parts rejected)",
                    )
                }
            }
        }
        if text.trim().is_empty() {
            return Err("message text is empty");
        }
        Ok(text)
    }

    /// Handle `agent/getCard` request — return the cached AgentCard.
    pub async fn handle_get_card(&self, id: Option<serde_json::Value>) -> serde_json::Value {
        let card_json = serde_json::to_value(&self.agent_card).unwrap_or_else(
            |e| serde_json::json!({"error": format!("Serialization error: {}", e)}),
        );

        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": card_json,
        })
    }

    /// Handle `tasks/send` request — create a Task and drive the runner.
    ///
    /// Creates a Task in `Submitted`, transitions to `Working`, extracts the
    /// user's text from the message, calls [`A2ARunner::run_task`], and maps
    /// the [`TaskOutcome`] to a terminal state: `Done` → `Completed` with a
    /// text Artifact; `Rejected`/`Error` → `Failed` with the reason.
    pub async fn handle_send_task(
        &self,
        id: Option<serde_json::Value>,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        // Parse the SendTaskParams
        let send_params: SendTaskParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": format!("Invalid params for tasks/send: {}", e),
                    }
                });
            }
        };

        // Extract the user's text from the message (File/Data parts rejected)
        let message_text = match Self::extract_text(&send_params.message) {
            Ok(t) => t,
            Err(msg) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": format!("Invalid params for tasks/send: {msg}"),
                    }
                });
            }
        };

        // Bind the A2A task to a session id so multi-turn tasks continue the
        // conversation (falls back to the task id when no session id is given).
        let session_id = send_params
            .session_id
            .clone()
            .unwrap_or_else(|| send_params.id.clone());

        // Create the task
        let _task = match self
            .task_store
            .create_task(&send_params.id, send_params.message.clone())
            .await
        {
            Ok(t) => t,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!("Task creation error: {}", e),
                    }
                });
            }
        };

        // Transition to Working state
        let _task = match self
            .task_store
            .transition_task(&send_params.id, TaskState::Working)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!("Task transition error: {}", e),
                    }
                });
            }
        };

        // gap P0 #4 — inbound W3C traceparent (injected into params metadata
        // by the axum layer from the HTTP header). Threaded to the runner so
        // trace-aware implementations attach the distributed trace.
        let traceparent = send_params
            .metadata
            .as_ref()
            .and_then(|m| m.get("traceparent"))
            .and_then(|v| v.as_str())
            .filter(|tp| oneai_trace::parse_traceparent(tp).is_some())
            .map(|tp| tp.to_string());
        if let Some(tp) = &traceparent {
            tracing::info!("A2A tasks/send continuing inbound trace: {}", tp);
        }

        // Drive the runner — a real AgentLoop turn when the CLI injected an
        // App-backed runner (PlaceholderRunner ack by default).
        let outcome = self
            .runner
            .run_task_with_trace(&session_id, &message_text, traceparent.as_deref(), None)
            .await;

        // Map the outcome to a terminal Task state.
        let task_result = match &outcome {
            TaskOutcome::Done { final_answer, .. } => {
                let artifact = Artifact::text("response", final_answer.clone());
                self.task_store
                    .complete_task(&send_params.id, Some(artifact))
                    .await
            }
            TaskOutcome::Rejected { reason } | TaskOutcome::Error { message: reason } => {
                self.task_store.fail_task(&send_params.id, reason).await
            }
        };

        match task_result {
            Ok(terminal_task) => {
                let task_json = serde_json::to_value(&terminal_task).unwrap_or_else(
                    |e| serde_json::json!({"error": format!("Serialization error: {}", e)}),
                );

                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": task_json,
                })
            }
            Err(e) => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!("Task completion error: {}", e),
                    }
                })
            }
        }
    }

    /// Handle `tasks/get` request — retrieve an existing task.
    pub async fn handle_get_task(
        &self,
        id: Option<serde_json::Value>,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        let get_params: GetTaskParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": format!("Invalid params for tasks/get: {}", e),
                    }
                });
            }
        };

        match self.task_store.get_task(&get_params.id).await {
            Ok(task) => {
                let task_json = serde_json::to_value(&task).unwrap_or_else(
                    |e| serde_json::json!({"error": format!("Serialization error: {}", e)}),
                );

                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": task_json,
                })
            }
            Err(e) => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32001,
                        "message": format!("Task not found: {}", e),
                    }
                })
            }
        }
    }

    /// Handle `tasks/cancel` request — cancel an existing task.
    pub async fn handle_cancel_task(
        &self,
        id: Option<serde_json::Value>,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        let cancel_params: CancelTaskParams = match serde_json::from_value(params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": format!("Invalid params for tasks/cancel: {}", e),
                    }
                });
            }
        };

        // Task must be in Submitted or Working state to cancel
        match self.task_store.cancel_task(&cancel_params.id).await {
            Ok(task) => {
                let task_json = serde_json::to_value(&task).unwrap_or_else(
                    |e| serde_json::json!({"error": format!("Serialization error: {}", e)}),
                );

                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": task_json,
                })
            }
            Err(e) => {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32001,
                        "message": format!("Cancel error: {}", e),
                    }
                })
            }
        }
    }

    /// Handle `tasks/sendSubscribe` — non-streaming fallback returns the final
    /// Task (the same result as `tasks/send`).
    ///
    /// Real SSE streaming (artifact chunks + status updates as they're
    /// produced) is handled at the **axum layer**, which detects
    /// `tasks/sendSubscribe` before dispatching and builds an
    /// [`axum::response::sse::Sse`] response backed by
    /// [`A2ARunner::run_task_streaming`]. This single-Value path exists so
    /// the JSON-RPC router (and its tests) still answer the method when the
    /// streaming endpoint isn't used.
    pub async fn handle_send_subscribe(
        &self,
        id: Option<serde_json::Value>,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        // Non-streaming fallback — real streaming is in the axum POST handler.
        self.handle_send_task(id, params).await
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn create_handler() -> A2AHandler {
        let card = AgentCard::new("test-agent", "A test agent", "https://test.example.com");
        let store = Arc::new(TaskStore::new());
        A2AHandler::new(card, store)
    }

    #[tokio::test]
    async fn test_handle_get_card() {
        let handler = create_handler();
        let response = handler.handle_get_card(Some(serde_json::json!(1))).await;

        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(1));
        let result = response.get("result").unwrap();
        assert_eq!(
            result.get("name").and_then(|n| n.as_str()),
            Some("test-agent")
        );
    }

    #[tokio::test]
    async fn test_handle_send_task() {
        let handler = create_handler();
        let params = serde_json::json!({
            "id": "task-test-1",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Hello agent"}]
            }
        });

        let response = handler
            .handle_send_task(Some(serde_json::json!(2)), &params)
            .await;
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(2));

        let result = response.get("result").unwrap();
        assert_eq!(
            result.get("id").and_then(|v| v.as_str()),
            Some("task-test-1")
        );
        // Task should be Completed
        let status = result.get("status").unwrap();
        assert_eq!(
            status.get("state").and_then(|s| s.as_str()),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn test_handle_get_task() {
        let handler = create_handler();
        // First create a task
        let send_params = serde_json::json!({
            "id": "task-test-2",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Find info"}]
            }
        });
        handler
            .handle_send_task(Some(serde_json::json!(1)), &send_params)
            .await;

        // Then get the task
        let get_params = serde_json::json!({
            "id": "task-test-2"
        });
        let response = handler
            .handle_get_task(Some(serde_json::json!(2)), &get_params)
            .await;
        assert_eq!(response.get("id").and_then(|v| v.as_u64()), Some(2));

        let result = response.get("result").unwrap();
        assert_eq!(
            result.get("id").and_then(|v| v.as_str()),
            Some("task-test-2")
        );
    }

    #[tokio::test]
    async fn test_handle_get_nonexistent_task() {
        let handler = create_handler();
        let get_params = serde_json::json!({
            "id": "nonexistent"
        });
        let response = handler
            .handle_get_task(Some(serde_json::json!(3)), &get_params)
            .await;

        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32001));
    }

    #[tokio::test]
    async fn test_handle_cancel_task() {
        let handler = create_handler();
        // Create a task
        let send_params = serde_json::json!({
            "id": "task-test-3",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Cancel this"}]
            }
        });
        handler
            .handle_send_task(Some(serde_json::json!(1)), &send_params)
            .await;

        // Cancel the task (note: already completed, so this will fail —
        // let's create a task that stays in Working state instead)
        let store = Arc::new(TaskStore::new());
        let card = AgentCard::new("cancel-agent", "Cancel test", "https://cancel.example.com");
        let handler = A2AHandler::new(card, store.clone());

        // Manually create a task in Working state
        store
            .create_task("task-cancel", Message::user_text("Working task"))
            .await
            .unwrap();
        store
            .transition_task("task-cancel", TaskState::Working)
            .await
            .unwrap();

        let cancel_params = serde_json::json!({
            "id": "task-cancel"
        });
        let response = handler
            .handle_cancel_task(Some(serde_json::json!(2)), &cancel_params)
            .await;

        let result = response.get("result").unwrap();
        let status = result.get("status").unwrap();
        assert_eq!(
            status.get("state").and_then(|s| s.as_str()),
            Some("canceled")
        );
    }

    #[tokio::test]
    async fn test_handle_send_task_invalid_params() {
        let handler = create_handler();
        let params = serde_json::json!({"invalid": true});

        let response = handler
            .handle_send_task(Some(serde_json::json!(4)), &params)
            .await;
        let error = response.get("error").unwrap();
        assert_eq!(error.get("code").and_then(|c| c.as_i64()), Some(-32602));
    }

    #[tokio::test]
    async fn test_handle_send_subscribe_delegates_to_send() {
        let handler = create_handler();
        let params = serde_json::json!({
            "id": "task-subscribe-1",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "Stream test"}]
            }
        });

        let response = handler
            .handle_send_subscribe(Some(serde_json::json!(5)), &params)
            .await;
        // Should produce the same result as handle_send_task
        assert!(response.get("result").is_some());
    }

    // ─── Inbound W3C traceparent propagation (gap P0 #4) ──────────────────

    /// Runner that captures the traceparent handed to `run_task_with_trace`.
    struct CapturingRunner {
        captured: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    #[async_trait::async_trait]
    impl A2ARunner for CapturingRunner {
        async fn run_task(&self, _session_id: &str, _message_text: &str) -> TaskOutcome {
            TaskOutcome::Done {
                final_answer: "ok".to_string(),
                completed: true,
                iterations: 1,
            }
        }

        async fn run_task_with_trace(
            &self,
            session_id: &str,
            message_text: &str,
            traceparent: Option<&str>,
            _sink: Option<Arc<dyn crate::runner::A2ASseSink>>,
        ) -> TaskOutcome {
            self.captured
                .lock()
                .unwrap()
                .push(traceparent.map(|s| s.to_string()));
            self.run_task(session_id, message_text).await
        }
    }

    fn params_with_traceparent(task_id: &str, traceparent: &str) -> serde_json::Value {
        serde_json::json!({
            "id": task_id,
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "distributed call"}]
            },
            "metadata": { "traceparent": traceparent }
        })
    }

    #[tokio::test]
    async fn valid_inbound_traceparent_reaches_runner() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let card = AgentCard::new("trace-agent", "t", "https://t.example.com");
        let handler = A2AHandler::new(card, Arc::new(TaskStore::new())).with_runner(Arc::new(
            CapturingRunner {
                captured: captured.clone(),
            },
        ));

        let tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        let response = handler
            .handle_send_task(
                Some(serde_json::json!(1)),
                &params_with_traceparent("task-tp-1", tp),
            )
            .await;
        assert!(response.get("result").is_some());

        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].as_deref(), Some(tp));
    }

    #[tokio::test]
    async fn malformed_inbound_traceparent_is_dropped() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let card = AgentCard::new("trace-agent", "t", "https://t.example.com");
        let handler = A2AHandler::new(card, Arc::new(TaskStore::new())).with_runner(Arc::new(
            CapturingRunner {
                captured: captured.clone(),
            },
        ));

        let response = handler
            .handle_send_task(
                Some(serde_json::json!(1)),
                &params_with_traceparent("task-tp-2", "garbage-not-a-traceparent"),
            )
            .await;
        assert!(response.get("result").is_some());

        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], None); // malformed → not propagated
    }

    #[tokio::test]
    async fn no_metadata_means_no_traceparent() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let card = AgentCard::new("trace-agent", "t", "https://t.example.com");
        let handler = A2AHandler::new(card, Arc::new(TaskStore::new())).with_runner(Arc::new(
            CapturingRunner {
                captured: captured.clone(),
            },
        ));

        let params = serde_json::json!({
            "id": "task-tp-3",
            "message": {
                "role": "user",
                "parts": [{"type": "text", "text": "plain call"}]
            }
        });
        let response = handler
            .handle_send_task(Some(serde_json::json!(1)), &params)
            .await;
        assert!(response.get("result").is_some());
        assert_eq!(captured.lock().unwrap()[0], None);
    }
}
