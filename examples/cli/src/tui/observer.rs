//! TUI Observer — receives AgentLoop events and forwards them
//! to the TUI via a channel for real-time rendering.

use oneai_agent::{AgentLoopObserver, AgentLoopResult, ParadigmKind, ToolCallRequest, SubAgentKind};
use oneai_core::ToolOutput;
use oneai_core::{ApprovalRequest, ApprovalResponse};

use super::app::TokenUsage;

/// Events sent from the observer to the TUI event loop.
#[derive(Debug, Clone)]
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
    ApprovalRequest(ApprovalRequest),
    ApprovalResponse(ApprovalResponse),
    TokenUsageUpdate(TokenUsage),
    CostUpdate(f64),

    /// Thinking/reasoning content fragment from extended thinking models.
    Thinking(String),
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

    fn on_approval_request(&self, request: &oneai_core::ApprovalRequest) {
        let _ = self.tx.send(ObserverEvent::ApprovalRequest(request.clone()));
    }

    fn on_approval_response(&self, response: &oneai_core::ApprovalResponse) {
        let _ = self.tx.send(ObserverEvent::ApprovalResponse(response.clone()));
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

    fn on_cost_update(&self, cost: f64) {
        let _ = self.tx.send(ObserverEvent::CostUpdate(cost));
    }

    fn on_thinking(&self, text: &str) {
        let _ = self.tx.send(ObserverEvent::Thinking(text.to_string()));
    }
}
