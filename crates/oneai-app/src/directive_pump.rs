//! Directive pump — the shared engine-driver half of the bus.
//!
//! A frontend (TUI, sidecar) holds the OTHER half: it consumes
//! `EngineYield`s directly off `bus.subscribe_yields()`. This module drains
//! `Directive::UserMessage` (and friends) off the directive stream the bus
//! forwards to, and turns each into an `AppSession` action via a
//! [`DirectiveRuntime`] implementation.
//!
//! Extracted from the TUI's `bus_consumer.rs` so the sidecar (`oneai serve`)
//! drives the engine with the *same* dispatch — zero drift between in-process
//! and sidecar frontends. The TUI's `SessionState` and the sidecar's
//! `SidecarRuntime` both impl [`DirectiveRuntime`]; the pump is generic over it.
//!
//! `Approve` / `Interrupt` never reach this pump — the bus resolves them itself.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_agent::{AgentLoop, ParadigmKind, SubAgentKind, SubAgentSummary, ToolCallRequest};
use oneai_bus::{
    BusParadigmKind, BusSubAgent, BusSubAgentKind, BusToolCall, BusTurnSummary, Directive,
    EngineBus, EngineYield, InProcessBus,
};
use oneai_core::{traits::LlmProvider, ContentBlock, Message};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::CompactOutcome;
use oneai_core::error::Result;

// ─── bus → agent DTO conversions ────────────────────────────────────────────
//
// Free functions rather than `impl From` — the orphan rule forbids a foreign
// trait (`From`) impl between two foreign types. These mirror `BusObserver`'s
// canonical forward conversions (agent → bus).

/// Map a bus paradigm back to the agent enum. An unknown variant from a newer
/// bus crate defaults to `ReAct` (the loop's own default).
pub fn paradigm_from_bus(k: BusParadigmKind) -> ParadigmKind {
    match k {
        BusParadigmKind::Plan => ParadigmKind::Plan,
        BusParadigmKind::ReAct => ParadigmKind::ReAct,
        BusParadigmKind::Reflect => ParadigmKind::Reflect,
        BusParadigmKind::Explore => ParadigmKind::Explore,
        _ => ParadigmKind::ReAct,
    }
}

