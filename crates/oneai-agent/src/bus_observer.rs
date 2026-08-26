//! `BusObserver` — an [`AgentLoopObserver`] that bridges the engine's
//! callback-based emission surface to the bus's stream-based [`EngineYield`].
//!
//! Each observer callback is translated 1:1 into an [`EngineYield`] variant and
//! emitted to the bus (synchronously — `broadcast::send` is sync, so this works
//! from the sync observer methods the `AgentLoop` calls). This is the reusable
//! form of the conversion `StudioState` does inline today (`oneai-studio`'s
//! `impl AgentLoopObserver for StudioState`); P5 retargets StudioState to it.

use std::sync::{Arc, Mutex};

use oneai_bus::{
    BusDelegateProgress, BusParadigmKind, BusSubAgent, BusSubAgentKind, BusToolCall,
    BusTurnSummary, BusUsageRecord, EngineBus, EngineYield,
};

use crate::agent_loop::{AgentLoopResult, DelegateProgressEvent, ParadigmKind, ToolCallRequest};
use crate::sub_agent::{SubAgentKind, SubAgentSummary};
use crate::AgentLoopObserver;

// ─── agent → bus DTO conversions ────────────────────────────────────────────

impl From<ParadigmKind> for BusParadigmKind {
    fn from(k: ParadigmKind) -> Self {
        match k {
            ParadigmKind::Plan => Self::Plan,
            ParadigmKind::ReAct => Self::ReAct,
            ParadigmKind::Reflect => Self::Reflect,
            ParadigmKind::Explore => Self::Explore,
            // ParadigmKind is not #[non_exhaustive]; a future variant added in
            // this crate will fail to compile here (desired — prompt a bus
            // mapping decision rather than silently defaulting).
        }
    }
}

impl From<&SubAgentKind> for BusSubAgentKind {
    fn from(k: &SubAgentKind) -> Self {
        match k {
            SubAgentKind::Plan => Self::Plan,
            SubAgentKind::Explore => Self::Explore,
            SubAgentKind::Code => Self::Code,
            SubAgentKind::Review => Self::Review,
            SubAgentKind::Reflect => Self::Reflect,
            SubAgentKind::Custom(name) => Self::Custom(name.clone()),
        }
    }
}

impl From<&ToolCallRequest> for BusToolCall {
    fn from(c: &ToolCallRequest) -> Self {
        Self {
            id: c.id.clone(),
            name: c.name.clone(),
            args: c.args.clone(),
        }
    }
}

impl From<&SubAgentSummary> for BusSubAgent {
    fn from(s: &SubAgentSummary) -> Self {
        Self {
            completed: s.completed,
            summary: s.summary.clone(),
            key_findings: s.key_findings.clone(),
            budget_exceeded: s.budget_exceeded,
            agent_kind: BusSubAgentKind::from(&s.agent_kind),
            tokens_used: s.tokens_used,
        }
    }
}

impl From<&DelegateProgressEvent> for BusDelegateProgress {
    fn from(e: &DelegateProgressEvent) -> Self {
        match e {
            DelegateProgressEvent::IterationStart {
                iteration,
                paradigm,
            } => Self::IterationStart {
                iteration: *iteration,
                paradigm: BusParadigmKind::from(*paradigm),
            },
            DelegateProgressEvent::ToolResult {
                tool_name,
                snapshot,
            } => Self::ToolResult {
                tool_name: tool_name.clone(),
                snapshot: snapshot.clone(),
            },
            DelegateProgressEvent::TokenUsage { prompt, completion } => Self::TokenUsage {
                prompt: *prompt,
                completion: *completion,
            },
            DelegateProgressEvent::Cancelled => Self::Cancelled,
        }
    }
}

impl From<&AgentLoopResult> for BusTurnSummary {
    fn from(r: &AgentLoopResult) -> Self {
        Self {
            final_answer: r.final_answer.clone(),
            iterations: r.iterations,
            completed: r.completed,
            active_paradigm: BusParadigmKind::from(r.active_paradigm),
        }
    }
}

// ─── BusObserver ─────────────────────────────────────────────────────────────

