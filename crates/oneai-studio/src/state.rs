//! StudioState — shared state that connects trace, AgentLoop, StateGraph,
//! and checkpoint data to the Studio frontend via WebSocket broadcast.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

use serde::{Deserialize, Serialize};
use oneai_core::{ToolOutput, InterruptPoint, ResumeSignal, ApprovalRequest, ApprovalResponse};
use oneai_agent::{AgentLoopObserver, ParadigmKind, ToolCallRequest, AgentLoopResult, SubAgentKind};
use oneai_trace::{TraceContext, InMemoryCollector};
use oneai_persistence::FilePersistence;
use oneai_tool::ToolRegistry;

// ─── StudioEvent ─────────────────────────────────────────────────────

/// An event broadcast to all WebSocket subscribers.
///
/// Each event corresponds to an AgentLoopObserver callback, serialized
/// as JSON for the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StudioEvent {
    /// Agent loop iteration started.
    IterationStart { iteration: usize, paradigm: String },

    /// Model produced a direct answer (loop will end).
    DirectAnswer { text: String },

    /// Model decided to call tools.
    ToolCalls { calls: Vec<ToolCallView> },

    /// Tool call completed with result.
    ToolResult {
        call_id: String,
        tool_name: String,
        success: bool,
        output_summary: String,
    },

    /// Model delegated to a sub-agent.
    Delegate { task: String, agent_type: String },

    /// Model switched paradigm.
    ParadigmSwitch { paradigm: String },

    /// Checkpoint saved.
    CheckpointSaved { iteration: usize, checkpoint_id: String },

    /// A trace event occurred (Thought, Action, Observation, etc.).
    TraceEvent { kind: String, name: String, attributes: serde_json::Value },

    /// Thinking/reasoning content (extended thinking).
    Thinking { text: String },

    /// Streaming text chunk (typewriter effect).
    StreamChunk { text: String },

    /// Approval request pending (high-risk tool).
    ApprovalRequest { tool_name: String, args: serde_json::Value, risk_level: String },

    /// Approval response received.
    ApprovalResponse { approved: bool, reason: String },

    /// Agent loop completed.
    LoopComplete { result_summary: String },

    /// An error occurred.
    Error { message: String },
}

// ─── ToolCallView ────────────────────────────────────────────────────

/// Frontend-friendly tool call representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallView {
    pub id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
}

// ─── SessionView ─────────────────────────────────────────────────────

/// A tracked session in the Studio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    /// Session ID.
    pub id: String,
    /// Active paradigm.
    pub paradigm: String,
    /// Current iteration number.
    pub iteration: usize,
    /// Whether the session is running.
    pub running: bool,
    /// Total tokens used.
    pub total_tokens: u64,
}

// ─── SessionUpdate ───────────────────────────────────────────────────

/// Partial update to a session's state.
#[derive(Debug, Clone)]
pub enum SessionUpdate {
    Paradigm(String),
    Iteration(usize),
    Running(bool),
    Tokens(u64),
}

// ─── StudioState ─────────────────────────────────────────────────────

/// Shared state for the Studio server — connects all data sources
/// and broadcasts events to WebSocket subscribers.
///
/// StudioState implements `AgentLoopObserver` — when the AgentLoop
/// runs, Observer callbacks push events to all WebSocket subscribers
/// via a broadcast channel.
pub struct StudioState {
    /// Trace context for collecting execution data.
    trace_context: TraceContext,

    /// Persistence for checkpoint time-travel.
    persistence: Arc<FilePersistence>,

    /// Tool registry for listing available tools.
    tool_registry: Arc<ToolRegistry>,

    /// Active sessions being tracked.
    sessions: RwLock<HashMap<String, SessionView>>,

    /// Broadcast channel for pushing events to WebSocket subscribers.
    /// Capacity: 1024 events (subscriber lag > 1024 = dropped).
    event_bus: broadcast::Sender<StudioEvent>,
}

impl StudioState {
    /// Create a new StudioState with the given components.
    pub fn new(
        trace_context: TraceContext,
        persistence: Arc<FilePersistence>,
        tool_registry: Arc<ToolRegistry>,
    ) -> Self {
        let (event_bus, _) = broadcast::channel(1024);

        Self {
            trace_context,
            persistence,
            tool_registry,
            sessions: RwLock::new(HashMap::new()),
            event_bus,
        }
    }

    /// Create a StudioState with default components for standalone Studio.
    pub fn new_default() -> Self {
        let trace_context = TraceContext::new(Arc::new(InMemoryCollector::new()));
        let persistence = Arc::new(FilePersistence::new("/tmp/oneai-studio-checkpoints"));
        let tool_registry = Arc::new(ToolRegistry::new());

        Self::new(trace_context, persistence, tool_registry)
    }

