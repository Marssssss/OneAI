//! The runner seam — the one place the A2A server touches the agent.
//!
//! `oneai-a2a` sits *below* `oneai-app` (no `oneai-app`/`oneai-agent` deps)
//! so it cannot call `run_agent` directly. [`A2ARunner`] mirrors
//! `oneai_gateway::GatewayRunner`: the CLI builds a real `App` +
//! `AppSession`, calls `app.create_session_with_id(session_id)` +
//! `session.run_agent_silent(task)` (or `run_agent` with a streaming
//! observer when an [`A2ASseSink`] is supplied), and supplies the impl. The
//! A2A server core calls [`A2ARunner::run_task`] /
//! [`A2ARunner::run_task_streaming`] and returns the reply as a Task +
//! Artifact (or, for the streaming path, SSE events pushed through the sink).
//!
//! ## Why this crate does NOT depend on `oneai-bus` (P5 decision)
//!
//! `tasks/sendSubscribe`'s SSE payload is the A2A-protocol-mandated
//! `task` / `status` / `artifact` shape (see `transport::TaskStreamEvent`),
//! NOT `EngineYield` — a remote A2A client speaks that protocol and would
//! break if the payload were swapped. The other two wires (studio WS,
//! supervisor IPC) converged to `EngineYield` in P5-A/P5-B because they are
//! *local* frontends that own both ends. A2A is inter-agent: only the codec
//! could be shared, and `serde_json` already is the codec. Bus-feeding the
//! SSE stream (subscribe to an `InProcessBus`, map each `EngineYield` to an
//! A2A `task`/`status`/`artifact` SSE event) is therefore a **CLI-side
//! concern**: a future `A2ARunner` impl may do that mapping inside
//! `run_task_streaming` without this crate gaining a `oneai-bus` dependency.
//! Keeping a2a bus-free preserves the protocol boundary (supply-chain
//! discipline — no new cross-crate dep for zero wire-level gain).
//!
//! The default [`PlaceholderRunner`] reproduces the pre-3.5 placeholder
//! ack ("Task received and processed. N skills available.") so the existing
//! handler/router/server unit tests — which assert "task transitions to
//! `completed`" — stay green unchanged (evolution-plan 戒律 #7: don't let the
//! fix break what it protected). The real CLI injects [`A2ARunner`] via
//! [`crate::server::A2AServerHost::with_runner`] /
//! [`crate::handler::A2AHandler::with_runner`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::types::TaskState;

/// Outcome of an A2A-driven agent turn — the server core reads `final_answer`
/// to build the Task's terminal Artifact (or `Failed` status on error/reject).
///
/// Mirrors `oneai_gateway::TurnOutcome` (re-defined locally because a2a does
/// not depend on the gateway crate — per supply-chain discipline, no new
/// cross-crate dep for one small enum).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TaskOutcome {
    /// The turn completed. `final_answer` is the agent's reply (→ Artifact).
    Done {
        final_answer: String,
        completed: bool,
        iterations: usize,
    },
    /// The runner refused to start the turn (no LLM provider configured, etc).
    /// Maps to `TaskState::Failed` with `reason` as the error message.
    Rejected { reason: String },
    /// The turn errored mid-flight. Maps to `TaskState::Failed`.
    Error { message: String },
}

impl TaskOutcome {
    /// The final answer text, if the turn completed with one.
    pub fn final_answer(&self) -> Option<&str> {
        match self {
            TaskOutcome::Done { final_answer, .. } => Some(final_answer),
            _ => None,
        }
    }

    /// The terminal [`TaskState`] this outcome implies.
    pub fn terminal_state(&self) -> TaskState {
        match self {
            TaskOutcome::Done { .. } => TaskState::Completed,
            TaskOutcome::Rejected { .. } | TaskOutcome::Error { .. } => TaskState::Failed,
        }
    }

    /// The failure reason, if this outcome is a rejection or error.
    pub fn failure_reason(&self) -> Option<&str> {
        match self {
            TaskOutcome::Rejected { reason } => Some(reason),
            TaskOutcome::Error { message } => Some(message),
            _ => None,
        }
    }
}

/// A sink the runner pushes intermediate assistant chunks + status changes
/// into for *SSE streaming reply* (§3.5). The server constructs a channel
/// backed [`A2ASseSink`] and hands it to [`A2ARunner::run_task_streaming`];
/// the CLI runner wires its `on_stream_chunk` observer to [`A2ASseSink::push_chunk`].
///
/// Methods are **sync** because `AgentLoopObserver::on_stream_chunk` is a sync
/// callback — the sink accumulates internally and a background flusher drains
/// to the SSE channel.
pub trait A2ASseSink: Send + Sync {
    /// Push an intermediate assistant text fragment (→ artifact SSE event).
    fn push_chunk(&self, _text: &str) {}
    /// Push a task status change (→ status SSE event).
    fn push_status(&self, _state: &TaskState) {}
}