/// Bridges `AgentLoopObserver` callbacks to the [`EngineBus`] yield stream.
///
/// Hold one `BusObserver` per turn (its `turn_id` is fixed for its lifetime);
/// the `AgentLoop` driver constructs it, calls `run_with_observer`, then drops
/// it. Frontends subscribe to the bus's yields independently of this handle.
pub struct BusObserver {
    bus: Arc<dyn EngineBus>,
    turn_id: String,
    /// Last paradigm the loop reported (via `on_iteration_start` or a prior
    /// switch) — the `from` side of a `ParadigmSwitch` yield. None until the
    /// first iteration reports.
    current_paradigm: Mutex<Option<BusParadigmKind>>,
}

impl BusObserver {
    /// Construct an observer that emits yields tagged with `turn_id` to `bus`.
    pub fn new(bus: Arc<dyn EngineBus>, turn_id: impl Into<String>) -> Self {
        Self {
            bus,
            turn_id: turn_id.into(),
            current_paradigm: Mutex::new(None),
        }
    }

    fn emit(&self, y: EngineYield) {
        // No-op on error: zero subscribers is legitimate (a turn may run before
        // any frontend subscribes). Other errors are unactionable from a sync
        // observer callback anyway — trace rather than panic.
        let _ = self.bus.emit(y);
    }
}

