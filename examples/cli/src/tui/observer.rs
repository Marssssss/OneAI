//! TUI Observer — receives AgentLoop events and forwards them
//! to the TUI via a channel for real-time rendering.

use oneai_agent::{AgentLoopObserver, AgentLoopResult, ParadigmKind, ToolCallRequest, SubAgentKind};
use oneai_core::ToolOutput;
use oneai_core::ContextAccounting;

use super::app::TokenUsage;

/// A snapshot of the live plan state, sent so the TUI can render the plan panel.
pub type PlanStateSnapshot = oneai_agent::PlanState;

/// Events sent from the observer to the TUI event loop.
///
/// Not `Clone` — `PlanSubmitted` carries a oneshot reply sender that must be
/// moved (not duplicated) into the TUI.
#[derive(Debug)]
pub enum ObserverEvent {
    IterationStart(usize, ParadigmKind),
    DirectAnswer(String),
    ToolCalls(Vec<ToolCallRequest>),
    ToolResult(String, String, ToolOutput),
    Delegate(String, SubAgentKind),
    ParadigmSwitch(ParadigmKind),
    Checkpoint(usize),
    Complete(AgentLoopResult),
    #[allow(dead_code)]
    StreamChunk(String),
    #[allow(dead_code)]
    Error(String),

    // New events for enhanced TUI
    TokenUsageUpdate(TokenUsage),
    ContextAccountingUpdate(ContextAccounting),

    /// Thinking/reasoning content fragment from extended thinking models.
    Thinking(String),

    /// Plan state changed (task created/updated). Carries the current plan
    /// snapshot (None = cleared). The TUI updates the persistent plan panel.
    PlanUpdate(Option<PlanStateSnapshot>),
    /// `/init` finished (background project-info generation). The payload is a
    /// pre-formatted result/error message to display. Always clears `is_thinking`.
    InitResult(String),

    /// `/compact` finished (background LLM summarization). `summary` is empty
    /// when the conversation was too short to compress. Always clears
    /// `is_thinking`.
    CompactResult {
        summary: String,
        removed_count: usize,
        /// Retained recent turns `(role, text)` — user/assistant only — for the
        /// TUI to re-render after clearing its display list.
        retained: Vec<(String, String)>,
    },
}

/// TUI observer — receives AgentLoop events and updates the App state
/// via a channel, so the TUI can render them in real-time.
pub struct TuiObserver {
    tx: tokio::sync::mpsc::UnboundedSender<ObserverEvent>,
}

impl TuiObserver {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<ObserverEvent>) -> Self {
        Self { tx }
    }
}

impl AgentLoopObserver for TuiObserver {
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind) {
        let _ = self.tx.send(ObserverEvent::IterationStart(iteration, paradigm));
    }

    fn on_direct_answer(&self, text: &str) {
        let _ = self.tx.send(ObserverEvent::DirectAnswer(text.to_string()));
    }

    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        let _ = self.tx.send(ObserverEvent::ToolCalls(calls.to_vec()));
    }

    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &ToolOutput) {
        let _ = self.tx.send(ObserverEvent::ToolResult(
            call_id.to_string(),
            tool_name.to_string(),
            output.clone(),
        ));
    }

    fn on_delegate(&self, task: &str, agent_type: &SubAgentKind) {
        let _ = self.tx.send(ObserverEvent::Delegate(task.to_string(), agent_type.clone()));
    }

    fn on_paradigm_switch(&self, paradigm: ParadigmKind) {
        let _ = self.tx.send(ObserverEvent::ParadigmSwitch(paradigm));
    }

    fn on_checkpoint(&self, iteration: usize) {
        let _ = self.tx.send(ObserverEvent::Checkpoint(iteration));
    }

    fn on_complete(&self, result: &AgentLoopResult) {
        let _ = self.tx.send(ObserverEvent::Complete(result.clone()));
    }

    fn on_stream_chunk(&self, text: &str) {
        let _ = self.tx.send(ObserverEvent::StreamChunk(text.to_string()));
    }

    fn on_token_usage(&self, prompt_tokens: u32, completion_tokens: u32) {
        let usage = super::app::TokenUsage {
            prompt: prompt_tokens,
            completion: completion_tokens,
            total: prompt_tokens + completion_tokens,
            is_estimated: false,
        };
        // Accumulate into session total
        let _ = self.tx.send(ObserverEvent::TokenUsageUpdate(usage));
    }

    fn on_context_accounting(&self, accounting: &oneai_core::ContextAccounting) {
        let _ = self.tx.send(ObserverEvent::ContextAccountingUpdate(accounting.clone()));
    }

    fn on_thinking(&self, text: &str) {
        let _ = self.tx.send(ObserverEvent::Thinking(text.to_string()));
    }

    fn on_plan_update(&self, plan: Option<&oneai_agent::PlanState>) {
        let _ = self.tx.send(ObserverEvent::PlanUpdate(plan.cloned()));
    }

    // on_plan_submitted is intentionally not overridden — plan confirmation now
    // flows through the InteractionGate (PlanReview) channel, not the observer.
    // The trait method remains (deprecated) with its empty default.
}