    /// Subscribe to the event bus — returns a broadcast Receiver.
    ///
    /// Each WebSocket connection subscribes via this method.
    pub fn subscribe(&self) -> broadcast::Receiver<StudioEvent> {
        self.event_bus.subscribe()
    }

    /// Broadcast an event to all WebSocket subscribers.
    pub fn broadcast(&self, event: StudioEvent) {
        // send() returns Err when there are no subscribers — that's OK
        let _ = self.event_bus.send(event);
    }

    /// Get the trace context.
    pub fn trace_context(&self) -> &TraceContext {
        &self.trace_context
    }

    /// Get the persistence layer.
    pub fn persistence(&self) -> &Arc<FilePersistence> {
        &self.persistence
    }

    /// Get the tool registry.
    pub fn tool_registry(&self) -> &Arc<ToolRegistry> {
        &self.tool_registry
    }

    /// Register a new session in the Studio.
    pub async fn register_session(&self, session: SessionView) {
        self.sessions.write().await.insert(session.id.clone(), session);
    }

    /// Update a session's state.
    pub async fn update_session(&self, id: &str, update: SessionUpdate) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(id) {
            match update {
                SessionUpdate::Paradigm(p) => session.paradigm = p,
                SessionUpdate::Iteration(i) => session.iteration = i,
                SessionUpdate::Running(r) => session.running = r,
                SessionUpdate::Tokens(t) => session.total_tokens = t,
            }
        }
    }

    /// List all tracked sessions.
    pub async fn list_sessions(&self) -> Vec<SessionView> {
        self.sessions.read().await.values().cloned().collect()
    }

    /// Get a specific session.
    pub async fn get_session(&self, id: &str) -> Option<SessionView> {
        self.sessions.read().await.get(id).cloned()
    }
}

// ─── AgentLoopObserver Implementation ─────────────────────────────────

