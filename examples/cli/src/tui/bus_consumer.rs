//! Bus consumer — the directive pump. The TUI main loop consumes
//! `EngineYield`s directly off `bus.subscribe_yields()` (one channel, one
//! schema); this module owns the OTHER half: a background task that drains
//! `Directive::UserMessage` off the directive stream and drives
//! `AppSession::run_turn_via_bus`.
//!
//! Yield→TUI translation is gone (the TUI matches on `EngineYield` directly in
//! `process_yield`). Approvals round-trip via `InProcessBus::resolve_approval`
//! (sync) — no oneshot captured in the card, no forwarder task. This module
//! holds only the pump + the bus→agent DTO helpers `process_yield` shares.

use std::sync::Arc;

use oneai_agent::{ParadigmKind, SubAgentKind, SubAgentSummary, ToolCallRequest};
use oneai_bus::{BusParadigmKind, BusSubAgent, BusSubAgentKind, BusToolCall, Directive, EngineBus};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::session::SessionState;

// ─── bus → agent DTO conversions ────────────────────────────────────────────
//
// Free functions rather than `impl From` — the orphan rule forbids a foreign
// trait (`From`) impl between two foreign types. These mirror `BusObserver`'s
// canonical forward conversions (agent → bus).

pub(crate) fn paradigm_from_bus(k: BusParadigmKind) -> ParadigmKind {
    match k {
        BusParadigmKind::Plan => ParadigmKind::Plan,
        BusParadigmKind::ReAct => ParadigmKind::ReAct,
        BusParadigmKind::Reflect => ParadigmKind::Reflect,
        BusParadigmKind::Explore => ParadigmKind::Explore,
        // BusParadigmKind is #[non_exhaustive]; an unknown variant from a newer
        // bus crate can't map losslessly — default to ReAct (the loop's own
        // default) so the TUI still renders.
        _ => ParadigmKind::ReAct,
    }
}

pub(crate) fn sub_agent_kind_from_bus(k: BusSubAgentKind) -> SubAgentKind {
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

pub(crate) fn tool_call_from_bus(c: BusToolCall) -> ToolCallRequest {
    ToolCallRequest {
        id: c.id,
        name: c.name,
        args: c.args,
    }
}

pub(crate) fn sub_agent_summary_from_bus(s: BusSubAgent) -> SubAgentSummary {
    SubAgentSummary {
        completed: s.completed,
        summary: s.summary,
        key_findings: s.key_findings,
        budget_exceeded: s.budget_exceeded,
        agent_kind: sub_agent_kind_from_bus(s.agent_kind),
        tokens_used: s.tokens_used,
    }
}

/// Spawn the directive pump. Drains `Directive::UserMessage` →
/// `run_turn_via_bus`; `Directive::Shutdown` stops the pump. Turn-level errors
/// (the `run_turn_via_bus` outer `Result`) are emitted back onto the bus as
/// `EngineYield::Error` so the TUI's `process_yield` Error arm renders them —
/// one channel, one schema, no separate observer channel.
///
/// `EngineYield::TurnComplete` (emitted by `BusObserver::on_complete` inside
/// `run_turn_via_bus`) drives the TUI's Complete arm directly; the pump does
/// NOT inject Complete/Error for the `!completed` case — that diagnostic is
/// folded into the Complete arm itself.
pub fn spawn_directive_pump(
    mut directive_rx: mpsc::Receiver<Directive>,
    session_state: Arc<tokio::sync::Mutex<SessionState>>,
    interrupt_slot: Arc<tokio::sync::Mutex<Option<oneai_agent::AgentLoop>>>,
    bus: Arc<oneai_bus::InProcessBus>,
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
                    if let Err(e) = result {
                        let err = e.to_string();
                        let _ = bus.emit(oneai_bus::EngineYield::Error {
                            recoverable: false,
                            message: format!("Error: {err}"),
                        });
                        if err.contains("API error") {
                            let _ = bus.emit(oneai_bus::EngineYield::Error {
                                recoverable: false,
                                message: "Hint: check your ONEAI_API_KEY and ONEAI_BASE_URL."
                                    .to_string(),
                            });
                        }
                    }
                    // Ok(summary) → BusObserver::on_complete already emitted
                    // TurnComplete; the !completed diagnostic is handled in the
                    // Complete arm.
                }
                Directive::SwitchParadigm { to } => {
                    // Frontend-forced paradigm switch. Persist it on the
                    // session (sticky across turns via conversation.metadata);
                    // the next run_turn_via_bus materializes it at turn start.
                    // Emit ParadigmSwitch so the TUI reflects immediately (the
                    // IterationStart yield on the next turn confirms it).
                    let target = paradigm_from_bus(to);
                    let mut state = session_state.lock().await;
                    let prev = state.session.set_paradigm(target);
                    let from =
                        oneai_bus::BusParadigmKind::from(prev.unwrap_or(ParadigmKind::ReAct));
                    let to_bus = oneai_bus::BusParadigmKind::from(target);
                    let turn_id = state.session.session_id().to_string();
                    let _ = bus.emit(oneai_bus::EngineYield::ParadigmSwitch {
                        turn_id,
                        from,
                        to: to_bus,
                    });
                    tracing::info!(
                        ?to_bus,
                        "Directive::SwitchParadigm applied — next turn starts under it"
                    );
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
pub(crate) fn extract_task(content: &[oneai_core::ContentBlock]) -> String {
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
            // bus → agent (this module) → agent → bus (BusObserver's canonical
            // From impl in oneai-agent, on the oneai_bus type).
            let agent = paradigm_from_bus(k);
            let back = oneai_bus::BusParadigmKind::from(agent);
            assert_eq!(format!("{k:?}"), format!("{back:?}"));
        }
    }
}