impl AgentLoopObserver for BusObserver {
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind) {
        let paradigm = BusParadigmKind::from(paradigm);
        *self
            .current_paradigm
            .lock()
            .expect("current_paradigm poisoned") = Some(paradigm);
        self.emit(EngineYield::IterationStart {
            turn_id: self.turn_id.clone(),
            iteration,
            paradigm,
        });
    }

    fn on_direct_answer(&self, text: &str) {
        self.emit(EngineYield::DirectAnswer {
            turn_id: self.turn_id.clone(),
            text: text.to_string(),
            speaker: None,
        });
    }

    fn on_tool_calls(&self, calls: &[ToolCallRequest]) {
        self.emit(EngineYield::ToolCalls {
            turn_id: self.turn_id.clone(),
            calls: calls.iter().map(BusToolCall::from).collect(),
            speaker: None,
        });
    }

    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &oneai_core::ToolOutput) {
        self.emit(EngineYield::ToolResult {
            turn_id: self.turn_id.clone(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            output: output.clone(),
            speaker: None,
        });
    }

    fn on_delegate(&self, id: &str, task: &str, agent_type: &SubAgentKind) {
        self.emit(EngineYield::Delegate {
            turn_id: self.turn_id.clone(),
            task_id: id.to_string(),
            task: task.to_string(),
            agent_kind: BusSubAgentKind::from(agent_type),
            speaker: None,
            depends_on: Vec::new(),
        });
    }

    fn on_delegate_full(&self, task: &crate::agent_loop::DelegateTask) {
        // Richer path used by the loop's spawn sites — carries `depends_on`
        // so a trajectory timeline can draw the delegation DAG.
        self.emit(EngineYield::Delegate {
            turn_id: self.turn_id.clone(),
            task_id: task.id.clone(),
            task: task.task.clone(),
            agent_kind: BusSubAgentKind::from(&task.agent_type),
            speaker: None,
            depends_on: task.depends_on.clone(),
        });
    }

    fn on_delegate_complete(&self, id: &str, summary: &SubAgentSummary) {
        self.emit(EngineYield::DelegateComplete {
            turn_id: self.turn_id.clone(),
            task_id: id.to_string(),
            summary: BusSubAgent::from(summary),
            speaker: None,
        });
    }

    fn on_delegate_progress(
        &self,
        delegate_id: &str,
        kind: &SubAgentKind,
        event: &DelegateProgressEvent,
    ) {
        self.emit(EngineYield::DelegateProgress {
            turn_id: self.turn_id.clone(),
            task_id: delegate_id.to_string(),
            agent_kind: BusSubAgentKind::from(kind),
            event: BusDelegateProgress::from(event),
        });
    }

    fn on_paradigm_switch(&self, paradigm: ParadigmKind) {
        let to = BusParadigmKind::from(paradigm);
        let from = self
            .current_paradigm
            .lock()
            .expect("current_paradigm poisoned")
            .replace(to);
        // `from` is None only if no iteration reported before the switch —
        // surface the target as both sides so a frontend can still render it.
        let from = from.unwrap_or(to);
        self.emit(EngineYield::ParadigmSwitch {
            turn_id: self.turn_id.clone(),
            from,
            to,
        });
    }

    fn on_checkpoint(&self, iteration: usize) {
        // Working-state persistence is the checkpoint successor (see
        // working-state-mechanism); a separate EngineYield variant isn't on the
        // P0 schema. Surface as WorkingState only when a payload exists; here
        // we no-op (the loop's WorkingState events flow through the projector).
        let _ = iteration;
    }

    fn on_complete(&self, result: &AgentLoopResult) {
        self.emit(EngineYield::TurnComplete {
            turn_id: self.turn_id.clone(),
            summary: BusTurnSummary::from(result),
        });
    }

    fn on_stream_chunk(&self, text: &str) {
        self.emit(EngineYield::StreamChunk {
            turn_id: self.turn_id.clone(),
            text: text.to_string(),
            speaker: None,
        });
    }

    fn on_thinking(&self, text: &str) {
        self.emit(EngineYield::Thinking {
            turn_id: self.turn_id.clone(),
            text: text.to_string(),
            speaker: None,
        });
    }

    fn on_token_usage_full(
        &self,
        prompt_tokens: u32,
        completion_tokens: u32,
        cache_read_tokens: u32,
        cache_creation_tokens: u32,
    ) {
        self.emit(EngineYield::TokenUsage {
            usage: BusUsageRecord {
                prompt_tokens,
                completion_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            },
        });
    }

    fn on_context_accounting(&self, accounting: &oneai_core::ContextAccounting) {
        self.emit(EngineYield::ContextAccounting {
            turn_id: self.turn_id.clone(),
            accounting: accounting.clone(),
        });
    }

    fn on_context_assembled(&self, snapshot: &oneai_core::ContextSnapshot, duration_ms: u64) {
        self.emit(EngineYield::ContextAssembled {
            turn_id: self.turn_id.clone(),
            iteration: snapshot.iteration,
            sections: snapshot.sections.clone(),
            duration_ms,
        });
    }

    fn on_inference(&self, snapshot: &oneai_core::InferenceSnapshot) {
        self.emit(EngineYield::Inference {
            turn_id: self.turn_id.clone(),
            snapshot: snapshot.clone(),
        });
    }

    fn on_working_state(&self, event: &oneai_core::TaskEventPayload) {
        self.emit(EngineYield::WorkingState {
            event: event.clone(),
        });
    }

    fn on_interrupt(&self, point: &oneai_core::InterruptPoint) {
        use oneai_core::InterruptReason;
        let (reason, label) = match &point.reason {
            InterruptReason::HumanApprovalNeeded { tool_name, .. } => (
                format!("approval needed for tool `{tool_name}`"),
                "human_approval",
            ),
            InterruptReason::HumanFeedbackRequested { question } => {
                (format!("feedback requested: {question}"), "human_feedback")
            }
            InterruptReason::ParadigmBoundary { from, to } => (
                format!("paradigm boundary {from} → {to}"),
                "paradigm_boundary",
            ),
            InterruptReason::Custom { reason } => (reason.clone(), "custom"),
        };
        self.emit(EngineYield::Interrupted {
            turn_id: self.turn_id.clone(),
            reason,
            point: label.to_string(),
        });
    }

    fn on_reflection(&self, summary: &str) {
        self.emit(EngineYield::Reflection {
            turn_id: self.turn_id.clone(),
            summary: summary.to_string(),
        });
    }

    fn on_plan_update(&self, plan: Option<&crate::plan_state::PlanState>) {
        // PlanState lives in this crate; the bus crate can't name it, so carry
        // the serialized form. None → cleared (no payload).
        let plan = plan.map(|p| serde_json::to_value(p).unwrap_or(serde_json::Value::Null));
        self.emit(EngineYield::PlanUpdate {
            turn_id: self.turn_id.clone(),
            plan,
        });
    }

    fn on_tools_added(&self, names: &[String]) {
        self.emit(EngineYield::ToolsAdded {
            turn_id: self.turn_id.clone(),
            names: names.to_vec(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_bus::InProcessBus;
    use oneai_core::{ContentBlock, InteractionRequest, InteractionResponse, ToolOutput};

    fn observed_bus() -> (Arc<InProcessBus>, Arc<dyn EngineBus>) {
        let (bus, _rx) = InProcessBus::new();
        let arc = Arc::new(bus);
        (arc.clone(), arc as Arc<dyn EngineBus>)
    }

    #[tokio::test]
    async fn iteration_start_emits_to_subscriber() {
        let (bus, engine_bus) = observed_bus();
        let mut sub = bus.subscribe_yields();
        let obs = BusObserver::new(engine_bus, "t_1");
        obs.on_iteration_start(3, ParadigmKind::Plan);
        match sub.recv().await.unwrap() {
            EngineYield::IterationStart {
                turn_id,
                iteration,
                paradigm,
            } => {
                assert_eq!(turn_id, "t_1");
                assert_eq!(iteration, 3);
                assert_eq!(paradigm, BusParadigmKind::Plan);
            }
            other => panic!("expected IterationStart, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_result_carries_full_output() {
        let (bus, engine_bus) = observed_bus();
        let mut sub = bus.subscribe_yields();
        let obs = BusObserver::new(engine_bus, "t_1");
        let output = ToolOutput {
            success: true,
            content: "done".to_string(),
            ..Default::default()
        };
        obs.on_tool_result("c_1", "shell", &output);
        match sub.recv().await.unwrap() {
            EngineYield::ToolResult {
                call_id,
                tool_name,
                output,
                ..
            } => {
                assert_eq!(call_id, "c_1");
                assert_eq!(tool_name, "shell");
                assert!(output.success);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_emits_turn_summary() {
        let (bus, engine_bus) = observed_bus();
        let mut sub = bus.subscribe_yields();
        let obs = BusObserver::new(engine_bus, "t_7");
        // Minimal AgentLoopResult for the projection — full conversation/global
        // state aren't needed by BusTurnSummary.
        let result = AgentLoopResult {
            conversation: oneai_core::Conversation::new(),
            final_answer: "42".to_string(),
            global_state: oneai_core::GlobalState::default(),
            iterations: 5,
            completed: true,
            active_paradigm: ParadigmKind::ReAct,
            sub_agent_results: Vec::new(),
        };
        obs.on_complete(&result);
        match sub.recv().await.unwrap() {
            EngineYield::TurnComplete { turn_id, summary } => {
                assert_eq!(turn_id, "t_7");
                assert_eq!(summary.final_answer, "42");
                assert!(summary.completed);
                assert_eq!(summary.active_paradigm, BusParadigmKind::ReAct);
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
    }

    // ─── Issue #40 trajectory events ────────────────────────────────────────

    #[tokio::test]
    async fn delegate_full_carries_depends_on() {
        let (bus, engine_bus) = observed_bus();
        let mut sub = bus.subscribe_yields();
        let obs = BusObserver::new(engine_bus, "t_1");
        let task = crate::agent_loop::DelegateTask {
            id: "d2".to_string(),
            task: "step two".to_string(),
            agent_type: crate::sub_agent::SubAgentKind::Code,
            budget: oneai_core::budget::TokenBudget::new(1000),
            depends_on: vec!["d1".to_string()],
            call_id: "call_d2".to_string(),
            custom_role: None,
            system_prompt_override: None,
            tools_override: None,
            inherit_context: false,
            inherit_last_n: 0,
            seed_messages: None,
        };
        obs.on_delegate_full(&task);
        match sub.recv().await.unwrap() {
            EngineYield::Delegate {
                task_id,
                depends_on,
                ..
            } => {
                assert_eq!(task_id, "d2");
                assert_eq!(depends_on, vec!["d1".to_string()]);
            }
            other => panic!("expected Delegate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_assembled_emits_sections() {
        let (bus, engine_bus) = observed_bus();
        let mut sub = bus.subscribe_yields();
        let obs = BusObserver::new(engine_bus, "t_1");
        let snapshot = oneai_core::ContextSnapshot {
            iteration: 1,
            sections: vec![oneai_core::ContextSection {
                key: oneai_core::ContextKey::BasePrompt,
                label: "system prompt".to_string(),
                tokens: 10,
                content_hash: 1,
                content: Some("you are OneAI".to_string()),
            }],
        };
        obs.on_context_assembled(&snapshot, 33);
        match sub.recv().await.unwrap() {
            EngineYield::ContextAssembled {
                turn_id,
                iteration,
                sections,
                duration_ms,
            } => {
                assert_eq!(turn_id, "t_1");
                assert_eq!(iteration, 1);
                assert_eq!(sections.len(), 1);
                assert_eq!(duration_ms, 33);
                assert_eq!(sections[0].key, oneai_core::ContextKey::BasePrompt);
            }
            other => panic!("expected ContextAssembled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inference_emits_snapshot() {
        let (bus, engine_bus) = observed_bus();
        let mut sub = bus.subscribe_yields();
        let obs = BusObserver::new(engine_bus, "t_1");
        let snapshot = oneai_core::InferenceSnapshot {
            iteration: 2,
            model: "gpt-4o".to_string(),
            temperature: Some(0.3),
            max_tokens: None,
            top_p: None,
            thinking_budget: None,
            tool_names: vec!["shell".to_string()],
            message_count: 1,
            request_messages: vec![oneai_core::Message::user("hi")],
            response: oneai_core::InferenceResponse {
                message: oneai_core::Message::assistant("hello"),
                usage: oneai_core::TokenUsage::new(10, 4),
                model: "gpt-4o".to_string(),
                metadata: Default::default(),
            },
            duration_ms: 555,
        };
        obs.on_inference(&snapshot);
        match sub.recv().await.unwrap() {
            EngineYield::Inference { turn_id, snapshot } => {
                assert_eq!(turn_id, "t_1");
                assert_eq!(snapshot.iteration, 2);
                assert_eq!(snapshot.duration_ms, 555);
            }
            other => panic!("expected Inference, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn working_state_snapshot_emits() {
        let (bus, engine_bus) = observed_bus();
        let mut sub = bus.subscribe_yields();
        let obs = BusObserver::new(engine_bus, "t_1");
        let ws = oneai_core::WorkingState {
            task_id: "task-1".to_string(),
            goal: "ship it".to_string(),
            ..Default::default()
        };
        obs.on_working_state(&oneai_core::TaskEventPayload::Snapshot { state: ws });
        match sub.recv().await.unwrap() {
            EngineYield::WorkingState { event } => match event {
                oneai_core::TaskEventPayload::Snapshot { state } => {
                    assert_eq!(state.goal, "ship it");
                }
                other => panic!("expected Snapshot payload, got {other:?}"),
            },
            other => panic!("expected WorkingState, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn interrupt_and_reflection_emit() {
        let (bus, engine_bus) = observed_bus();
        let mut sub = bus.subscribe_yields();
        let obs = BusObserver::new(engine_bus, "t_1");
        obs.on_interrupt(&oneai_core::InterruptPoint {
            id: "i1".to_string(),
            iteration: 2,
            reason: oneai_core::InterruptReason::Custom {
                reason: "rate limit".to_string(),
            },
            checkpoint_id: None,
        });
        match sub.recv().await.unwrap() {
            EngineYield::Interrupted { reason, point, .. } => {
                assert_eq!(reason, "rate limit");
                assert_eq!(point, "custom");
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }

        obs.on_reflection("reconsider the approach");
        match sub.recv().await.unwrap() {
            EngineYield::Reflection { summary, .. } => {
                assert_eq!(summary, "reconsider the approach");
            }
            other => panic!("expected Reflection, got {other:?}"),
        }
    }

    // Sanity: the approval round-trip via a bus-backed gate (BusInteractionGate)
    // is exercised in its own module; here we only assert the observer surface.
    #[allow(dead_code)]
    fn _ensure_imports_used() {
        let _ = ContentBlock::Text {
            text: String::new(),
        };
        let _ = InteractionRequest::NetworkApproval {
            host: String::new(),
            requested_by: String::new(),
        };
        let _ = InteractionResponse::Proceed;
    }
}
