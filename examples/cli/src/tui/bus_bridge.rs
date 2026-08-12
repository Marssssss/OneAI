//! Bus bridge — adapts the `EngineBus` (`Directive`/`EngineYield`) to the
//! TUI's existing consumer surfaces (`ObserverEvent` + `InteractionPendingItem`
//! + the directive pump driving `run_turn_via_bus`).
//!
//! This is the in-process Shape A validation (P2): the TUI no longer directly
//! calls `AppSession::run_agent` with a `TuiObserver`. Instead a directive pump
//! turns `Directive::UserMessage` into `run_turn_via_bus`, and a yield bridge
//! translates each `EngineYield` back into the `ObserverEvent` /
//! `InteractionPendingItem` the TUI already knows how to render. The render
//! path, approval card, plan-review UI, and `RenderScheduler` debounce are
//! untouched — the bus is a drop-in replacement for the direct drive.
//!
//! Approvals round-trip on the bus: `EngineYield::ApprovalRequest` → bridge
//! applies the threshold pre-filter (mirroring the old `ThresholdInteractionGate`
//! auto-proceed for at/below-threshold tools) → forwards the rest as
//! `InteractionPendingItem` into the TUI's existing approval queue; the card's
//! `response_tx` reply is forwarded back as `Directive::Approve` by a per-request
//! forwarder task. Interrupts: Esc submits `Directive::Interrupt`; the engine
//! side registered the turn's `CancellationToken` in `run_agent` (P2 wiring),
//! so the bus fires it directly.

use std::sync::Arc;

use oneai_agent::{AgentLoopResult, ParadigmKind, SubAgentKind, SubAgentSummary, ToolCallRequest};
use oneai_bus::{
    BusParadigmKind, BusSubAgent, BusSubAgentKind, BusToolCall, BusTurnSummary, Directive,
    EngineYield, InProcessBus,
};
use oneai_core::{InteractionRequest, InteractionResponse, PermissionLevel, RiskLevel};
use oneai_tool::InteractionPendingItem;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::app::TokenUsage;
use super::observer::ObserverEvent;
use super::session::SessionState;

// ─── bus → agent DTO conversions (reverse of `BusObserver`'s From impls) ─────
//
// Free functions rather than `impl From` — the orphan rule forbids a foreign
// trait (`From`) impl between two foreign types (`BusParadigmKind` /
// `ParadigmKind`). These mirror `BusObserver`'s canonical forward conversions.

fn paradigm_from_bus(k: BusParadigmKind) -> ParadigmKind {
    match k {
        BusParadigmKind::Plan => ParadigmKind::Plan,
        BusParadigmKind::ReAct => ParadigmKind::ReAct,
        BusParadigmKind::Reflect => ParadigmKind::Reflect,
        BusParadigmKind::Explore => ParadigmKind::Explore,
        // BusParadigmKind is #[non_exhaustive]; an unknown variant from a
        // newer bus crate can't be mapped losslessly — default to ReAct
        // (the loop's own default paradigm) so the TUI still renders.
        _ => ParadigmKind::ReAct,
    }
}

fn sub_agent_kind_from_bus(k: BusSubAgentKind) -> SubAgentKind {
    match k {
        BusSubAgentKind::Plan => SubAgentKind::Plan,
        BusSubAgentKind::Explore => SubAgentKind::Explore,
        BusSubAgentKind::Code => SubAgentKind::Code,
        BusSubAgentKind::Review => SubAgentKind::Review,
        BusSubAgentKind::Reflect => SubAgentKind::Reflect,
        BusSubAgentKind::Custom(name) => SubAgentKind::Custom(name),
        _ => SubAgentKind::Custom("unknown".to_string()),
    }
}

fn tool_call_from_bus(c: BusToolCall) -> ToolCallRequest {
    ToolCallRequest {
        id: c.id,
        name: c.name,
        args: c.args,
    }
}

fn sub_agent_summary_from_bus(s: BusSubAgent) -> SubAgentSummary {
    SubAgentSummary {
        completed: s.completed,
        summary: s.summary,
        key_findings: s.key_findings,
        budget_exceeded: s.budget_exceeded,
        agent_kind: sub_agent_kind_from_bus(s.agent_kind),
        tokens_used: s.tokens_used,
    }
}