impl AgentLoopObserver for StudioState {
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind) {
        self.broadcast(StudioEvent::IterationStart {
            iteration,
            paradigm: paradigm_to_string(paradigm),
        });
    }

    fn on_direct_answer(&self, text: &str) {
        self.broadcast(StudioEvent::DirectAnswer { text: text.to_string() });
    }

    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        self.broadcast(StudioEvent::ToolCalls {
            calls: calls.iter().map(|c| ToolCallView {
                id: c.id.clone(),
                tool_name: c.name.clone(),
                args: c.args.clone(),
            }).collect(),
        });
    }

    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &ToolOutput) {
        self.broadcast(StudioEvent::ToolResult {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            success: output.success,
            output_summary: if output.success {
                truncate(&output.content, 200)
            } else {
                output.error.clone().unwrap_or_default()
            },
        });
    }

    fn on_delegate(&self, task: &str, agent_type: &SubAgentKind) {
        self.broadcast(StudioEvent::Delegate {
            task: task.to_string(),
            agent_type: agent_type.name().to_string(),
        });
    }

    fn on_paradigm_switch(&self, paradigm: ParadigmKind) {
        self.broadcast(StudioEvent::ParadigmSwitch {
            paradigm: paradigm_to_string(paradigm),
        });
    }

    fn on_checkpoint(&self, iteration: usize) {
        self.broadcast(StudioEvent::CheckpointSaved {
            iteration,
            checkpoint_id: format!("checkpoint_iter_{}", iteration),
        });
    }

    fn on_complete(&self, result: &AgentLoopResult) {
        self.broadcast(StudioEvent::LoopComplete {
            result_summary: format!(
                "Completed: {} iterations, paradigm {}",
                result.iterations,
                paradigm_to_string(result.active_paradigm)
            ),
        });
    }

    fn on_stream_chunk(&self, text: &str) {
        self.broadcast(StudioEvent::StreamChunk { text: text.to_string() });
    }

    fn on_thinking(&self, text: &str) {
        self.broadcast(StudioEvent::Thinking { text: text.to_string() });
    }

    fn on_approval_request(&self, request: &ApprovalRequest) {
        self.broadcast(StudioEvent::ApprovalRequest {
            tool_name: request.tool_name.clone(),
            args: request.args.clone(),
            risk_level: format!("{:?}", request.risk_level),
        });
    }

    fn on_approval_response(&self, response: &ApprovalResponse) {
        let (approved, reason): (bool, String) = match response {
            ApprovalResponse::Approved { .. } => (true, "Approved".to_string()),
            ApprovalResponse::Denied { reason } => (false, reason.clone()),
            ApprovalResponse::Modified { .. } => (true, "Modified".to_string()),
            ApprovalResponse::Observe { observation } => (false, observation.clone()),
        };
        self.broadcast(StudioEvent::ApprovalResponse {
            approved,
            reason,
        });
    }

    fn on_token_usage(&self, prompt_tokens: u32, completion_tokens: u32) {
        self.broadcast(StudioEvent::TraceEvent {
            kind: "TokenUsage".to_string(),
            name: "llm.token_usage".to_string(),
            attributes: serde_json::json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            }),
        });
    }

    fn on_interrupt(&self, point: &InterruptPoint) {
        self.broadcast(StudioEvent::TraceEvent {
            kind: "Interrupt".to_string(),
            name: "agent.interrupt".to_string(),
            attributes: serde_json::json!({
                "id": point.id,
                "iteration": point.iteration,
                "reason": format!("{:?}", point.reason),
            }),
        });
    }

    fn on_resume(&self, signal: &ResumeSignal) {
        self.broadcast(StudioEvent::TraceEvent {
            kind: "Resume".to_string(),
            name: "agent.resume".to_string(),
            attributes: serde_json::json!({
                "interrupt_id": signal.interrupt_id,
                "feedback": signal.feedback,
                "action": format!("{:?}", signal.action),
            }),
        });
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────

/// Convert ParadigmKind to string.
fn paradigm_to_string(kind: ParadigmKind) -> String {
    match kind {
        ParadigmKind::Plan => "plan".to_string(),
        ParadigmKind::ReAct => "react".to_string(),
        ParadigmKind::Reflect => "reflect".to_string(),
        ParadigmKind::Explore => "explore".to_string(),
        _ => "unknown".to_string(),
    }
}

/// Truncate a string to a maximum length.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_studio_state_creation() {
        let state = StudioState::new_default();
        let sessions = state.list_sessions();
        // Need to await — use block_on
        let rt = tokio::runtime::Runtime::new().unwrap();
        let sessions = rt.block_on(sessions);
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_broadcast_iteration_start() {
        let state = StudioState::new_default();
        let mut rx = state.subscribe();

        state.broadcast(StudioEvent::IterationStart {
            iteration: 1,
            paradigm: "react".to_string(),
        });

        let event = rx.try_recv().unwrap();
        match event {
            StudioEvent::IterationStart { iteration, paradigm } => {
                assert_eq!(iteration, 1);
                assert_eq!(paradigm, "react");
            }
            _ => panic!("Expected IterationStart event"),
        }
    }

    #[test]
    fn test_broadcast_direct_answer() {
        let state = StudioState::new_default();
        let mut rx = state.subscribe();

        state.broadcast(StudioEvent::DirectAnswer {
            text: "The answer is 42".to_string(),
        });

        let event = rx.try_recv().unwrap();
        match event {
            StudioEvent::DirectAnswer { text } => {
                assert_eq!(text, "The answer is 42");
            }
            _ => panic!("Expected DirectAnswer event"),
        }
    }

    #[test]
    fn test_multiple_subscribers() {
        let state = StudioState::new_default();
        let mut rx1 = state.subscribe();
        let mut rx2 = state.subscribe();

        state.broadcast(StudioEvent::ParadigmSwitch { paradigm: "plan".to_string() });

        let event1 = rx1.try_recv().unwrap();
        let event2 = rx2.try_recv().unwrap();
        assert!(matches!(event1, StudioEvent::ParadigmSwitch { .. }));
        assert!(matches!(event2, StudioEvent::ParadigmSwitch { .. }));
    }

    #[test]
    fn test_session_registration() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let state = StudioState::new_default();

        rt.block_on(state.register_session(SessionView {
            id: "sess_1".to_string(),
            paradigm: "react".to_string(),
            iteration: 0,
            running: true,
            total_tokens: 0,
        }));

        let sessions = rt.block_on(state.list_sessions());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "sess_1");
    }

    #[test]
    fn test_session_update() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let state = StudioState::new_default();

        rt.block_on(state.register_session(SessionView {
            id: "sess_1".to_string(),
            paradigm: "react".to_string(),
            iteration: 0,
            running: true,
            total_tokens: 0,
        }));

        rt.block_on(state.update_session("sess_1", SessionUpdate::Iteration(5)));
        rt.block_on(state.update_session("sess_1", SessionUpdate::Tokens(1200)));

        let session = rt.block_on(state.get_session("sess_1")).unwrap();
        assert_eq!(session.iteration, 5);
        assert_eq!(session.total_tokens, 1200);
    }

    #[test]
    fn test_studio_event_json_serialization() {
        let event = StudioEvent::ToolResult {
            call_id: "call_1".to_string(),
            tool_name: "shell".to_string(),
            success: true,
            output_summary: "OK".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"ToolResult\""));
        assert!(json.contains("\"call_1\""));

        // Deserialize back
        let deserialized: StudioEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, StudioEvent::ToolResult { .. }));
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a very long string that exceeds limit", 10), "a very lon...");
    }

    #[test]
    fn test_paradigm_to_string() {
        assert_eq!(paradigm_to_string(ParadigmKind::Plan), "plan");
        assert_eq!(paradigm_to_string(ParadigmKind::ReAct), "react");
        assert_eq!(paradigm_to_string(ParadigmKind::Reflect), "reflect");
        assert_eq!(paradigm_to_string(ParadigmKind::Explore), "explore");
    }
}
