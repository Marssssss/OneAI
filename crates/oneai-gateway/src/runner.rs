//! The runner seam — the one place the gateway touches the agent.
//!
//! `oneai-gateway` sits *below* `oneai-app` (no `oneai-*` deps) so it cannot
//! call `run_agent` directly. [`GatewayRunner`] mirrors `StudioRunner`:
//! the CLI builds a real `App` + `AppSession`, calls
//! `app.create_session_with_id(session_id)` + `session.run_agent_silent(task)`
//! (or `run_agent` with a streaming observer — see [`GatewayRunner::run_turn_streaming`]),
//! and supplies the impl. The gateway core calls [`GatewayRunner::run_turn`] /
//! [`GatewayRunner::run_turn_streaming`] and relays the reply back via the
//! platform adapter.

use std::sync::Arc;

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

/// A sink the runner pushes intermediate assistant chunks into for *streaming
/// reply* (tail #3 of evolution-plan §3.1). The gateway core constructs a
/// concrete [`crate::platform::MessagePlatform`]-backed sink and hands it to
/// [`GatewayRunner::run_turn_streaming`]; the CLI runner wires its
/// `on_stream_chunk` observer to [`ReplySink::push`].
///
/// `push` is **sync** because `AgentLoopObserver::on_stream_chunk` is a sync
/// callback — the sink accumulates internally and a background coalescer
/// flushes to the platform. [`ReplySink::finalize`] drains pending chunks
/// after the turn ends; [`ReplySink::did_stream`] lets the gateway core skip
/// the final segment-send when the reply was already streamed (dedup).
#[async_trait]
pub trait ReplySink: Send + Sync {
    /// Push an intermediate assistant chunk (sync — from the observer).
    fn push(&self, text: &str);
    /// Whether any chunk was pushed — used by the gateway to decide whether
    /// to skip the final segment-send (dedup).
    fn did_stream(&self) -> bool {
        false
    }
    /// Drain pending chunks and flush them to the platform. Called after the
    /// turn ends so trailing chunks aren't lost.
    async fn finalize(&self);
}

/// The seam the CLI implements to drive a real `AgentLoop` turn.
///
/// Implementations hold an `Arc<oneai_app::App>` and, per call:
/// 1. `app.create_session_with_id(session_id).await` — reloads the channel's
///    persisted conversation history (if SQLite persistence is on).
/// 2. `session.run_agent_silent(task).await` (or `run_agent` with a streaming
///    observer when a [`ReplySink`] is supplied) — runs the loop, auto-saves.
/// 3. Maps `AgentLoopResult { final_answer, completed, iterations }` into
///    [`TurnOutcome::Done`].
#[async_trait]
pub trait GatewayRunner: Send + Sync {
    async fn run_turn(&self, session_id: &str, task: &str) -> TurnOutcome;

    /// Drive a turn, streaming intermediate assistant chunks to `sink`.
    ///
    /// Default impl falls back to non-streaming `run_turn` (drops chunks —
    /// backward-compatible with runners that haven't wired a streaming
    /// observer). The gateway core only calls this when both the platform
    /// `supports_streaming_reply()` and [`GatewayRunner::supports_streaming`]
    /// are true.
    async fn run_turn_streaming(
        &self,
        session_id: &str,
        task: &str,
        _sink: Arc<dyn ReplySink>,
    ) -> TurnOutcome {
        self.run_turn(session_id, task).await
    }

    /// Whether this runner actually streams chunks (vs the no-op default that
    /// falls back to `run_turn`).
    fn supports_streaming(&self) -> bool {
        false
    }
}