/// The seam the CLI implements to drive a real `AgentLoop` turn on an
/// incoming A2A task message.
///
/// Implementations hold an `Arc<oneai_app::App>` and, per call:
/// 1. `app.create_session_with_id(session_id).await` — binds the A2A task to
///    a session (multi-turn continuation when the client reuses a session id).
/// 2. `session.run_agent_silent(message_text).await` (or `run_agent` with a
///    streaming observer when a sink is supplied) — runs the loop, auto-saves.
/// 3. Maps `AgentLoopResult { final_answer, completed, iterations }` into
///    [`TaskOutcome::Done`].
#[async_trait]
pub trait A2ARunner: Send + Sync {
    /// Drive a non-streaming agent turn on `message_text`. `session_id`
    /// identifies the session (so multi-turn A2A tasks continue a conversation).
    async fn run_task(&self, session_id: &str, message_text: &str) -> TaskOutcome;

    /// Drive a turn, streaming intermediate assistant chunks to `sink`.
    ///
    /// Default impl falls back to non-streaming `run_task` (drops chunks —
    /// backward-compatible with runners that haven't wired a streaming
    /// observer). The server core only calls this when
    /// [`A2ARunner::supports_streaming`] is true.
    async fn run_task_streaming(
        &self,
        session_id: &str,
        message_text: &str,
        _sink: Arc<dyn A2ASseSink>,
    ) -> TaskOutcome {
        self.run_task(session_id, message_text).await
    }

    /// Whether this runner actually streams chunks (vs the no-op default that
    /// falls back to `run_task`).
    fn supports_streaming(&self) -> bool {
        false
    }
}

// ─── PlaceholderRunner (default — preserves pre-3.5 behavior) ─────────────────

/// The default [`A2ARunner`] — reproduces the pre-3.5 placeholder ack so the
/// existing handler/router/server unit tests stay green unchanged. The real
/// CLI injects an `App`-backed runner via `with_runner`.
pub struct PlaceholderRunner {
    /// Number of "skills" to advertise in the placeholder ack (mirrors the old
    /// `self.agent_card.skills.len()` phrasing).
    pub skills_len: usize,
}

impl PlaceholderRunner {
    pub fn new(skills_len: usize) -> Self {
        Self { skills_len }
    }
}

#[async_trait]
impl A2ARunner for PlaceholderRunner {
    async fn run_task(&self, _session_id: &str, _message_text: &str) -> TaskOutcome {
        // Pre-3.5 placeholder text — preserved verbatim so the regression
        // tests asserting "task → completed" pass without modification.
        TaskOutcome::Done {
            final_answer: format!(
                "Task received and processed. Agent capabilities: {} skills available.",
                self.skills_len
            ),
            completed: true,
            iterations: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcome_terminal_state() {
        assert_eq!(
            TaskOutcome::Done {
                final_answer: "x".into(),
                completed: true,
                iterations: 2
            }
            .terminal_state(),
            TaskState::Completed
        );
        assert_eq!(
            TaskOutcome::Rejected {
                reason: "no provider".into()
            }
            .terminal_state(),
            TaskState::Failed
        );
        assert_eq!(
            TaskOutcome::Error {
                message: "boom".into()
            }
            .terminal_state(),
            TaskState::Failed
        );
    }

    #[test]
    fn test_outcome_accessors() {
        let done = TaskOutcome::Done {
            final_answer: "answer".into(),
            completed: true,
            iterations: 3,
        };
        assert_eq!(done.final_answer(), Some("answer"));
        assert!(done.failure_reason().is_none());

        let rej = TaskOutcome::Rejected {
            reason: "no provider".into(),
        };
        assert_eq!(rej.final_answer(), None);
        assert_eq!(rej.failure_reason(), Some("no provider"));
    }

    #[tokio::test]
    async fn test_placeholder_runner_reproduces_old_ack() {
        let r = PlaceholderRunner::new(5);
        let out = r.run_task("s1", "hi").await;
        assert!(matches!(out, TaskOutcome::Done { .. }));
        assert_eq!(
            out.final_answer(),
            Some("Task received and processed. Agent capabilities: 5 skills available.")
        );
        assert_eq!(out.terminal_state(), TaskState::Completed);
    }

    #[tokio::test]
    async fn test_streaming_default_falls_back_to_run_task() {
        struct NoopSink;
        impl A2ASseSink for NoopSink {}

        // A runner that records it was called via the streaming path.
        struct CountRunner;
        #[async_trait]
        impl A2ARunner for CountRunner {
            async fn run_task(&self, _: &str, _: &str) -> TaskOutcome {
                TaskOutcome::Done {
                    final_answer: "fb".into(),
                    completed: true,
                    iterations: 1,
                }
            }
            // supports_streaming stays default false
        }

        let r = CountRunner;
        assert!(!r.supports_streaming());
        let out = r
            .run_task_streaming("s", "m", Arc::new(NoopSink) as Arc<dyn A2ASseSink>)
            .await;
        // Default impl falls back to run_task → Done "fb"
        assert_eq!(out.final_answer(), Some("fb"));
    }
}