/// Reconstruct a minimal `AgentLoopResult` from a `BusTurnSummary`. The TUI's
/// `ObserverEvent::Complete` handler reads only `final_answer` / `iterations`
/// / `completed` (plus `active_paradigm`), so the missing `Conversation` /
/// `GlobalState` / `sub_agent_results` are defaulted — the bus DTO projection
/// (mirroring `oneai_supervisor::TurnSummary`) intentionally doesn't carry them.
fn result_from_summary(summary: BusTurnSummary) -> AgentLoopResult {
    AgentLoopResult {
        conversation: oneai_core::Conversation::new(),
        final_answer: summary.final_answer,
        global_state: oneai_core::GlobalState::default(),
        iterations: summary.iterations,
        completed: summary.completed,
        active_paradigm: paradigm_from_bus(summary.active_paradigm),
        sub_agent_results: Vec::new(),
    }
}

/// Threshold to auto-proceed tool approvals at or below (mirrors the old
/// `ThresholdInteractionGate` configured with `RiskLevel::Medium`). `None`
/// means surface every tool approval as a card.
#[derive(Clone, Copy)]
pub struct AutoApproveThreshold(pub PermissionLevel);

impl Default for AutoApproveThreshold {
    fn default() -> Self {
        Self(PermissionLevel::from_risk_level(RiskLevel::Medium))
    }
}

