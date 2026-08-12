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
                Directive::UpdateConfig {
                    plan_mode: Some(on),
                } => {
                    // Hot-sync session config. Currently just plan_mode (Plan
                    // blocks tool execution); provider/model/memory overrides
                    // are future fields. FIFO-ordered before any UserMessage,
                    // so the next turn sees the new config. `plan_mode: None`
                    // (leave unchanged) matches the catch-all below.
                    session_state.lock().await.session.set_plan_mode(on);
                }
                Directive::Compact { keep_recent_turns } => {
                    // /compact: LLM-summarize the backend conversation in place.
                    // Holds the session lock for the call (a concurrent
                    // UserMessage blocks — same as the old direct path).
                    let outcome = {
                        let mut state = session_state.lock().await;
                        state.session.compact(keep_recent_turns).await
                    };
                    let _ = match &outcome {
                        Ok(o) if o.summary.is_empty() => {
                            bus.emit(oneai_bus::EngineYield::CompactResult {
                                summary: String::new(),
                                removed_count: 0,
                                retained: Vec::new(),
                            })
                        }
                        Ok(o) => bus.emit(oneai_bus::EngineYield::CompactResult {
                            summary: o.summary.clone(),
                            removed_count: o.removed_count,
                            retained: o
                                .retained
                                .iter()
                                .map(|(r, t)| (r.clone(), t.clone()))
                                .collect(),
                        }),
                        Err(e) => bus.emit(oneai_bus::EngineYield::Error {
                            recoverable: false,
                            message: format!("Error: {e}"),
                        }),
                    };
                }
                Directive::InitProject {
                    format,
                    force,
                    no_llm,
                } => {
                    // /init: generate a project-instruction file. Runs the
                    // probe/LLM synthesis WITHOUT the session lock (only the
                    // provider is borrowed under a short lock).
                    let fmt = format
                        .as_deref()
                        .and_then(|s| {
                            oneai_domain::project_info::ProjectInfoFormat::from_name(s).ok()
                        })
                        .unwrap_or_default();
                    let opts = oneai_domain::project_info::ProjectInfoOptions {
                        format: fmt,
                        force,
                        ..Default::default()
                    };
                    let cwd =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    let dir = std::fs::canonicalize(&cwd).unwrap_or(cwd);
                    let provider: Option<std::sync::Arc<dyn oneai_core::traits::LlmProvider>> =
                        if no_llm {
                            None
                        } else {
                            session_state.lock().await.session.provider().cloned()
                        };
                    let result = match &provider {
                        Some(p) => {
                            oneai_domain::project_info::generate_project_info_with_llm(
                                &dir, &opts, &**p,
                            )
                            .await
                        }
                        None => {
                            oneai_domain::project_info::generate_project_info(&dir, &opts).await
                        }
                    };
                    let format_label = opts.format.label().to_string();
                    let msg = match &result {
                        Ok(r) if r.skipped => format!(
                            "⊘ {} already exists — left untouched.\nRe-run `/init --force` to overwrite, or edit it directly.",
                            r.path.display()
                        ),
                        Ok(r) => {
                            let verb = if r.overwritten { "Overwrote" } else { "Created" };
                            let mode = if r.llm_generated {
                                "LLM-synthesized"
                            } else {
                                "heuristic"
                            };
                            if !r.llm_generated && provider.is_some() {
                                format!(
                                    "✅ {} {} ({})\n⚠  LLM synthesis failed — wrote a heuristic doc instead. Check ONEAI_API_KEY or use --no-llm.\nFormat: {} — loaded into agent context on next session.",
                                    verb, r.path.display(), mode, format_label
                                )
                            } else {
                                format!(
                                    "✅ {} {} ({})\nFormat: {} — loaded into agent context on next session.\nEdit it to add project conventions & constraints.",
                                    verb, r.path.display(), mode, format_label
                                )
                            }
                        }
                        Err(e) => format!("✗ /init failed: {}", e),
                    };
                    let _ = bus.emit(oneai_bus::EngineYield::InitResult { message: msg });
                }
                Directive::CreateSession { id } => {
                    // Start a fresh session. `id` None ⇒ engine assigns a new
                    // uuid (the common /new path); Some ⇒ bind to that id.
                    let new_id = match id {
                        Some(wanted) => {
                            let new_session = session_state
                                .lock()
                                .await
                                .app
                                .create_session_with_id(&wanted)
                                .await;
                            let nid = new_session.session_id().to_string();
                            session_state.lock().await.session = new_session;
                            nid
                        }
                        None => {
                            let mut state = session_state.lock().await;
                            state.reset_session();
                            state.session.session_id().to_string()
                        }
                    };
                    let _ = bus.emit(oneai_bus::EngineYield::SessionCreated { id: new_id });
                }
                Directive::LoadSession { id } => {
                    // Load a saved session by full id or unique short prefix
                    // (issue #23: a bare short id resolved to an empty
                    // conversation left the model amnesiac — resolve here, and
                    // emit the message history so the frontend rebuilds).
                    let (resolved, msgs) = {
                        let mut state = session_state.lock().await;
                        let sessions = state.app.list_conversations().await;
                        let resolved = if sessions.iter().any(|s| s.id == id) {
                            id.clone()
                        } else {
                            let matches: Vec<_> =
                                sessions.iter().filter(|s| s.id.starts_with(&id)).collect();
                            match matches.len() {
                                1 => matches[0].id.clone(),
                                _ => id.clone(),
                            }
                        };
                        let new_session = state.app.create_session_with_id(&resolved).await;
                        let msgs = new_session.conversation().messages.clone();
                        state.session = new_session;
                        (resolved, msgs)
                    };
                    tracing::info!(
                        "[LoadSession] requested={} resolved={} loaded_msgs={} (0 => not found / empty)",
                        id,
                        resolved,
                        msgs.len()
                    );
                    let _ = bus.emit(oneai_bus::EngineYield::SessionLoaded {
                        id: resolved,
                        messages: msgs,
                    });
                }
                Directive::ClearSession => {
                    // Clear the live conversation — fresh backend, new id.
                    let new_id = {
                        let mut state = session_state.lock().await;
                        state.reset_session();
                        state.session.session_id().to_string()
                    };
                    let _ = bus.emit(oneai_bus::EngineYield::SessionCleared { id: new_id });
                }
                Directive::DeleteSession { id } => {
                    // Delete a saved session from the durable store.
                    let result = session_state
                        .lock()
                        .await
                        .app
                        .delete_conversation(&id)
                        .await;
                    let _ = match result {
                        Ok(()) => bus.emit(oneai_bus::EngineYield::SessionDeleted { id }),
                        Err(e) => bus.emit(oneai_bus::EngineYield::Error {
                            recoverable: false,
                            message: format!("Error: {e}"),
                        }),
                    };
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