pub fn sub_agent_kind_from_bus(k: BusSubAgentKind) -> SubAgentKind {
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

pub fn tool_call_from_bus(c: BusToolCall) -> ToolCallRequest {
    ToolCallRequest {
        id: c.id,
        name: c.name,
        args: c.args,
    }
}

pub fn sub_agent_summary_from_bus(s: BusSubAgent) -> SubAgentSummary {
    SubAgentSummary {
        completed: s.completed,
        summary: s.summary,
        key_findings: s.key_findings,
        budget_exceeded: s.budget_exceeded,
        agent_kind: sub_agent_kind_from_bus(s.agent_kind),
        tokens_used: s.tokens_used,
    }
}

/// The engine-side abstraction a frontend's session holder implements. Each
/// method locks the holder itself (the pump holds an `Arc<Mutex<R>>` and
/// locks per directive, exactly as the TUI did before the extraction).
#[async_trait]
pub trait DirectiveRuntime: Send {
    /// Run one agent turn driven by the bus. Emits the intermediate yields
    /// via the `BusObserver` wired inside; returns the projected summary.
    async fn run_turn(
        &mut self,
        task: &str,
        interrupt_slot: Arc<Mutex<Option<AgentLoop>>>,
    ) -> Result<BusTurnSummary>;

    /// Force-set the active paradigm; return the previous one (if any).
    async fn set_paradigm(&mut self, to: ParadigmKind) -> Option<ParadigmKind>;

    /// Hot-set plan mode (Plan blocks tool execution).
    async fn set_plan_mode(&mut self, on: bool);

    /// Compact the backend conversation via LLM summarization.
    async fn compact(&mut self, keep_recent_turns: usize) -> Result<CompactOutcome>;

    /// Borrow the configured provider (sync — the pump holds the guard; used
    /// by `/init`'s short-lock-then-await-LLM path).
    fn provider(&self) -> Option<Arc<dyn LlmProvider>>;

    /// Start a fresh session. `id` = None ⇒ engine assigns; Some ⇒ bind to it.
    /// Returns the new session id.
    async fn create_session(&mut self, id: Option<String>) -> String;

    /// Load a saved session by id (or unique short prefix). Returns the
    /// resolved id and the loaded message history.
    async fn load_session(&mut self, id: String) -> (String, Vec<Message>);

    /// Clear the live conversation — fresh backend, new id. Returns the new id.
    async fn reset_session(&mut self) -> String;

    /// Delete a saved session from the durable store.
    async fn delete_session(&mut self, id: String) -> Result<()>;

    /// Current session id (owned — crosses awaits).
    async fn session_id(&mut self) -> String;

    // ── Group-chat methods (P4) ─────────────────────────────────────────
    // Default impls error out so a runtime that isn't group-aware (the TUI's
    // `SessionState`, the sidecar's `SidecarRuntime`) typechecks unchanged —
    // group chat is driven by the in-process 3-symbol c_facade runtime, which
    // overrides these. The pump dispatches `StartGroupChat`/`GroupStart`/
    // `GroupUserMessage`/`GroupSetScriptedOrder` directives to them.

    /// Build a multi-agent `GroupChatSession` from a scenario; subsequent
    /// `GroupStart`/`GroupUserMessage` directives drive it. Displaces the
    /// single-agent session for the group's lifetime.
    async fn start_group(&mut self, _scenario: oneai_bus::BusGroupScenario) -> Result<()> {
        Err(oneai_core::error::OneAIError::Agent(
            "group chat not active on this runtime".into(),
        ))
    }

    /// Run the scenario's configured opener turn.
    async fn group_start(&mut self) -> Result<()> {
        Err(oneai_core::error::OneAIError::Agent(
            "group chat not active on this runtime".into(),
        ))
    }

    /// Append the user's message and run the round's speakers per the turn
    /// policy until it's the user's turn again.
    async fn group_run_task(&mut self, _user_input: &str) -> Result<()> {
        Err(oneai_core::error::OneAIError::Agent(
            "group chat not active on this runtime".into(),
        ))
    }

    /// Hot-swap the group's turn policy to a fixed scripted order at runtime.
    async fn group_set_scripted_order(&mut self, _order: Vec<String>) {
        // Default no-op — a non-group runtime silently ignores the directive.
    }
}

/// Spawn the directive pump. Drains `Directive::UserMessage` →
/// [`DirectiveRuntime::run_turn`]; `Directive::Shutdown` stops the pump.
/// Turn-level errors (the `run_turn` outer `Result`) are emitted back onto
/// the bus as [`EngineYield::Error`] so the frontend's Error arm renders them
/// — one channel, one schema, no separate observer channel.
pub fn spawn_directive_pump<R: DirectiveRuntime + 'static>(
    mut directive_rx: mpsc::Receiver<Directive>,
    rt: Arc<Mutex<R>>,
    interrupt_slot: Arc<Mutex<Option<AgentLoop>>>,
    bus: Arc<InProcessBus>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(directive) = directive_rx.recv().await {
            match directive {
                Directive::UserMessage { content } => {
                    let task = extract_task(&content);
                    let result = {
                        let mut rt = rt.lock().await;
                        rt.run_turn(&task, interrupt_slot.clone()).await
                    };
                    if let Err(e) = result {
                        let err = e.to_string();
                        let _ = bus.emit(EngineYield::Error {
                            recoverable: false,
                            message: format!("Error: {err}"),
                        });
                        if err.contains("API error") {
                            let _ = bus.emit(EngineYield::Error {
                                recoverable: false,
                                message: "Hint: check your ONEAI_API_KEY and ONEAI_BASE_URL."
                                    .to_string(),
                            });
                        }
                    }
                    // Ok(summary) → BusObserver::on_complete already emitted
                    // TurnComplete; the !completed diagnostic is handled in
                    // the frontend's Complete arm.
                }
                Directive::SwitchParadigm { to } => {
                    // Frontend-forced paradigm switch. Persist it on the
                    // session (sticky across turns); the next run_turn
                    // materializes it at turn start. Emit ParadigmSwitch so the
                    // frontend reflects immediately.
                    let (turn_id, from, to_bus) = {
                        let mut rt = rt.lock().await;
                        let target = paradigm_from_bus(to);
                        let prev = rt.set_paradigm(target).await;
                        let from = BusParadigmKind::from(prev.unwrap_or(ParadigmKind::ReAct));
                        let to_bus = BusParadigmKind::from(target);
                        let turn_id = rt.session_id().await;
                        (turn_id, from, to_bus)
                    };
                    let _ = bus.emit(EngineYield::ParadigmSwitch {
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
                    rt.lock().await.set_plan_mode(on).await;
                }
                Directive::Compact { keep_recent_turns } => {
                    // /compact: LLM-summarize the backend conversation in place.
                    // Holds the session lock for the call (a concurrent
                    // UserMessage blocks — same as the old direct path).
                    let outcome = { rt.lock().await.compact(keep_recent_turns).await };
                    let _ = match &outcome {
                        Ok(o) if o.summary.is_empty() => bus.emit(EngineYield::CompactResult {
                            summary: String::new(),
                            removed_count: 0,
                            retained: Vec::new(),
                        }),
                        Ok(o) => bus.emit(EngineYield::CompactResult {
                            summary: o.summary.clone(),
                            removed_count: o.removed_count,
                            retained: o
                                .retained
                                .iter()
                                .map(|(r, t)| (r.clone(), t.clone()))
                                .collect(),
                        }),
                        Err(e) => bus.emit(EngineYield::Error {
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
                    let provider: Option<Arc<dyn LlmProvider>> = if no_llm {
                        None
                    } else {
                        rt.lock().await.provider()
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
                    let _ = bus.emit(EngineYield::InitResult { message: msg });
                }
                Directive::CreateSession { id } => {
                    // Start a fresh session. `id` None ⇒ engine assigns; Some ⇒ bind.
                    let new_id = { rt.lock().await.create_session(id).await };
                    let _ = bus.emit(EngineYield::SessionCreated { id: new_id });
                }
                Directive::LoadSession { id } => {
                    // Load a saved session by full id or unique short prefix
                    // (issue #23: a bare short id resolved to an empty
                    // conversation left the model amnesiac — resolve here, and
                    // emit the message history so the frontend rebuilds).
                    let (resolved, msgs) = { rt.lock().await.load_session(id.clone()).await };
                    tracing::info!(
                        "[LoadSession] requested={} resolved={} loaded_msgs={} (0 => not found / empty)",
                        id,
                        resolved,
                        msgs.len()
                    );
                    let _ = bus.emit(EngineYield::SessionLoaded {
                        id: resolved,
                        messages: msgs,
                    });
                }
                Directive::ClearSession => {
                    // Clear the live conversation — fresh backend, new id.
                    let new_id = { rt.lock().await.reset_session().await };
                    let _ = bus.emit(EngineYield::SessionCleared { id: new_id });
                }
                Directive::DeleteSession { id } => {
                    let result = { rt.lock().await.delete_session(id.clone()).await };
                    let _ = match result {
                        Ok(()) => bus.emit(EngineYield::SessionDeleted { id }),
                        Err(e) => bus.emit(EngineYield::Error {
                            recoverable: false,
                            message: format!("Error: {e}"),
                        }),
                    };
                }
                Directive::Shutdown => {
                    tracing::info!("Directive::Shutdown received — stopping directive pump");
                    break;
                }
                // ── Group-chat directives (P4) ────────────────────────────
                // Drive a GroupChatSession through `GroupChatBusObserver`,
                // emitting speaker-tagged yields. `Init` never reaches the
                // pump (the in-process c_facade intercepts it to build the
                // engine+bus+pump); a stray `Init` from a sidecar is ignored.
                Directive::Init { .. } => {
                    tracing::warn!(
                        "Directive::Init reached the pump — it should be intercepted by the \
                         in-process c_facade; ignoring"
                    );
                }
                Directive::StartGroupChat { scenario } => {
                    let result = rt.lock().await.start_group(scenario).await;
                    if let Err(e) = result {
                        let _ = bus.emit(EngineYield::Error {
                            recoverable: true,
                            message: format!("group chat start failed: {e}"),
                        });
                    }
                }
                Directive::GroupStart => {
                    let result = rt.lock().await.group_start().await;
                    if let Err(e) = result {
                        let _ = bus.emit(EngineYield::Error {
                            recoverable: true,
                            message: format!("group start failed: {e}"),
                        });
                    }
                }
                Directive::GroupUserMessage { user_input } => {
                    let result = rt.lock().await.group_run_task(&user_input).await;
                    if let Err(e) = result {
                        let _ = bus.emit(EngineYield::Error {
                            recoverable: true,
                            message: format!("group run_task failed: {e}"),
                        });
                    }
                }
                Directive::GroupSetScriptedOrder { order } => {
                    rt.lock().await.group_set_scripted_order(order).await;
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
/// `Text` blocks (a frontend submits a single text block; joining is the
/// honest generalization for multi-block content).
pub fn extract_task(content: &[ContentBlock]) -> String {
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
            let back = BusParadigmKind::from(agent);
            assert_eq!(format!("{k:?}"), format!("{back:?}"));
        }
    }
}