/// Spawn the yield→TUI bridge. Drains `EngineYield`s off the bus and forwards
/// each to the TUI's existing `observer_tx` (as `ObserverEvent`) or
/// `interaction_tx` (as `InteractionPendingItem` for approvals the threshold
/// didn't auto-proceed). Returns the task handle (kept alive for the session).
pub fn spawn_yield_bridge(
    bus: Arc<InProcessBus>,
    observer_tx: mpsc::UnboundedSender<ObserverEvent>,
    interaction_tx: mpsc::Sender<InteractionPendingItem>,
    threshold: AutoApproveThreshold,
) -> JoinHandle<()> {
    // EngineBus trait is in scope for `submit`/`subscribe_yields`.
    use oneai_bus::EngineBus;
    let mut rx = bus.subscribe_yields();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(y) => forward_yield(
                    bus.clone(),
                    y,
                    observer_tx.clone(),
                    interaction_tx.clone(),
                    threshold,
                ),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // A lagging subscriber misses n yields — rare under TUI's
                    // 30fps debounce, but a long blocking approval card could
                    // cause it. Log and continue; the next TurnComplete still
                    // arrives to finalize state.
                    tracing::warn!("TUI yield bridge lagged, skipped {n} yields");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

fn forward_yield(
    bus: Arc<InProcessBus>,
    y: EngineYield,
    observer_tx: mpsc::UnboundedSender<ObserverEvent>,
    interaction_tx: mpsc::Sender<InteractionPendingItem>,
    threshold: AutoApproveThreshold,
) {
    let event = match y {
        EngineYield::IterationStart { paradigm, .. } => {
            ObserverEvent::IterationStart(0, paradigm_from_bus(paradigm))
        }
        // The pump owns the Complete event (it reconstructs from the
        // `run_turn_via_bus` return value, matching the old tx2 path that sent
        // Complete after `run_agent` returned). The observer's TurnComplete
        // yield would duplicate it — skip.
        EngineYield::TurnComplete { .. } => return,
        // Turn bracketing was implicit in the direct-drive path (the spawn
        // task WAS the turn); no ObserverEvent equivalent. Skip.
        EngineYield::TurnStart { .. } => return,
        // WorkingState flows through the working-state projector, not the
        // observer. Skip.
        EngineYield::WorkingState { .. } => return,
        EngineYield::SessionEnded => return,
        EngineYield::StreamChunk { text, .. } => ObserverEvent::StreamChunk(text),
        EngineYield::Thinking { text, .. } => ObserverEvent::Thinking(text),
        EngineYield::DirectAnswer { text, .. } => ObserverEvent::DirectAnswer(text),
        EngineYield::ToolCalls { calls, .. } => {
            ObserverEvent::ToolCalls(calls.into_iter().map(tool_call_from_bus).collect())
        }
        EngineYield::ToolResult {
            call_id,
            tool_name,
            output,
            ..
        } => ObserverEvent::ToolResult(call_id, tool_name, output),
        EngineYield::Delegate {
            task, agent_kind, ..
        } => ObserverEvent::Delegate(task, sub_agent_kind_from_bus(agent_kind)),
        EngineYield::DelegateComplete { summary, .. } => {
            ObserverEvent::DelegateComplete(sub_agent_summary_from_bus(summary))
        }
        // The observer's `on_paradigm_switch` reports only the target; the
        // bus yield carries from→to, so use `to`.
        EngineYield::ParadigmSwitch { to, .. } => {
            ObserverEvent::ParadigmSwitch(paradigm_from_bus(to))
        }
        EngineYield::TokenUsage { usage } => ObserverEvent::TokenUsageUpdate(TokenUsage {
            prompt: usage.prompt_tokens,
            completion: usage.completion_tokens,
            total: usage.prompt_tokens + usage.completion_tokens,
            is_estimated: false,
            cache_read: usage.cache_read_tokens,
            cache_creation: usage.cache_creation_tokens,
            ..Default::default()
        }),
        EngineYield::Error { message, .. } => ObserverEvent::Error(message),
        EngineYield::ContextAccounting { accounting, .. } => {
            ObserverEvent::ContextAccountingUpdate(accounting)
        }
        EngineYield::PlanUpdate { plan, .. } => {
            // Deserialize the carried JSON back into PlanState. None or a parse
            // failure (newer PlanState shape than this binary) clears the panel.
            let plan = plan.and_then(|v| serde_json::from_value::<oneai_agent::PlanState>(v).ok());
            ObserverEvent::PlanUpdate(plan)
        }
        EngineYield::ToolsAdded { .. } => {
            // The old direct-drive TuiObserver didn't override
            // `on_tools_added` (default no-op) — the TUI never surfaced
            // self-extension. Skip to preserve behavior.
            return;
        }
        EngineYield::ApprovalRequest {
            request_id,
            request,
        } => {
            handle_approval_request(bus, request_id, request, interaction_tx, threshold);
            return;
        }
        // EngineYield is #[non_exhaustive]; a future variant the TUI doesn't
        // render yet is dropped here (a newer bus crate than this binary).
        _ => return,
    };
    let _ = observer_tx.send(event);
}

/// On an `ApprovalRequest`: apply the threshold pre-filter (auto-proceed
/// at/below-threshold tool approvals, mirroring `ThresholdInteractionGate`),
/// then forward the rest to the TUI's existing approval queue. A per-request
/// forwarder awaits the card's `response_tx` reply and submits the matching
/// `Directive::Approve` back to the bus.
fn handle_approval_request(
    bus: Arc<InProcessBus>,
    request_id: String,
    request: InteractionRequest,
    interaction_tx: mpsc::Sender<InteractionPendingItem>,
    threshold: AutoApproveThreshold,
) {
    use oneai_bus::EngineBus;

    // Threshold auto-proceed for tool approvals (matches the old gate's
    // `should_auto_approve(threshold)` short-circuit). Other decision points
    // (PlanDecision/PlanReview/NetworkApproval/McpElicitation) always reach the
    // card — exactly as the old gate forwarded them after its threshold check.
    if let InteractionRequest::ToolApproval { ref approval } = request {
        let level = approval
            .permission_level
            .unwrap_or_else(|| PermissionLevel::from_risk_level(approval.risk_level));
        if level.should_auto_approve(&threshold.0) {
            let rid = request_id.clone();
            let tool_name = approval.tool_name.clone();
            tokio::spawn(async move {
                // Match the old gate's "Auto-proceeding tool '<name>' …" trace.
                tracing::info!(
                    "Auto-proceeding tool '{}' with permission level {:?} (at/below threshold {:?})",
                    tool_name,
                    level,
                    threshold.0
                );
                let _ = bus
                    .submit(Directive::Approve {
                        request_id: rid,
                        response: InteractionResponse::Proceed,
                    })
                    .await;
            });
            return;
        }
    }

    // Forward to the TUI's approval queue. A forwarder task bridges the card's
    // oneshot reply back to the bus as `Directive::Approve`.
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let item = InteractionPendingItem {
        request,
        response_tx,
    };
    let rid = request_id;
    tokio::spawn(async move {
        // send().await fails only if the TUI dropped the receiver (shutdown);
        // the engine's request_approval then sees BusError::Closed, which is
        // the correct failure mode.
        if interaction_tx.send(item).await.is_err() {
            return;
        }
        if let Ok(response) = response_rx.await {
            let _ = bus
                .submit(Directive::Approve {
                    request_id: rid,
                    response,
                })
                .await;
        }
    });
}

/// Spawn the directive pump. Drains `Directive::UserMessage` →
/// `run_turn_via_bus`; `Directive::Shutdown` stops the pump (the in-process TUI
/// quits via `/quit`, so Shutdown is a no-op here but honored for symmetry with
/// the sidecar shape). `Directive::SwitchParadigm` is deferred (the model drives
/// paradigm via meta-tools; frontend-forced switches are a future nicety).
///
/// `ObserverEvent::Complete` is owned HERE (not by the yield bridge) to match
/// the old direct-drive path's tx2 ordering (Error-before-Complete, then the
/// API-key hint on provider errors).
pub fn spawn_directive_pump(
    mut directive_rx: mpsc::Receiver<Directive>,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>>,
    observer_tx: mpsc::UnboundedSender<ObserverEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(directive) = directive_rx.recv().await {
            match directive {
                Directive::UserMessage { content } => {
                    let task = extract_task(&content);
                    let mut state = session_state.lock().await;
                    let result = state
                        .session
                        .run_turn_via_bus(&task, interrupt_slot.clone())
                        .await;
                    match result {
                        Ok(summary) => {
                            if !summary.completed {
                                let _ = observer_tx.send(ObserverEvent::Error(format!(
                                    "Agent did not reach a final answer after {} iterations.",
                                    summary.iterations
                                )));
                            }
                            let _ = observer_tx
                                .send(ObserverEvent::Complete(result_from_summary(summary)));
                        }
                        Err(e) => {
                            let err_str = e.to_string();
                            let _ =
                                observer_tx.send(ObserverEvent::Error(format!("Error: {err_str}")));
                            if err_str.contains("API error") {
                                let _ = observer_tx.send(ObserverEvent::Error(
                                    "Hint: check your ONEAI_API_KEY and ONEAI_BASE_URL."
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }
                Directive::SwitchParadigm { to } => {
                    tracing::info!(?to, "Directive::SwitchParadigm received — frontend-forced paradigm switch not yet wired (model drives paradigm via meta-tools)");
                }
                Directive::Shutdown => {
                    tracing::info!("Directive::Shutdown received — stopping directive pump");
                    break;
                }
                // Approve / Interrupt are handled by the bus itself and never
                // reach the directive stream. A future variant the pump doesn't
                // act on yet is ignored (newer bus crate than this binary).
                Directive::Approve { .. } | Directive::Interrupt { .. } => {}
                _ => {}
            }
        }
    })
}

/// Extract the task string from a `UserMessage`'s content blocks. Joins all
/// `Text` blocks (the TUI submits a single text block; joining is the honest
/// generalization for multi-block content).
fn extract_task(content: &[oneai_core::ContentBlock]) -> String {
    use oneai_core::ContentBlock;
    let mut out = String::new();
    for block in content {
        if let ContentBlock::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_task_joins_text_blocks() {
        use oneai_core::ContentBlock;
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::Text {
                text: "world".into(),
            },
        ];
        assert_eq!(extract_task(&blocks), "hello\nworld");
    }

    #[test]
    fn paradigm_round_trips() {
        for k in [
            BusParadigmKind::Plan,
            BusParadigmKind::ReAct,
            BusParadigmKind::Reflect,
            BusParadigmKind::Explore,
        ] {
            // bus → agent (this module's free fn) → agent → bus (BusObserver's
            // canonical From impl in oneai-agent, on the oneai_bus type).
            let agent = paradigm_from_bus(k);
            let back = oneai_bus::BusParadigmKind::from(agent);
            assert_eq!(format!("{k:?}"), format!("{back:?}"));
        }
    }

    #[tokio::test]
    async fn yield_bridge_forwards_stream_chunk() {
        use oneai_bus::EngineBus;
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let (observer_tx, mut observer_rx) = mpsc::unbounded_channel();
        let (interaction_tx, _interaction_rx) = mpsc::channel(4);
        let _bridge = spawn_yield_bridge(
            bus.clone(),
            observer_tx,
            interaction_tx,
            AutoApproveThreshold::default(),
        );
        // emit a StreamChunk; the bridge should forward it as ObserverEvent.
        let _ = bus.emit(EngineYield::StreamChunk {
            turn_id: "t1".into(),
            text: "hi".into(),
        });
        let ev = observer_rx.recv().await.expect("event");
        match ev {
            ObserverEvent::StreamChunk(s) => assert_eq!(s, "hi"),
            other => panic!("expected StreamChunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn yield_bridge_threshold_auto_proceeds_medium_tool() {
        use oneai_bus::EngineBus;
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let (observer_tx, _observer_rx) = mpsc::unbounded_channel();
        let (interaction_tx, mut interaction_rx) = mpsc::channel(4);
        let _bridge = spawn_yield_bridge(
            bus.clone(),
            observer_tx,
            interaction_tx,
            AutoApproveThreshold::default(),
        );

        // Engine requests approval for a Medium-risk (Standard) tool.
        let bus_clone = bus.clone();
        let task = tokio::spawn(async move {
            bus_clone
                .request_approval(InteractionRequest::ToolApproval {
                    approval: oneai_core::ApprovalRequest {
                        tool_name: "read_file".into(),
                        args: serde_json::json!({}),
                        risk_level: RiskLevel::Medium,
                        permission_level: Some(PermissionLevel::Standard),
                        justification: "test".into(),
                    },
                })
                .await
        });

        // The bridge should auto-proceed (no card in interaction_rx).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            interaction_rx.try_recv().is_err(),
            "at-threshold tool should not reach the approval card"
        );
        let resp = task.await.unwrap().unwrap();
        assert!(matches!(resp, InteractionResponse::Proceed));
    }

    #[tokio::test]
    async fn yield_bridge_forwards_high_risk_tool_to_card() {
        use oneai_bus::EngineBus;
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let (observer_tx, _observer_rx) = mpsc::unbounded_channel();
        let (interaction_tx, mut interaction_rx) = mpsc::channel(4);
        let _bridge = spawn_yield_bridge(
            bus.clone(),
            observer_tx,
            interaction_tx,
            AutoApproveThreshold::default(),
        );

        let bus_clone = bus.clone();
        let _task = tokio::spawn(async move {
            bus_clone
                .request_approval(InteractionRequest::ToolApproval {
                    approval: oneai_core::ApprovalRequest {
                        tool_name: "shell".into(),
                        args: serde_json::json!({}),
                        risk_level: RiskLevel::High,
                        permission_level: Some(PermissionLevel::Full),
                        justification: "test".into(),
                    },
                })
                .await
        });

        // Above-threshold tool → card in the queue.
        let item =
            tokio::time::timeout(std::time::Duration::from_millis(200), interaction_rx.recv())
                .await
                .expect("timed out")
                .expect("item");
        match item.request {
            InteractionRequest::ToolApproval { approval } => {
                assert_eq!(approval.tool_name, "shell");
            }
            other => panic!("expected ToolApproval, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn approval_card_reply_round_trips_via_bus() {
        use oneai_bus::EngineBus;
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let (observer_tx, _observer_rx) = mpsc::unbounded_channel();
        let (interaction_tx, mut interaction_rx) = mpsc::channel(4);
        let _bridge = spawn_yield_bridge(
            bus.clone(),
            observer_tx,
            interaction_tx,
            AutoApproveThreshold::default(),
        );

        let bus_clone = bus.clone();
        let task = tokio::spawn(async move {
            bus_clone
                .request_approval(InteractionRequest::ToolApproval {
                    approval: oneai_core::ApprovalRequest {
                        tool_name: "shell".into(),
                        args: serde_json::json!({}),
                        risk_level: RiskLevel::High,
                        permission_level: Some(PermissionLevel::Full),
                        justification: "test".into(),
                    },
                })
                .await
        });

        let item = interaction_rx.recv().await.expect("item");
        let _ = item.response_tx.send(InteractionResponse::Abort {
            reason: "denied".into(),
        });
        let resp = task.await.unwrap().unwrap();
        assert!(matches!(resp, InteractionResponse::Abort { .. }));
    }
}
