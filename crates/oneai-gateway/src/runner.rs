//! The runner seam — the one place the gateway touches the agent.
//!
//! `oneai-gateway` sits *below* `oneai-app` (no `oneai-*` deps) so it cannot
//! call `run_agent` directly. [`GatewayRunner`] mirrors `StudioRunner`:
//! the CLI builds a real `App` + `AppSession`, calls
//! `app.create_session_with_id(session_id)` + `session.run_agent_silent(task)`,
//! and supplies the impl. The gateway core calls [`GatewayRunner::run_turn`]
//! and relays the `final_answer` back via the platform adapter.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Outcome of a gateway-driven agent turn — the gateway core reads
/// `final_answer` to relay back over the platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnOutcome {
    /// The turn completed. `final_answer` is the agent's reply to send.
    Done {
        final_answer: String,
        completed: bool,
        iterations: usize,
    },
    /// The runner could not start the turn (no provider configured, busy).
    Rejected { reason: String },
    /// The turn failed with an error.
    Error { message: String },
}

impl TurnOutcome {
    /// The final answer text, if the turn completed with one.
    pub fn final_answer(&self) -> Option<&str> {
        match self {
            TurnOutcome::Done { final_answer, .. } => Some(final_answer),
            _ => None,
        }
    }
}

/// The seam the CLI implements to drive a real `AgentLoop` turn.
///
/// Implementations hold an `Arc<oneai_app::App>` and, per call:
/// 1. `app.create_session_with_id(session_id).await` — reloads the channel's
///    persisted conversation history (if SQLite persistence is on).
/// 2. `session.run_agent_silent(task).await` — runs the loop, auto-saves.
/// 3. Maps `AgentLoopResult { final_answer, completed, iterations }` into
///    [`TurnOutcome::Done`].
#[async_trait]
pub trait GatewayRunner: Send + Sync {
    async fn run_turn(&self, session_id: &str, task: &str) -> TurnOutcome;
}
