//! The CLI-implemented runner trait (mirrors `StudioRunner`).
//!
//! `oneai-supervisor` sits *below* `oneai-app` and cannot hold an `App` /
//! `AppSession` or call `run_agent` directly (same layering constraint as
//! `oneai-studio`). Instead it defines [`SupervisorRunner`] +
//! [`InstanceHandle`]; the CLI (`examples/cli/cmd_supervisor`) builds a real
//! `App` + `AppSession` per spawned instance and supplies the impl.
//!
//! No `AppBuilder` method is added — one `AppBuilder` = one `App` = one
//! session, but the supervisor needs N per-instance sessions. This mirrors the
//! studio precedent exactly.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use oneai_agent::{AgentLoopObserver, AgentLoopResult, ParadigmKind};

use crate::error::Result;
use crate::registry::InstanceSpec;

/// The runner a CLI wires into the supervisor — a factory for instance handles.
#[async_trait]
pub trait SupervisorRunner: Send + Sync {
    /// Whether a provider is configured (so spawn can succeed).
    fn has_provider(&self) -> bool;

    /// Build a new long-lived instance for `spec`. The returned handle owns
    /// the per-instance `AppSession` (multi-turn conversation state).
    async fn spawn(&self, spec: &InstanceSpec) -> Result<Arc<dyn InstanceHandle>>;
}

/// A live, supervised instance — owns one agent session.
#[async_trait]
pub trait InstanceHandle: Send + Sync {
    /// Current lifecycle status (mostly `Idle`/`Running`; the supervisor
    /// tracks `Stopping`/`Stopped` via the registry).
    fn status(&self) -> InstanceStatus;

    /// Run one agent turn, streaming lifecycle events through `observer`.
    async fn run_turn(
        &self,
        task: &str,
        observer: Arc<dyn AgentLoopObserver>,
    ) -> Result<TurnSummary>;

    /// Request a graceful stop of the in-flight turn (if any).
    async fn stop(&self);
}

/// Re-exported for trait impls / tests.
pub use crate::registry::InstanceStatus;

/// A serializable summary of one completed agent turn.
///
/// A DTO projection of [`AgentLoopResult`] — `Conversation` / `GlobalState` /
/// `Vec<SubAgentSummary>` are not all `Serialize`, so only the high-level
/// fields are kept (mirrors how `oneai-studio` projects the result).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSummary {
    pub final_answer: String,
    pub iterations: usize,
    pub completed: bool,
    pub active_paradigm: String,
}

impl From<&AgentLoopResult> for TurnSummary {
    fn from(r: &AgentLoopResult) -> Self {
        Self {
            final_answer: r.final_answer.clone(),
            iterations: r.iterations,
            completed: r.completed,
            active_paradigm: paradigm_to_string(r.active_paradigm),
        }
    }
}

/// Convert a paradigm to its canonical short name.
///
/// Mirrors `oneai-studio::state::paradigm_to_string` (`Plan/ReAct/Reflect/
/// Explore → plan/react/reflect/explore`).
pub fn paradigm_to_string(kind: ParadigmKind) -> String {
    match kind {
        ParadigmKind::Plan => "plan".to_string(),
        ParadigmKind::ReAct => "react".to_string(),
        ParadigmKind::Reflect => "reflect".to_string(),
        ParadigmKind::Explore => "explore".to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::GlobalState;

    fn fake_result(paradigm: ParadigmKind, iterations: usize, completed: bool) -> AgentLoopResult {
        AgentLoopResult {
            conversation: oneai_core::Conversation::new(),
            final_answer: "done".to_string(),
            global_state: GlobalState::default(),
            iterations,
            completed,
            active_paradigm: paradigm,
            sub_agent_results: Vec::new(),
        }
    }

    #[test]
    fn turn_summary_from_result() {
        let r = fake_result(ParadigmKind::Plan, 3, true);
        let s = TurnSummary::from(&r);
        assert_eq!(s.final_answer, "done");
        assert_eq!(s.iterations, 3);
        assert!(s.completed);
        assert_eq!(s.active_paradigm, "plan");
    }

    #[test]
    fn paradigm_names() {
        assert_eq!(paradigm_to_string(ParadigmKind::Plan), "plan");
        assert_eq!(paradigm_to_string(ParadigmKind::ReAct), "react");
        assert_eq!(paradigm_to_string(ParadigmKind::Reflect), "reflect");
        assert_eq!(paradigm_to_string(ParadigmKind::Explore), "explore");
    }
}
