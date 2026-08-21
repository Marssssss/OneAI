//! Agentic Loop — dynamic loop where each iteration decides the next action
//! based on model output (DirectAnswer / ToolCalls / Delegate / SwitchParadigm).
//!
//! This replaces the fixed pipeline (Plan → Parallel → ReAct → Reflect)
//! with a dynamic loop inspired by Claude Code's Agentic Loop architecture.
//!
//! Key differences from the old AgentRunner::run():
//! - No fixed ordering of paradigms — the model decides dynamically
//! - Supports direct answers (loop ends immediately)
//! - Supports delegation to sub-agents (hierarchical task decomposition)
//! - Supports paradigm switching (from ReAct → Plan → ReAct, etc.)
//! - Iteration limit is governed by TokenBudget, not hardcoded max_iterations
//! - Context compression is triggered automatically per iteration
//! - Skill injection happens per iteration with automatic unload
//! - Checkpoints are saved automatically per iteration

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use oneai_core::budget::TokenBudget;
use oneai_core::error::Result;
use oneai_core::traits::{InteractionGate, LlmProvider, OutputParser, Tool};
use oneai_core::{
    ConstrainedMode, ConstrainedOutputConfig, ConstrainedOutputPolicy, ContentBlock, Conversation,
    HookContext, HookPoint, InferenceRequest, InferenceResponse, InteractionModification,
    InteractionPoint, InteractionRequest, InteractionResponse, InterruptPoint, InterruptReason,
    Message, ResumeAction, ResumeSignal, Role, StructuredOutputConfig, ToolDefinition, ToolOutput,
};

use oneai_domain::{MergedDomainPack, PermissionAction};

use crate::context_assembler::ContextAssembler;
use crate::hooks::{HookRegistry, ResolvedHookAction};
use crate::streaming::IncrementalStreamParser;
use crate::structured_output::{build_retry_prompt, validate_json_schema};
use crate::sub_agent::{SubAgentFactory, SubAgentKind, SubAgentSummary};
use oneai_trace::{EventKind, SpanKind, SpanStatus, TraceContext};
// OtelMetricsProvider is only exported by oneai-trace when its `otel` feature
// is on (oneai-agent's `otel` feature forwards it). The metrics wiring below is
// cfg-gated so the non-otel build stays zero-cost.
#[cfg(feature = "otel")]
use oneai_trace::OtelMetricsProvider;

// ─── AgentLoopObserver ─────────────────────────────────────────────────────

/// Observer callback trait — allows external UI (CLI, desktop app) to
/// receive real-time events during the Agentic Loop execution.
///
/// This enables the interactive CLI to show tool calls, paradigm switches,
/// and intermediate results as they happen, rather than only showing
/// the final answer after the loop completes.
pub trait AgentLoopObserver: Send + Sync {
    /// Called when a new iteration begins.
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind);

    /// Called when the model produces a DirectAnswer (loop will end).
    fn on_direct_answer(&self, text: &str);

    /// Called when the model decides to call tools.
    fn on_tool_calls(&self, calls: &[ToolCallRequest]);

    /// Called after a tool call completes (with its result).
    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &ToolOutput);

    /// Called when the model delegates to a sub-agent.
    fn on_delegate(&self, id: &str, task: &str, agent_type: &SubAgentKind);

    /// Called when a delegated sub-agent finishes and its summary is fed back
    /// into the parent conversation. Pairs with `on_delegate` so the UI can
    /// show the full sub-agent lifecycle (start → completion) instead of only
    /// the start. `id` matches the `on_delegate` call so a frontend can fan
    /// the completion onto the right sub-agent card (incl. across turns for
    /// background delegations). Default empty to keep existing implementations
    /// compiling.
    fn on_delegate_complete(&self, _id: &str, _summary: &crate::sub_agent::SubAgentSummary) {}

    /// Called with mid-run progress from a delegated sub-agent (Opt 1
    /// Op-channel-lite). Sub-agent iteration/tool-result/usage events are
    /// forwarded here by [`spawn_sub_agents_batch`](struct.AgentLoop.html)
    /// so the parent UI is not blind during a possibly-minutes-long
    /// delegation. Default empty so existing observers keep compiling. See
    /// [`DelegateProgressEvent`].
    fn on_delegate_progress(
        &self,
        _delegate_id: &str,
        _kind: &crate::sub_agent::SubAgentKind,
        _event: &DelegateProgressEvent,
    ) {
    }

    /// Called when the model switches to a different paradigm.
    fn on_paradigm_switch(&self, paradigm: ParadigmKind);

    /// Called when a checkpoint is saved.
    fn on_checkpoint(&self, iteration: usize);

    /// Called when the loop completes with the final result.
    fn on_complete(&self, result: &AgentLoopResult);

    /// Called for each text fragment during streaming inference.
    /// Enables typewriter effect in the UI.
    fn on_stream_chunk(&self, _text: &str) {}

    /// Called when the model produces thinking/reasoning content (extended thinking).
    /// Each call contains a fragment of the thinking text (streaming).
    fn on_thinking(&self, _text: &str) {}

    /// Called after each inference with token usage stats.
    fn on_token_usage(&self, _prompt_tokens: u32, _completion_tokens: u32) {}

    /// Called after each inference with the **full** token-usage breakdown,
    /// including prompt-cache tokens (`cache_read` / `cache_creation`). This is
    /// the cache-aware successor to [`AgentLoopObserver::on_token_usage`]; the
    /// default delegates to the legacy method so existing implementations keep
    /// working. Override this to surface the cache-hit ratio in the UI.
    fn on_token_usage_full(
        &self,
        prompt_tokens: u32,
        completion_tokens: u32,
        cache_read_tokens: u32,
        cache_creation_tokens: u32,
    ) {
        let _ = (cache_read_tokens, cache_creation_tokens);
        self.on_token_usage(prompt_tokens, completion_tokens);
    }

    /// Called after assembling the context for each iteration, with a breakdown
    /// of how the context window is occupied. This includes the full assembled
    /// conversation (system prompt, tool defs, context sources, messages),
    /// not just the bare session conversation.
    fn on_context_accounting(&self, _accounting: &oneai_core::ContextAccounting) {}

    /// Called when the loop is interrupted (paused at an iteration boundary).
    /// The UI can display the interrupt reason and await human feedback.
    fn on_interrupt(&self, _point: &InterruptPoint) {}

    /// Called when the loop resumes from an interrupt with human feedback.
    fn on_resume(&self, _signal: &ResumeSignal) {}

    /// Called when the plan state changes (task created/updated). The TUI uses
    /// this to re-render the persistent plan panel. `None` means the plan was
    /// cleared; `Some(plan)` is the current state (clone).
    fn on_plan_update(&self, _plan: Option<&crate::plan_state::PlanState>) {}

    /// Called when a cadence-fired `Reflect` sub-agent finishes (Phase 2.1
    /// Stage A). The summary is NOT injected into the parent conversation —
    /// the UI may surface it as a transient side-note. Default empty so
    /// existing observers keep compiling.
    fn on_reflection(&self, _summary: &str) {}

    /// Called when a tool batch causes new tools to become active in the
    /// schema (self-extension, evolution-plan §3.4) — either because a tool
    /// self-reported them via `ToolOutput::added_tool_names` or because the
    /// loop's live `ToolRegistry` diff detected a mid-turn registration /
    /// Footprint-gate flip. The model is separately nudged via a one-shot
    /// system note (`pending_new_tools_note`); this event lets the UI log the
    /// extension. Default empty so existing observers keep compiling.
    fn on_tools_added(&self, _names: &[String]) {}
}

/// What fired a cadence-fired `Reflect` sub-agent — telemetry only
/// (Phase 2.1 Stage A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflectionTrigger {
    /// Mid-run: `iterations` crossed a multiple of `reflection_cadence`.
    Cadence,
    /// End-of-run: the loop just delivered a `DirectAnswer`.
    DirectAnswer,
}

// ─── AgentDecision ──────────────────────────────────────────────────────────

// ─── DelegateProgressEvent ───────────────────────────────────────────────────

/// A coarse mid-run progress event forwarded from a delegated sub-agent to
/// the parent loop's observer (Opt 1). Only the high-signal events the
/// parent/UI cares about are surfaced — full per-token streams stay inside
/// the sub-agent to avoid event flooding.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DelegateProgressEvent {
    /// The sub-agent started a new iteration under `paradigm`.
    IterationStart {
        iteration: usize,
        paradigm: ParadigmKind,
    },
    /// A tool finished inside the sub-agent. `snapshot` is a short result
    /// preview (truncated) — not the full output.
    ToolResult { tool_name: String, snapshot: String },
    /// Token usage after a sub-agent inference.
    TokenUsage { prompt: u32, completion: u32 },
    /// The sub-agent was cancelled (parent interrupt propagated).
    Cancelled,
}

/// Build a `completed:false` [`SubAgentSummary`] for a sub-agent that failed
/// or was interrupted, so a dependent task in the same batch can still proceed
/// (prefixed with this note) instead of tripping the cycle guard. Used by
/// [`AgentLoop::spawn_sub_agents_batch`] for partial-failure handling (Opt 1).
pub(crate) fn failure_summary(
    kind: crate::sub_agent::SubAgentKind,
    note: &str,
) -> crate::sub_agent::SubAgentSummary {
    crate::sub_agent::SubAgentSummary {
        completed: false,
        summary: note.to_string(),
        key_findings: Vec::new(),
        budget_exceeded: false,
        agent_kind: kind,
        tokens_used: 0,
    }
}

// ─── DelegationPolicy ────────────────────────────────────────────────────────

/// Resource bounds for delegated sub-agents (Opt 2 resource guardrail).
/// Defaults: `max_concurrent=2` (background sub-agents share the parent's
/// provider — too many concurrent streams starve each other and every
/// inference crawls; 2 keeps the parent responsive while still parallelizing),
/// `max_depth=1` (sub-agents can't nest), `budget_pool=None`.
#[derive(Clone)]
pub struct DelegationPolicy {
    /// Max sub-agents running LLM inference concurrently within a single
    /// delegate batch wave. A semaphore gates `spawn_sub_agents_batch` /
    /// `AsyncTaskRunner`. The parent turn also hits the same provider, so the
    /// effective concurrent-stream ceiling is `max_concurrent + 1` — keep this
    /// low to avoid provider starvation (each stream slow + contended).
    pub max_concurrent: usize,
    /// Max delegation depth. `1` = sub-agents can't spawn further
    /// sub-agents (today's behavior). `>1` unlocks nested delegation; the
    /// `DepthLimitedSubAgentFactory` chain refuses beyond this.
    pub max_depth: usize,
    /// Optional shared token budget across ALL sub-agents in a run. When
    /// exhausted, remaining wave tasks short-circuit with a budget-exceeded
    /// summary. `None` = no global pool (each task's `budget` cap governs).
    pub budget_pool: Option<oneai_core::budget::TokenBudget>,
}

impl Default for DelegationPolicy {
    fn default() -> Self {
        Self {
            max_concurrent: 2,
            max_depth: 1,
            budget_pool: None,
        }
    }
}

// ─── ForwardingObserver ──────────────────────────────────────────────────────

/// Owned, `'static` observer that a spawned sub-agent task uses to forward
/// its high-signal progress events back to the parent. Two transport modes:
///
/// - **Background (`bus` = Some):** emit `EngineYield::DelegateProgress`
///   DIRECTLY to the engine bus (sync `emit`) — so progress keeps flowing to
///   the frontend EVEN AFTER the parent turn has ended (the parent loop's
///   `drain_progress` only runs during the parent's own iterations; once the
///   parent yields with a `DirectAnswer`, draining stops and a long-running
///   background sub-agent would otherwise appear "stuck" with no live
///   status). The bus is `Arc`-cloned into the spawned task, so it outlives
///   the per-turn runner.
/// - **Foreground batch (`bus` = None):** send to an mpsc channel the parent
///   drains while it `await`s the batch (the parent is blocked on the wave,
///   so per-iteration draining suffices).
///
/// Only `on_iteration_start` / `on_tool_result` / `on_token_usage_full` are
/// forwarded — the high-signal events a parent UI cares about. Per-token
/// streams stay inside the sub-agent to avoid flooding.
pub(crate) struct ForwardingObserver {
    pub(crate) delegate_id: String,
    pub(crate) kind: crate::sub_agent::SubAgentKind,
    pub(crate) turn_id: String,
    pub(crate) bus: Option<Arc<dyn oneai_bus::EngineBus>>,
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<(
        String,
        crate::sub_agent::SubAgentKind,
        DelegateProgressEvent,
    )>,
}

impl ForwardingObserver {
    pub(crate) fn new(
        delegate_id: String,
        kind: crate::sub_agent::SubAgentKind,
        tx: tokio::sync::mpsc::UnboundedSender<(
            String,
            crate::sub_agent::SubAgentKind,
            DelegateProgressEvent,
        )>,
        turn_id: String,
        bus: Option<Arc<dyn oneai_bus::EngineBus>>,
    ) -> Self {
        Self {
            delegate_id,
            kind,
            turn_id,
            bus,
            tx,
        }
    }

    /// Forward a progress event: direct bus emit (background mode) or channel
    /// send (foreground batch). Direct emit keeps progress live after the
    /// parent turn ends; the channel is left unused in background mode so
    /// there's no double-delivery.
    fn forward(&self, event: DelegateProgressEvent) {
        if let Some(bus) = &self.bus {
            let _ = bus.emit(oneai_bus::EngineYield::DelegateProgress {
                turn_id: self.turn_id.clone(),
                task_id: self.delegate_id.clone(),
                agent_kind: oneai_bus::BusSubAgentKind::from(&self.kind),
                event: oneai_bus::BusDelegateProgress::from(&event),
            });
        } else {
            let _ = self
                .tx
                .send((self.delegate_id.clone(), self.kind.clone(), event));
        }
    }
}

impl AgentLoopObserver for ForwardingObserver {
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind) {
        self.forward(DelegateProgressEvent::IterationStart {
            iteration,
            paradigm,
        });
    }

    fn on_direct_answer(&self, _: &str) {}
    fn on_tool_calls(&self, _: &[ToolCallRequest]) {}
    fn on_tool_result(&self, _call_id: &str, tool_name: &str, _output: &oneai_core::ToolOutput) {
        // Forward just the tool name + an empty snapshot to avoid pulling the
        // full ToolOutput shape across the channel; the parent UI only needs
        // "the sub-agent ran tool X" as a liveness signal.
        self.forward(DelegateProgressEvent::ToolResult {
            tool_name: tool_name.to_string(),
            snapshot: String::new(),
        });
    }

    fn on_token_usage_full(
        &self,
        prompt_tokens: u32,
        completion_tokens: u32,
        _cache_read_tokens: u32,
        _cache_creation_tokens: u32,
    ) {
        self.forward(DelegateProgressEvent::TokenUsage {
            prompt: prompt_tokens,
            completion: completion_tokens,
        });
    }

    fn on_delegate(&self, _: &str, _: &str, _: &crate::sub_agent::SubAgentKind) {}
    fn on_paradigm_switch(&self, _: ParadigmKind) {}
    fn on_checkpoint(&self, _: usize) {}
    fn on_complete(&self, _: &AgentLoopResult) {}
}

// ─── DelegateTask / AgentDecision ────────────────────────────────────────────

///
/// A single turn may contain several `delegate` calls — the model fans them
/// out by emitting multiple `delegate` blocks in one inference response, and
/// `parse_decision` collects them into an `AgentDecision::Delegate` batch.
/// `id` + `depends_on` let the model express a DAG: tasks with no
/// `depends_on` run in parallel; a task whose `depends_on` ids have not yet
/// completed waits for them, and its `task` text is automatically prefixed
/// with their summaries before the sub-agent starts (so dependent sub-agents
/// receive upstream results without the model re-stating them).
#[derive(Debug, Clone)]
pub struct DelegateTask {
    /// Stable identifier for dependency resolution. When the model omits it,
    /// `parse_decision` falls back to the tool-call id (e.g. `call_abc123`)
    /// so every delegation has a usable key.
    pub id: String,
    /// The self-contained subtask. For dependent tasks this is the *original*
    /// text — the scheduler prepends dependency summaries at run time.
    pub task: String,
    /// The specialized sub-agent kind to spawn.
    pub agent_type: SubAgentKind,
    /// Token budget cap for this sub-agent.
    pub budget: oneai_core::budget::TokenBudget,
    /// Ids of delegations in the same batch that must complete first.
    /// References to unknown ids are dropped (with a warning) at parse time.
    pub depends_on: Vec<String>,
    /// The actual tool-call id (e.g. `call_abc123`) of the `delegate`
    /// ContentBlock — used to feed back a synthetic `tool_result` so the
    /// frontend's tool-call card (created from the streaming `on_tool_calls`
    /// the parser fires for every tool call, including the intercepted
    /// `delegate` meta-tool) resolves to "done" instead of staying
    /// "running" forever. Distinct from `id` (the model's own dependency
    /// id, which may be a semantic name like "explore-reggae").
    pub call_id: String,
    /// Opt 3: name for a `Custom` kind (ignored for fixed kinds).
    pub custom_role: Option<String>,
    /// Opt 3: override the kind's default system prompt (role layering).
    pub system_prompt_override: Option<String>,
    /// Opt 3: narrow the sub-agent's toolset (intersected with the kind
    /// default — never widened) below the kind default.
    pub tools_override: Option<Vec<String>>,
    /// Opt 4: whether to seed the sub-agent with the parent's recent turns.
    pub inherit_context: bool,
    /// Opt 4: how many of the parent's trailing non-system messages to seed.
    /// `0` with `inherit_context=true` defaults to 6 at materialization.
    pub inherit_last_n: usize,
    /// Opt 4 (materialized by the Delegate handler, not parse_decision):
    /// the actual seed messages snapshot, set from the parent conversation
    /// before the batch runs. `None` until the handler fills it.
    pub seed_messages: Option<Vec<oneai_core::Message>>,
}

/// The decision type produced by parsing the model's output each loop iteration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AgentDecision {
    /// The model produced a final answer — no tool calls, no delegation.
    DirectAnswer { text: String },

    /// The model wants to invoke one or more tools.
    ToolCalls { calls: Vec<ToolCallRequest> },

    /// The model wants to delegate one or more subtasks to specialized
    /// sub-agents. All `delegate` calls in the turn are collected here as a
    /// batch; the scheduler runs independent tasks in parallel and honors
    /// `depends_on` ordering (see [`DelegateTask`]).
    Delegate { tasks: Vec<DelegateTask> },

    /// The model wants to delegate one or more subtasks to **background**
    /// sub-agents (Phase 2A `delegate_background`). Unlike [`Delegate`], the
    /// loop does not wait — each task is submitted to the `AsyncTaskRunner`
    /// and the loop continues immediately. When a background sub-agent
    /// finishes, its result is injected back into the parent conversation and
    /// a new parent turn is triggered (fire-and-auto-notify — see
    /// [`crate::async_task_runner::AsyncTaskRunner`]).
    DelegateBackground { tasks: Vec<DelegateTask> },

    /// The model wants to switch to a different paradigm.
    SwitchParadigm { paradigm: ParadigmKind },

    /// The model wants to re-bind the project context to a different project
    /// root (the `switch_project` meta-tool). `call_id` is the tool-call id so
    /// the loop can feed back a `tool_result` confirmation; `dir` is the target
    /// project root. The next iteration injects the new project's context.
    SwitchProject { call_id: String, dir: PathBuf },
}

// ─── ParadigmKind ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParadigmKind {
    Plan,
    ReAct,
    Reflect,
    Explore,
}

// ─── ParadigmConfig ──────────────────────────────────────────────────────────

/// Configuration for a specific paradigm — defines how the agent behaves
/// when this paradigm is active.
///
/// Each paradigm changes three things:
/// 1. **System prompt**: A paradigm-specific prompt that defines the agent's
///    role and behavioral constraints for this mode.
/// 2. **Tool filter**: The set of tools available in this paradigm.
///    Plan paradigm doesn't need execution tools; Explore doesn't need edit tools.
/// 3. **Decision hint**: A brief description injected into context telling the
///    model what kind of decisions to make in this paradigm.
///
/// This addresses the "范式切换语义空洞" gap — previously, `run_paradigm()`
/// just returned a text string like "Plan paradigm activated" without any
/// actual behavioral change. Now, paradigm switching produces real, observable
/// effects: system prompt changes, tool filtering, and decision guidance.
///
/// Inspired by Aider's Architect/Editor dual-model pattern where each "role"
/// has its own prompt and tool set. OneAI extends this to 4 paradigms.
#[derive(Debug, Clone)]
pub struct ParadigmConfig {
    /// The paradigm this config applies to.
    pub paradigm: ParadigmKind,

    /// System prompt for this paradigm — replaces the default system prompt
    /// when this paradigm is active.
    pub system_prompt: String,

    /// Tools available in this paradigm — only these tools are sent to the
    /// model as tool definitions. Other tools are hidden from the model.
    pub tool_filter: Vec<String>,

    /// Decision hint — injected into context as a system message when
    /// this paradigm becomes active. Tells the model what kind of
    /// decisions to make (plan vs execute vs review vs explore).
    pub decision_hint: String,
}

impl ParadigmConfig {
    /// Get the default configuration for each paradigm kind.
    ///
    /// These defaults are modeled after Aider's Architect/Editor pattern:
    /// - Plan: No execution tools, focus on decomposition
    /// - ReAct: Full tool set, focus on action
    /// - Reflect: Read-only tools, focus on review
    /// - Explore: Read + search tools, focus on discovery
    pub fn defaults() -> Vec<ParadigmConfig> {
        vec![
            ParadigmConfig {
                paradigm: ParadigmKind::Plan,
                system_prompt: "You are a planning agent. Your ONLY job is to decompose the given \
                    task into a structured plan with ordered steps and dependencies. \
                    Do NOT execute any tools — produce only a plan as a numbered list. \
                    Each step should be specific, actionable, and identify which tool would be needed. \
                    Focus on: understanding the task scope, identifying dependencies, \
                    ordering steps logically, and flagging risks or unknowns."
                    .to_string(),
                tool_filter: vec![
                    "read_file".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "environment".into(),
                ],
                decision_hint: "You are in PLAN mode — focus on decomposing the task into ordered steps. \
                    Do NOT execute any tools. Produce only a plan.".to_string(),
            },
            ParadigmConfig {
                paradigm: ParadigmKind::ReAct,
                system_prompt: "You are a ReAct agent — you reason about what to do, then act using \
                    available tools, observe the results, and iterate. This is the default \
                    execution mode. Use tools to accomplish the task, and report the final \
                    answer when done. If you encounter errors, try to fix them in subsequent iterations. \
                    Focus on: executing actions efficiently, verifying results, and iterating until complete."
                    .to_string(),
                tool_filter: vec![
                    "read_file".into(), "edit_file".into(), "apply_patch".into(),
                    "shell".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "environment".into(),
                    "web_fetch".into(), "notebook_edit".into(),
                ],
                decision_hint: "You are in REACT mode — reason about what to do, then act using tools, \
                    observe results, and iterate.".to_string(),
            },
            ParadigmConfig {
                paradigm: ParadigmKind::Reflect,
                system_prompt: "You are a reflection agent. Your job is to review the current state \
                    of work, identify errors, improvements, and missing steps. You have \
                    read-only access — you can examine files and search the codebase, but \
                    you cannot make changes. Your output should be a structured review \
                    with: (1) issues found, (2) improvements suggested, (3) next steps recommended. \
                    Focus on: correctness, completeness, quality, and potential risks."
                    .to_string(),
                tool_filter: vec![
                    "read_file".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "environment".into(),
                ],
                decision_hint: "You are in REFLECT mode — review the current state, identify errors \
                    and improvements, and suggest next steps.".to_string(),
            },
            ParadigmConfig {
                paradigm: ParadigmKind::Explore,
                system_prompt: "You are an exploration agent. Your job is to search and understand \
                    the codebase/environment. You can read files, search patterns, and list \
                    directories, but you cannot modify anything. Return a comprehensive \
                    summary of your findings including: file paths, function signatures, \
                    key patterns, relevant dependencies, and any important observations. \
                    Focus on: thoroughness, accuracy, and providing useful context for \
                    subsequent planning or execution."
                    .to_string(),
                tool_filter: vec![
                    "read_file".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "environment".into(),
                    "web_fetch".into(),
                ],
                decision_hint: "You are in EXPLORE mode — search and understand the environment. \
                    Report findings without modifying anything.".to_string(),
            },
        ]
    }

    /// Get the ParadigmConfig for a specific paradigm kind from the defaults.
    pub fn for_paradigm(kind: ParadigmKind) -> ParadigmConfig {
        Self::defaults()
            .into_iter()
            .find(|c| c.paradigm == kind)
            .unwrap_or_else(|| ParadigmConfig {
                paradigm: kind,
                system_prompt: String::new(),
                tool_filter: vec![], // Empty filter = all tools available
                decision_hint: String::new(),
            })
    }
}

// ─── ToolCallRequest / ToolCallResult ────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub call_id: String,
    /// The tool name — used by TUI observer to identify which tool produced this result,
    /// enabling it to find and update the corresponding ToolInvocation message.
    pub tool_name: String,
    pub output: ToolOutput,
}

// ─── LoopState ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LoopState {
    pub original_task: String,
    pub conversation: Conversation,
    pub global_state: oneai_core::GlobalState,
    pub iterations: usize,
    pub is_complete: bool,
    pub final_answer: Option<String>,
    pub active_skills: Vec<oneai_core::SkillDescriptor>,
    pub active_paradigm: ParadigmKind,
    /// The active paradigm configuration — determines system prompt,
    /// tool filter, and decision hint for the current paradigm.
    /// Updated when paradigm switching occurs.
    pub active_paradigm_config: Option<ParadigmConfig>,
    pub sub_agent_results: Vec<SubAgentSummary>,
    /// Interrupt points accumulated during the loop.
    /// Each interrupt represents a pause point where human feedback was requested.
    pub interrupt_points: Vec<InterruptPoint>,
    /// The current pending interrupt (if the loop is paused).
    /// When set, the loop will break at the next iteration boundary.
    pub pending_interrupt: Option<InterruptPoint>,
    /// Live plan state — the task list the model mutates via the `task_*`
    /// control tools. None until a plan is created. Persists across iterations
    /// and interrupts (agent-side state, not model output).
    pub plan_state: Option<crate::plan_state::PlanState>,
    /// The durable working-state projection (goal/steps/decisions/blockers/
    /// notes), derived from the cross-session event log held by
    /// `WorkingStateStore`. None when no store is configured OR no task has
    /// been bound yet. The pinned `[Task Anchor]` / `[Plan & Progress]` /
    /// `[Decisions]` / `[Blockers]` blocks read this in-memory projection
    /// every turn (zero IO) — the durable source of truth is the event log,
    /// not the conversation transcript.
    pub working_state: Option<oneai_core::WorkingState>,
    /// The task id bound to this loop run — set when a `WorkingStateStore` is
    /// configured and `exit_plan_mode` (or a cross-session `continue`) creates
    /// / binds a task. Carried in `conversation.metadata["task_id"]` so a
    /// same-session resume can re-bind the working state.
    pub task_id: Option<String>,
    /// The session id this run belongs to (for working-state event audit).
    /// Empty until the AppSession sets it; events still persist when empty.
    pub session_id: String,
    /// The owning user (cross-session namespace) for working-state events.
    pub user_id: String,
    /// The project / cwd scope for working-state events.
    pub project: String,
    /// Telemetry: how many cadence-fired `Reflect` sub-agents have run this
    /// loop. Prevents re-firing on the same boundary after a retry that
    /// didn't advance `iterations`.
    pub reflections_fired: usize,
    /// Cumulative iteration count at the last `ReflectionFired` event,
    /// hydrated from the working-state event log on resume (Phase 2.1
    /// Stage C). The cadence check fires on `cadence_baseline + iterations`
    /// so a task resumed cross-session continues from the prior session's
    /// last fire boundary instead of re-firing from zero. 0 when no store
    /// is configured or the task has no prior reflections.
    pub cadence_baseline: u64,
    /// Snapshot of the active (Footprint-gate `service_available()==true`)
    /// tool names after the last tool batch — the baseline for the
    /// self-extension diff (evolution-plan §3.4). After each tool batch the
    /// loop recomputes the active set and surfaces any names newly present
    /// vs. this baseline (mid-turn registrations / gate flips), unioned with
    /// the tools each result self-reported via `ToolOutput::added_tool_names`.
    /// The diff is authoritative (catches registrations that didn't
    /// self-report); the field is the explicit signal. `None` until the first
    /// batch establishes the baseline (so the initial toolset isn't reported
    /// as "newly added").
    pub prev_active_tool_names: Option<std::collections::HashSet<String>>,
    /// A one-shot system note listing tools that became available this turn
    /// (self-extension), consumed by `inject_pinned_blocks` on the next
    /// context assembly. `Some(names)` when a tool batch surfaced new tools,
    /// cleared after injection so the nudge doesn't repeat.
    pub pending_new_tools_note: Option<Vec<String>>,
}

impl LoopState {
    pub fn new(task: &str) -> Self {
        let mut conversation = Conversation::new();
        // Mirror the original task into conversation metadata so every
        // compressor (which copies metadata verbatim) preserves the task
        // anchor even if the first user message itself gets summarized away.
        conversation
            .metadata
            .insert("task_anchor".to_string(), task.to_string());
        conversation.add_message(Message::user(task.to_string()));
        Self {
            original_task: task.to_string(),
            conversation,
            global_state: oneai_core::GlobalState::new(),
            iterations: 0,
            is_complete: false,
            final_answer: None,
            active_skills: Vec::new(),
            active_paradigm: ParadigmKind::ReAct,
            active_paradigm_config: None, // Uses default system prompt until switch
            sub_agent_results: Vec::new(),
            interrupt_points: Vec::new(),
            pending_interrupt: None,
            plan_state: None,
            working_state: None,
            task_id: None,
            session_id: String::new(),
            user_id: String::new(),
            project: String::new(),
            reflections_fired: 0,
            cadence_baseline: 0,
            prev_active_tool_names: None,
            pending_new_tools_note: None,
        }
    }

    /// Create a LoopState from an existing conversation, adding a new user message.
    ///
    /// This preserves prior conversation history (multi-turn context)
    /// while appending the new user input as the latest message.
    pub fn from_conversation(conversation: Conversation, task: &str) -> Self {
        let mut conv = conversation;
        conv.metadata
            .insert("task_anchor".to_string(), task.to_string());
        conv.add_message(Message::user(task.to_string()));
        // Q3 reseed: restore the live plan list from metadata so a reloaded /
        // compacted session continues the in-flight task instead of losing it.
        let plan_state = conv
            .metadata
            .get("plan_state")
            .and_then(|s| crate::plan_state::PlanState::from_metadata_string(s));
        // The durable working state is NOT rehydrated from metadata here — it
        // lives in the cross-session event log (WorkingStateStore), read on
        // session start / resume via the store. `task_id` (a pointer) is the
        // only thing carried in metadata; the caller rehydrates `working_state`
        // from the store using it. `original_task` keeps the new user message
        // for the durable log; the pinned `[Task Anchor]` prefers the working
        // state's `goal` when available (the canonical original goal).
        let task_id = conv
            .metadata
            .get("task_id")
            .cloned()
            .filter(|s| !s.is_empty());
        Self {
            original_task: task.to_string(),
            conversation: conv,
            global_state: oneai_core::GlobalState::new(),
            iterations: 0,
            is_complete: false,
            final_answer: None,
            active_skills: Vec::new(),
            active_paradigm: ParadigmKind::ReAct,
            active_paradigm_config: None,
            sub_agent_results: Vec::new(),
            interrupt_points: Vec::new(),
            pending_interrupt: None,
            plan_state,
            working_state: None,
            task_id,
            session_id: String::new(),
            user_id: String::new(),
            project: String::new(),
            reflections_fired: 0,
            cadence_baseline: 0,
            prev_active_tool_names: None,
            pending_new_tools_note: None,
        }
    }

    pub fn set_final_answer(&mut self, text: String) {
        self.final_answer = Some(text);
        self.is_complete = true;
    }

    pub fn mark_complete(&mut self) {
        self.is_complete = true;
    }
    pub fn is_complete(&self) -> bool {
        self.is_complete
    }

    pub fn feed_tool_results(&mut self, results: Vec<ToolCallResult>) {
        for result in results {
            let content = if result.output.success {
                if result.output.content.is_empty() {
                    // Provide a meaningful default message for successful tools with no output
                    // (e.g., mkdir, file write, etc. — commands that succeed silently)
                    "Tool executed successfully (no output).".to_string()
                } else {
                    result.output.content.clone()
                }
            } else {
                format!(
                    "Error: {}",
                    result.output.error.as_deref().unwrap_or("Unknown error")
                )
            };
            self.conversation
                .add_message(Message::tool_result(result.call_id.clone(), content));
        }
    }

    pub fn feed_sub_agent_result(&mut self, summary: SubAgentSummary) {
        self.sub_agent_results.push(summary.clone());
        self.conversation.add_message(Message::assistant(format!(
            "[Sub-agent result]: {} {}",
            summary.summary,
            if summary.key_findings.is_empty() {
                String::new()
            } else {
                format!("\nKey findings: {}", summary.key_findings.join("; "))
            }
        )));
    }

    pub fn feed_paradigm_result(&mut self, paradigm: ParadigmKind, result_text: String) {
        self.conversation.add_message(Message::assistant(format!(
            "[{} paradigm result]: {}",
            paradigm_name(&paradigm),
            result_text
        )));
    }

    pub fn into_result(self) -> AgentLoopResult {
        AgentLoopResult {
            conversation: self.conversation,
            final_answer: self.final_answer.unwrap_or_default(),
            global_state: self.global_state,
            iterations: self.iterations,
            completed: self.is_complete,
            active_paradigm: self.active_paradigm,
            sub_agent_results: self.sub_agent_results,
        }
    }
}

fn paradigm_name(kind: &ParadigmKind) -> &'static str {
    match kind {
        ParadigmKind::Plan => "Plan",
        ParadigmKind::ReAct => "ReAct",
        ParadigmKind::Reflect => "Reflect",
        ParadigmKind::Explore => "Explore",
    }
}

/// Inverse of [`paradigm_name`] for the lowercase form stored in
/// `conversation.metadata["active_paradigm"]` by `Directive::SwitchParadigm`.
/// Returns `None` for an unknown string so a stale/corrupt metadata value from
/// an older session degrades to the loop's default (ReAct) rather than
/// panicking.
fn paradigm_from_metadata(s: &str) -> Option<ParadigmKind> {
    match s {
        "plan" => Some(ParadigmKind::Plan),
        "react" => Some(ParadigmKind::ReAct),
        "reflect" => Some(ParadigmKind::Reflect),
        "explore" => Some(ParadigmKind::Explore),
        _ => None,
    }
}

/// Build a safe fallback `InferenceResponse` for when the PostInfer interaction
/// gate aborts a response (e.g. the layer flagged disallowed content). The
/// fallback is a benign assistant message carrying the abort reason.
fn safe_fallback_response(reason: &str) -> InferenceResponse {
    InferenceResponse {
        message: Message::assistant(format!("(response replaced by PostInfer gate: {})", reason)),
        usage: oneai_core::TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            ..Default::default()
        },
        model: String::new(),
        metadata: HashMap::new(),
    }
}

// ─── AgentLoopResult ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AgentLoopResult {
    pub conversation: Conversation,
    pub final_answer: String,
    pub global_state: oneai_core::GlobalState,
    pub iterations: usize,
    pub completed: bool,
    pub active_paradigm: ParadigmKind,
    pub sub_agent_results: Vec<SubAgentSummary>,
}

// ─── AgentLoopConfig ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub system_prompt: String,
    pub use_streaming: bool,
    pub temperature: Option<f32>,
    /// Top-p (nucleus) sampling mass. `None` lets the provider use its own
    /// default (1.0 = no nucleus filtering, the safe baseline).
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Token budget for extended thinking/reasoning (Anthropic budget_tokens, etc).
    /// `None` = thinking **disabled** (the default — thinking is opt-in, since it
    /// is Anthropic-specific, costs tokens, and silently inflates `max_tokens`).
    /// `Some(N)` = enable thinking with an N-token budget.
    pub thinking_budget: Option<u32>,
    /// Stop sequences — generation halts when any is emitted.
    pub stop_sequences: Vec<String>,
    pub hard_max_iterations: Option<usize>,
    /// Run-cost token budget — a cumulative cap on total tokens consumed
    /// (prompt + completion) across the whole run. When set, the loop
    /// terminates on exhaustion (see `run_loop`'s while condition) in
    /// ADDITION to `hard_max_iterations`. `None` = no cost cap (only the
    /// iteration limit guards against runaway). This makes the documented
    /// "iteration limit is governed by TokenBudget" claim actually true —
    /// previously `TokenBudget` existed but was never checked or consumed,
    /// so a runaway model burned tokens indefinitely up to
    /// `hard_max_iterations`.
    pub token_budget: Option<oneai_core::budget::TokenBudget>,
    pub inject_skills: bool,
    /// Usage tracker — records token usage after each inference call.
    /// When set, the loop automatically records token usage (no USD cost).
    pub usage_tracker: Option<Arc<dyn oneai_core::UsageTracker>>,
    /// Rate limiter — checks rate before each provider call.
    /// When set, the loop waits if the rate limit is exceeded.
    pub rate_limiter: Option<Arc<dyn oneai_core::RateLimiter>>,
    /// Circuit breaker — provider failover on repeated failures.
    /// When set, the loop skips calls to providers with open circuits.
    pub circuit_breaker: Option<Arc<dyn oneai_core::CircuitBreaker>>,
    /// Token counter — client-side token estimation. When the provider returns
    /// no usage in its (streaming) response, the loop falls back to counting
    /// tokens locally with this counter so the usage axis (tokens) isn't
    /// silently zero. Matches the litellm/aider pattern: API usage authoritative
    /// when present, client-side estimate otherwise.
    pub token_counter: Option<Arc<dyn oneai_core::TokenCounter>>,
    /// Model-aware context manager — when set, the loop checks each
    /// inference request against the target model's context window and, if
    /// it doesn't fit, applies the configured trimming strategy
    /// (`TruncateOldest` / `ImportanceRanked` / `CompressMiddle` /
    /// `SmartSummary`) to the per-request conversation. This makes the
    /// 4-strategy `ContextManager` reachable on the hot path — previously it
    /// was constructed by `AppBuilder` but never invoked, so only the
    /// `ContextCompressor` keep-recent path ran (gap-analysis #3).
    /// Trimming is applied to the per-request `conv_for_inference` clone,
    /// not the durable log, so it is lossy only for the request (the full
    /// durable log persists for later turns / replay).
    pub context_manager: Option<Arc<oneai_core::ContextManager>>,
    /// Structured output configuration — when set, the model's final
    /// answer is validated against a JSON Schema. If validation fails,
    /// the model is re-prompted with the error for self-correction (ModelRetry).
    pub structured_output: Option<StructuredOutputConfig>,
    /// Policy for attaching `ConstrainedOutputConfig` (Layer-1 constrained
    /// decoding) to inference requests, derived from `structured_output.schema`.
    /// Tier-gated: constrained decoding helps local/small models but hurts
    /// cloud SOTA reasoning quality. Post-hoc validation + ModelRetry run
    /// regardless of this policy. Default `Auto` trusts the provider's
    /// `prefers_constrained_output()` recommendation.
    pub constrained_output_policy: oneai_core::ConstrainedOutputPolicy,
    /// Trace context for observability — when set, spans and events are
    /// emitted at key lifecycle points (iteration, inference, tool call,
    /// paradigm switch, delegation, approval). When None, tracing is
    /// completely disabled (zero overhead).
    pub trace_context: Option<TraceContext>,
    /// OTEL metrics provider — when set (and the `otel` feature is on), the
    /// loop records real counters/histograms at the lifecycle hot paths:
    /// inference requests + token usage, tool-call success/failure, errors.
    /// Without the feature or when None, this is zero-cost (no field, no code).
    #[cfg(feature = "otel")]
    pub metrics_provider: Option<std::sync::Arc<OtelMetricsProvider>>,
    /// Plan mode — when true, tool execution is blocked entirely. Instead of
    /// running tools, the loop injects a synthetic tool result telling the
    /// model it must produce a step-by-step plan rather than executing. This
    /// mirrors Claude Code's plan mode (read-only research/planning).
    pub plan_mode: bool,
    /// Policy for provider-side prompt caching (Anthropic `cache_control` on
    /// the stable system+tools prefix). Default `Auto` = caching on. Set `Off`
    /// for A/B replay to measure the no-cache baseline (efficiency axis:
    /// `EfficiencyProfile.cache_read_tokens` / `cache_hit_ratio`).
    pub prompt_cache_policy: oneai_core::PromptCachePolicy,
    /// Cadence for the background `Reflect` sub-agent (Phase 2.1 Stage A).
    /// `None` (default) = reflect never fires (backward-compat). `Some(n)`
    /// = fire a reflect sub-agent every `n` iterations (mid-run cadence) AND
    /// once on `DirectAnswer` delivery, when not interrupted. The reflect
    /// sub-agent inherits the parent provider, runs a Hermes-style review
    /// prompt, and persists durable learnings to memory; its summary is
    /// surfaced via `AgentLoopObserver::on_reflection` and is NOT injected
    /// into the parent conversation.
    pub reflection_cadence: Option<usize>,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("system_prompt", &self.system_prompt)
            .field("use_streaming", &self.use_streaming)
            .field("temperature", &self.temperature)
            .field("top_p", &self.top_p)
            .field("max_tokens", &self.max_tokens)
            .field("thinking_budget", &self.thinking_budget)
            .field("stop_sequences", &self.stop_sequences)
            .field("hard_max_iterations", &self.hard_max_iterations)
            .field("token_budget", &self.token_budget)
            .field("inject_skills", &self.inject_skills)
            .field(
                "usage_tracker",
                &self.usage_tracker.as_ref().map(|_| "Arc<dyn UsageTracker>"),
            )
            .field(
                "rate_limiter",
                &self.rate_limiter.as_ref().map(|_| "Arc<dyn RateLimiter>"),
            )
            .field(
                "circuit_breaker",
                &self
                    .circuit_breaker
                    .as_ref()
                    .map(|_| "Arc<dyn CircuitBreaker>"),
            )
            .field(
                "token_counter",
                &self.token_counter.as_ref().map(|_| "Arc<dyn TokenCounter>"),
            )
            .field(
                "context_manager",
                &self.context_manager.as_ref().map(|_| "Arc<ContextManager>"),
            )
            .field("structured_output", &self.structured_output)
            .field("trace_context", &self.trace_context)
            // metrics_provider (otel) holds atomics and is not Debug-rendered;
            // the manual impl may omit fields, so it stays absent here.
            .field("plan_mode", &self.plan_mode)
            .field("reflection_cadence", &self.reflection_cadence)
            .finish()
    }
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            system_prompt: "You are an intelligent AI agent that can plan, execute, and reflect on tasks. \
                When you need to use a tool, output a tool call. When you have the final answer, \
                respond with just text without any tool calls.\n\n\
                **Model-driven control tools** (call these instead of plain tools when appropriate):\n\
                - `delegate(task, agent_type, budget_tokens?)`: hand a self-contained subtask to a \
                specialized sub-agent that runs in its own context window and returns a summary. \
                `agent_type` is one of \"Plan\", \"Explore\", \"Code\", \"Review\". Use it when the \
                subtask has a clear boundary and the main loop should not be cluttered with its \
                intermediate steps. After calling `delegate`, the main loop waits for the \
                sub-agent's summary — do not call other tools in the same turn.\n\
                - `switch_paradigm(paradigm)`: switch to a fixed graph flow. `paradigm` is one of \
                \"plan\", \"react\", \"reflect\", \"explore\". Use \"plan\" for structured \
                decomposition, \"reflect\" to deeply review the last result, \"explore\" for \
                breadth-first search, \"react\" to return to the standard reason-then-act loop. \
                After calling, execution continues inside that paradigm's graph and the result is \
                fed back to you.\n\
                - `enter_plan_mode(plan?)`: escalate from normal execution into plan mode. Call \
                this ONLY when the task is genuinely complex and needs step-by-step decomposition \
                — NOT for simple one-shot tasks, which you should just do directly with execution \
                tools. After calling, you are switched into the plan toolset (task_create / \
                exit_plan_mode) so you can commit a plan for approval. Avoid calling it for trivia.\n\
                (Sub-agent kinds mirror the configured SubAgentTypeDefinitions; see the domain pack.)\n\n\
                {{TOOL_PREFERENCE_RULES}}"
                .to_string(),
            use_streaming: false,
            temperature: None,
            top_p: None,
            max_tokens: None,
            // Thinking is opt-in: it is Anthropic-specific, costs tokens, and
            // silently inflates max_tokens. Enable via GenerationConfig /
            // AgentLoopConfig::thinking_budget when wanted.
            thinking_budget: None,
            stop_sequences: Vec::new(),
            hard_max_iterations: Some(200), // Safety guard: None = only budget constraint, Some(N) = budget + iteration limit
            // No run-cost token cap by default (opt-in). AppSession wires a
            // conservative runaway guardrail when building the loop.
            token_budget: None,
            inject_skills: true,
            usage_tracker: None,
            rate_limiter: None,
            circuit_breaker: None,
            token_counter: None,
            context_manager: None,
            structured_output: None,
            constrained_output_policy: oneai_core::ConstrainedOutputPolicy::Auto,
            trace_context: None,
            // OTEL metrics are opt-in — AppBuilder wires the provider when the
            // `otel` feature is on and the user enables metrics.
            #[cfg(feature = "otel")]
            metrics_provider: None,
            plan_mode: false,
            prompt_cache_policy: oneai_core::PromptCachePolicy::Auto,
            reflection_cadence: None,
        }
    }
}

impl AgentLoopConfig {
    /// Apply user-configured [`GenerationConfig`] on top of this config.
    ///
    /// Each `Some` field in `cfg` overrides the corresponding field here;
    /// `None` fields are left untouched (so the scenario default set elsewhere
    /// — e.g. via `..AgentLoopConfig::default()` or a paradigm agent — wins).
    /// `stop_sequences` replaces when non-empty.
    pub fn apply_generation_config(&mut self, cfg: &oneai_core::GenerationConfig) {
        if let Some(t) = cfg.temperature {
            self.temperature = Some(t);
        }
        if let Some(p) = cfg.top_p {
            self.top_p = Some(p);
        }
        if let Some(m) = cfg.max_tokens {
            self.max_tokens = Some(m);
        }
        // thinking_budget is authoritative even when None (user explicitly
        // disabling thinking), so assign directly rather than Option::or.
        self.thinking_budget = cfg.thinking_budget;
        if !cfg.stop_sequences.is_empty() {
            self.stop_sequences = cfg.stop_sequences.clone();
        }
    }
}

// ─── AgentLoop ──────────────────────────────────────────────────────────────

pub struct AgentLoop {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    parser: Arc<dyn OutputParser>,
    /// Unified interaction gate — single surface for every loop-suspend
    /// decision point (PreInfer/PostInfer/ToolApproval/PlanDecision/PlanReview).
    interaction_gate: Arc<dyn InteractionGate>,
    skill_selector: Arc<oneai_skill::SkillSelector>,
    /// Shared skill registry — used to inject the always-on skill menu (Tier1
    /// progressive disclosure) into the system prompt each turn. The `skill`
    /// tool reads the same registry to load a skill's full prompt on demand.
    skill_registry: Arc<oneai_skill::SkillRegistry>,
    /// Optional skill lifecycle metadata store (Phase 2.1 Stage B). When
    /// set, `build_skill_menu` hides `Archived` skills (retired = invisible
    /// to the model) and the `skill` tool bumps `use_count` on activation.
    /// `None` = legacy stateless skill behavior.
    skill_metadata_store: Option<Arc<oneai_skill::SkillMetadataStore>>,
    /// Manually-activated skill name (via `/skill <name>`). When set, the
    /// skill's full prompt_template is injected as a system message each turn.
    active_skill: Option<String>,
    context_budget: Arc<oneai_core::budget::ContextBudgetManager>,
    sub_agent_factory: Arc<dyn SubAgentFactory>,
    /// Optional async task runner for parallel sub-agent delegation.
    /// When enabled, the AgentLoop can submit sub-agent tasks to the
    /// runner for background execution, continuing work while sub-agents
    /// run independently. The runner uses the same SubAgentFactory as
    /// serial delegation, ensuring consistent sub-agent creation.
    /// If None, all delegation is serial (spawn_sub_agent → wait).
    async_task_runner: Option<Arc<crate::async_task_runner::AsyncTaskRunner>>,
    context_assembler: Arc<tokio::sync::RwLock<ContextAssembler>>,
    stream_parser: Arc<tokio::sync::RwLock<IncrementalStreamParser>>,
    /// Durable working-state store — the cross-session source of truth for
    /// goal/steps/decisions/blockers/notes, persisted as a per-task append-only
    /// event log independent of any session transcript. When `Some`, the loop
    /// appends a working-state event at every plan-control-tool mutation
    /// (exit_plan_mode / task_update / decision resolution) so progress
    /// survives crashes (§8.1) and is discoverable by a brand-new session
    /// (§6.2). The hot read path (pinned re-injection) uses the in-memory
    /// `LoopState.working_state` projection, not this store — zero IO per turn.
    working_state_store: Option<Arc<dyn oneai_core::traits::WorkingStateStore>>,
    /// Per-run working-state scope (user / project / session) — threaded into
    /// `LoopState` at the start of each run so working-state events are scoped
    /// to the right cross-session namespace. Set via `with_working_state_scope`.
    ws_user_id: String,
    ws_project: String,
    ws_session_id: String,
    recovery_manager: Option<Arc<crate::error_recovery::RecoveryManager>>,
    hook_registry: Arc<tokio::sync::RwLock<HookRegistry>>,
    interrupt_requested: Arc<AtomicBool>,
    interrupt_reason: Arc<tokio::sync::Mutex<Option<InterruptReason>>>,
    /// Cooperative cancellation token — fired by `request_interrupt` so an
    /// in-flight `provider.infer` / stream / tool execution aborts immediately
    /// (via `tokio::select!`) instead of waiting for the iteration boundary.
    cancel_token: CancellationToken,
    /// Live plan-mode flag (mirrors `config.plan_mode` at construction but is
    /// mutable mid-run so `exit_plan_mode` acceptance can flip it off).
    plan_mode_active: Arc<AtomicBool>,
    config: AgentLoopConfig,
    domain_pack: Option<Arc<MergedDomainPack>>,
    /// Inherited permission resolver for delegated sub-agents. When `Some`
    /// (set via `with_permission_pack` by `DefaultSubAgentFactory`, which
    /// threads the PARENT's domain pack here), the loop's tool-permission
    /// check consults this pack's `resolve_permission` — so a sub-agent
    /// inherits the parent's permission policy (e.g. the CodingPack
    /// auto-approves `web_search`/`web_fetch` → the Explore sub-agent's web
    /// calls don't prompt). INHERITANCE OF PERMISSION ONLY: exposure,
    /// paradigm strategies, context sources, and tool-def filtering still come
    /// from `domain_pack` (None for sub-agents → tool defaults: all Direct,
    /// no CodingPack context bloat). See `domain_permission_checks`.
    permission_pack: Option<Arc<MergedDomainPack>>,
    /// Opt 2 resource bounds for delegated sub-agents (concurrency cap,
    /// max nesting depth, optional global budget pool). Default preserves
    /// today's behavior. Set via [`AgentLoop::with_delegation_policy`].
    delegation_policy: DelegationPolicy,
}

/// Manual Clone implementation for AgentLoop — all fields are Arc/RwLock/Arc<RwLock>,
/// so cloning is cheap (just Arc pointer cloning, no data duplication).
///
/// This enables SubAgentWrapper to clone the AgentLoop for tokio::spawn,
/// allowing sub-agents to run on separate async tasks independently.
impl Clone for AgentLoop {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            parser: self.parser.clone(),
            interaction_gate: self.interaction_gate.clone(),
            skill_selector: self.skill_selector.clone(),
            skill_registry: self.skill_registry.clone(),
            skill_metadata_store: self.skill_metadata_store.clone(),
            active_skill: self.active_skill.clone(),
            context_budget: self.context_budget.clone(),
            sub_agent_factory: self.sub_agent_factory.clone(),
            async_task_runner: self.async_task_runner.clone(),
            context_assembler: self.context_assembler.clone(),
            stream_parser: self.stream_parser.clone(),
            working_state_store: self.working_state_store.clone(),
            ws_user_id: self.ws_user_id.clone(),
            ws_project: self.ws_project.clone(),
            ws_session_id: self.ws_session_id.clone(),
            recovery_manager: self.recovery_manager.clone(),
            hook_registry: self.hook_registry.clone(),
            interrupt_requested: self.interrupt_requested.clone(),
            interrupt_reason: self.interrupt_reason.clone(),
            cancel_token: self.cancel_token.clone(),
            plan_mode_active: self.plan_mode_active.clone(),
            config: self.config.clone(),
            domain_pack: self.domain_pack.clone(),
            permission_pack: self.permission_pack.clone(),
            delegation_policy: self.delegation_policy.clone(),
        }
    }
}

impl AgentLoop {
    /// Toggle plan mode at runtime (used by the `exit_plan_mode` gate).
    pub fn set_plan_mode(&self, on: bool) {
        self.plan_mode_active.store(on, Ordering::Relaxed);
    }

    /// Whether plan mode is currently active.
    fn plan_mode(&self) -> bool {
        self.plan_mode_active.load(Ordering::Relaxed)
    }

    /// Get the provider name for cost tracking.
    fn provider_name(&self) -> String {
        let config = self.provider.config();
        match config.cloud_kind {
            Some(oneai_core::CloudProviderKind::OpenAI) => "openai".to_string(),
            Some(oneai_core::CloudProviderKind::Anthropic) => "anthropic".to_string(),
            Some(oneai_core::CloudProviderKind::Gemini) => "gemini".to_string(),
            None => match config.provider_type {
                oneai_core::ProviderType::Local => "ollama".to_string(),
                oneai_core::ProviderType::Transformers => "local".to_string(),
                oneai_core::ProviderType::Cloud => "cloud".to_string(),
            },
        }
    }

    /// Create a new AgentLoop with all dependencies.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
        parser: Arc<dyn OutputParser>,
        interaction_gate: Arc<dyn InteractionGate>,
        skill_selector: Arc<oneai_skill::SkillSelector>,
        context_budget: Arc<oneai_core::budget::ContextBudgetManager>,
        sub_agent_factory: Arc<dyn SubAgentFactory>,
        context_assembler: ContextAssembler,
        stream_parser: IncrementalStreamParser,
        config: AgentLoopConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            parser,
            interaction_gate,
            skill_selector,
            context_budget,
            sub_agent_factory,
            async_task_runner: None,
            context_assembler: Arc::new(tokio::sync::RwLock::new(context_assembler)),
            stream_parser: Arc::new(tokio::sync::RwLock::new(stream_parser)),
            working_state_store: None,
            ws_user_id: String::new(),
            ws_project: String::new(),
            ws_session_id: String::new(),
            recovery_manager: None,
            hook_registry: Arc::new(tokio::sync::RwLock::new(HookRegistry::new())),
            interrupt_requested: Arc::new(AtomicBool::new(false)),
            interrupt_reason: Arc::new(tokio::sync::Mutex::new(None)),
            cancel_token: CancellationToken::new(),
            plan_mode_active: Arc::new(AtomicBool::new(config.plan_mode)),
            skill_registry: Arc::new(oneai_skill::SkillRegistry::new()),
            skill_metadata_store: None,
            active_skill: None,
            config,
            domain_pack: None,
            permission_pack: None,
            delegation_policy: DelegationPolicy::default(),
        }
    }

    /// Create a new AgentLoop with a domain pack and recovery manager.
    #[allow(clippy::too_many_arguments)]
    pub fn with_domain_pack(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
        parser: Arc<dyn OutputParser>,
        interaction_gate: Arc<dyn InteractionGate>,
        skill_selector: Arc<oneai_skill::SkillSelector>,
        context_budget: Arc<oneai_core::budget::ContextBudgetManager>,
        sub_agent_factory: Arc<dyn SubAgentFactory>,
        context_assembler: ContextAssembler,
        stream_parser: IncrementalStreamParser,
        config: AgentLoopConfig,
        domain_pack: Arc<MergedDomainPack>,
    ) -> Self {
        Self {
            provider,
            tools,
            parser,
            interaction_gate,
            skill_selector,
            context_budget,
            sub_agent_factory,
            async_task_runner: None,
            context_assembler: Arc::new(tokio::sync::RwLock::new(context_assembler)),
            stream_parser: Arc::new(tokio::sync::RwLock::new(stream_parser)),
            working_state_store: None,
            ws_user_id: String::new(),
            ws_project: String::new(),
            ws_session_id: String::new(),
            recovery_manager: None,
            hook_registry: Arc::new(tokio::sync::RwLock::new(HookRegistry::new())),
            interrupt_requested: Arc::new(AtomicBool::new(false)),
            interrupt_reason: Arc::new(tokio::sync::Mutex::new(None)),
            cancel_token: CancellationToken::new(),
            plan_mode_active: Arc::new(AtomicBool::new(config.plan_mode)),
            skill_registry: Arc::new(oneai_skill::SkillRegistry::new()),
            skill_metadata_store: None,
            active_skill: None,
            config,
            domain_pack: Some(domain_pack),
            permission_pack: None,
            delegation_policy: DelegationPolicy::default(),
        }
    }

    /// Set the delegation resource bounds (Opt 2): max concurrent sub-agents
    /// per wave, max nesting depth, and an optional global budget pool.
    /// Default (`max_concurrent=4`, `max_depth=1`, no pool) matches today's
    /// behavior. **Note**: raising `max_depth` only takes effect if the
    /// installed `sub_agent_factory` is a `DepthLimitedSubAgentFactory` chain
    /// built with that `max_depth` (the factory, not this policy, embeds the
    /// depth gate into spawned sub-agents).
    pub fn with_delegation_policy(self, policy: DelegationPolicy) -> Self {
        Self {
            delegation_policy: policy,
            ..self
        }
    }

    /// Inherit a parent's permission policy (permission-inheritance for
    /// delegated sub-agents). The loop's tool-permission check consults this
    /// pack's `resolve_permission` BEFORE falling back to its own
    /// `domain_pack`, so a sub-agent (whose `domain_pack` is None) inherits the
    /// parent's auto-approve / require-confirmation decisions — e.g. the
    /// CodingPack auto-approves `web_search`/`web_fetch`, so an Explore
    /// sub-agent's web calls don't prompt. Permission ONLY: this does not
    /// inherit exposure / context / paradigm (those stay None → tool defaults).
    pub fn with_permission_pack(self, pack: Arc<MergedDomainPack>) -> Self {
        Self {
            permission_pack: Some(pack),
            ..self
        }
    }

    /// Like [`Self::with_permission_pack`] but accepts `None` (no parent
    /// domain pack → no inheritance, sub-agents fall back to tool-level
    /// permission). Used by `DefaultSubAgentFactory::build`.
    pub fn with_optional_permission_pack(self, pack: Option<Arc<MergedDomainPack>>) -> Self {
        match pack {
            Some(p) => self.with_permission_pack(p),
            None => self,
        }
    }

    /// Attach the shared skill registry (for the always-on skill menu) and an
    /// optionally manually-activated skill whose prompt is injected each turn.
    /// Called by the session after construction so the giant `new`/`with_domain_pack`
    /// signatures don't need new positional params.
    pub fn with_skill_registry(
        mut self,
        registry: Arc<oneai_skill::SkillRegistry>,
        active_skill: Option<String>,
    ) -> Self {
        self.skill_registry = registry;
        self.active_skill = active_skill;
        self
    }

    /// Attach the skill lifecycle metadata store (Phase 2.1 Stage B). When
    /// set, the always-on skill menu hides `Archived` skills (retired =
    /// invisible to the model) so the curator's retirements take effect
    /// without a restart. The store should already be `load()`ed.
    pub fn with_skill_metadata_store(
        mut self,
        store: Arc<oneai_skill::SkillMetadataStore>,
    ) -> Self {
        self.skill_metadata_store = Some(store);
        self
    }

    /// Attach a durable working-state store. When set, the loop appends a
    /// working-state event at every plan-control-tool mutation (exit_plan_mode
    /// / task_update / decision resolution) so progress survives crashes and
    /// is discoverable by a brand-new session. The pinned blocks read the
    /// in-memory `LoopState.working_state` projection (rehydrated on
    /// startup/continue), not this store — zero IO per turn.
    pub fn with_working_state_store(
        mut self,
        store: Arc<dyn oneai_core::traits::WorkingStateStore>,
    ) -> Self {
        self.working_state_store = Some(store);
        self
    }

    /// Set the per-run working-state scope (user / project / session). These
    /// are threaded into `LoopState` at run start so working-state events land
    /// in the right cross-session namespace, and so a `chat --resume` /
    /// `continue` can rehydrate the bound task's projection.
    pub fn with_working_state_scope(
        mut self,
        user_id: impl Into<String>,
        project: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        self.ws_user_id = user_id.into();
        self.ws_project = project.into();
        self.ws_session_id = session_id.into();
        self
    }

    /// Thread the AgentLoop's scope into `LoopState`, and — when a task is
    /// already bound (same-session resume / cross-session continue) — rehydrate
    /// the in-memory `working_state` projection from the durable event log and
    /// restore `original_task` from the task's `goal` (so the pinned
    /// `[Task Anchor]` shows the canonical original goal, not the resume
    /// prompt like "continue"). No-op when no store is configured.
    async fn hydrate_working_state(&self, state: &mut LoopState) {
        state.user_id = self.ws_user_id.clone();
        state.project = self.ws_project.clone();
        state.session_id = self.ws_session_id.clone();
        let Some(store) = self.working_state_store.clone() else {
            return;
        };
        if let Some(task_id) = state.task_id.clone() {
            match store.get_task(&task_id).await {
                Ok(Some(ws)) => {
                    // Restore the canonical goal — fixes the §6 bug where
                    // `from_conversation` overwrote original_task with the new
                    // resume prompt.
                    if !ws.goal.is_empty() {
                        state.original_task = ws.goal.clone();
                        state
                            .conversation
                            .metadata
                            .insert("task_anchor".to_string(), ws.goal.clone());
                    }
                    state.working_state = Some(ws);
                    // Phase 2.1 Stage C — hydrate the cumulative cadence
                    // counters from the durable event log so a resumed task
                    // continues from the prior session's last fire boundary
                    // instead of re-firing from zero. `reflections_fired`
                    // becomes the true cumulative count (was in-memory
                    // telemetry that reset every run); `cadence_baseline`
                    // anchors the cadence check to the cumulative iteration.
                    let ws_hydr = state.working_state.as_ref().unwrap();
                    state.reflections_fired = ws_hydr.reflection_count as usize;
                    state.cadence_baseline = ws_hydr.last_reflection_iter;
                }
                Ok(None) => {
                    tracing::warn!(
                        "Working-state task '{}' not found in store; starting fresh",
                        task_id
                    );
                    state.task_id = None;
                }
                Err(e) => tracing::warn!("Failed to rehydrate working state: {}", e),
            }
        }
    }

    /// Enable non-blocking background sub-agent delegation (Phase 2A,
    /// fire-and-auto-notify). Constructs an [`AsyncTaskRunner`] wired to this
    /// loop's `sub_agent_factory`, `delegation_policy`, `cancel_token`, and a
    /// `sink` that injects each finished sub-agent's result back into the
    /// parent conversation + re-triggers a parent turn. The
    /// `delegate_background` meta-tool is advertised only while a runner is
    /// present and the factory has available kinds.
    ///
    /// Call after creating the AgentLoop, before running it:
    /// ```ignore
    /// let agent_loop = AgentLoop::new(...)
    ///     .with_background_delegation(DelegationPolicy::default(), sink, turn_id, bus);
    /// ```
    pub fn with_background_delegation(
        self,
        policy: DelegationPolicy,
        sink: Arc<dyn crate::async_task_runner::BackgroundCompletionSink>,
        turn_id: String,
        bus: Option<Arc<dyn oneai_bus::EngineBus>>,
    ) -> Self {
        let runner = Arc::new(crate::async_task_runner::AsyncTaskRunner::new(
            self.sub_agent_factory.clone(),
            policy,
            self.cancel_token.clone(),
            sink,
            turn_id,
            bus,
        ));
        Self {
            async_task_runner: Some(runner),
            ..self
        }
    }

    /// Legacy entry point — enables background delegation with the default
    /// `DelegationPolicy` and a no-op sink (results are discarded; use
    /// [`Self::with_background_delegation`] for fire-and-notify delivery).
    pub fn with_parallel_delegation(self) -> Self {
        self.with_background_delegation(
            DelegationPolicy::default(),
            Arc::new(crate::async_task_runner::NoopCompletionSink),
            String::new(),
            None,
        )
    }

    /// Legacy entry point — background delegation with a shared budget pool.
    pub fn with_parallel_delegation_and_budget(
        self,
        budget: oneai_core::budget::TokenBudget,
    ) -> Self {
        let policy = DelegationPolicy {
            budget_pool: Some(budget),
            ..DelegationPolicy::default()
        };
        self.with_background_delegation(
            policy,
            Arc::new(crate::async_task_runner::NoopCompletionSink),
            String::new(),
            None,
        )
    }

    /// Set the RecoveryManager for error recovery during the loop.
    ///
    /// When set, failed tool calls trigger recovery strategy evaluation.
    /// The RecoveryManager can apply Retry, ConditionalFallback, Rollback,
    /// ExternalFeedback, or Escalate strategies based on the error type.
    pub fn with_recovery_manager(
        mut self,
        manager: Arc<crate::error_recovery::RecoveryManager>,
    ) -> Self {
        self.recovery_manager = Some(manager);
        self
    }

    /// Run the Agentic Loop with an observer for real-time UI updates.
    ///
    /// The observer receives callbacks for each iteration, tool call,
    /// paradigm switch, etc., enabling interactive CLI display.
    pub async fn run_with_observer(
        &self,
        task: &str,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        let mut state = LoopState::new(task);
        self.hydrate_working_state(&mut state).await;

        if !state
            .conversation
            .messages
            .iter()
            .any(|m| m.role == Role::System)
        {
            // Append the runtime context block (current date/time + a nudge to use
            // web_search for time-sensitive questions) so the model always knows
            // "today" and reaches for search tools rather than stale memory.
            // The base prompt is resolved through `build_system_prompt`, which
            // substitutes the `{{TOOL_PREFERENCE_RULES}}` marker with rules
            // derived from the actual tool registry — so the prompt never
            // references tools the model cannot call.
            let system_prompt = format!(
                "{}{}",
                self.build_system_prompt().await,
                crate::context_assembler::runtime_context_block(),
            );
            state
                .conversation
                .add_message(Message::system(system_prompt));
        }

        self.run_loop(state, observer).await
    }

    /// Run the Agentic Loop with an existing conversation (multi-turn context).
    ///
    /// The conversation should contain prior messages from previous turns.
    /// The new user message is already appended to the conversation.
    /// This preserves multi-turn context so the model sees the full history.
    pub async fn run_with_conversation(
        &self,
        conversation: Conversation,
        task: &str,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        let mut state = LoopState::from_conversation(conversation, task);
        self.hydrate_working_state(&mut state).await;

        if !state
            .conversation
            .messages
            .iter()
            .any(|m| m.role == Role::System)
        {
            // See run_with_observer: append current date/time + search guidance,
            // and resolve the `{{TOOL_PREFERENCE_RULES}}` marker against the
            // actual tool registry.
            let system_prompt = format!(
                "{}{}",
                self.build_system_prompt().await,
                crate::context_assembler::runtime_context_block(),
            );
            state
                .conversation
                .add_message(Message::system(system_prompt));
        }

        // Materialize a frontend-forced paradigm (Directive::SwitchParadigm →
        // conversation.metadata["active_paradigm"]) before the loop runs, so
        // this turn starts under the chosen paradigm's prompt + tool filter.
        // No-op when no paradigm was forced (the default-turn path).
        self.activate_forced_paradigm(&mut state);

        self.run_loop(state, observer).await
    }

    /// The core loop logic — shared between run_with_observer and run_with_conversation.
    async fn run_loop(
        &self,
        mut state: LoopState,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        // Track structured output retry count (separate from iteration count)
        let mut structured_retry_count: usize = 0;
        // Track consecutive rate limit errors — after too many, terminate the loop
        // with a clear message instead of infinitely retrying.
        let mut consecutive_rate_limit_errors: usize = 0;
        const MAX_CONSECUTIVE_RATE_LIMIT_ERRORS: usize = 10;

        // ─── Trace: start AGENT span for the entire loop ──────────────
        let loop_span_id = if let Some(ctx) = &self.config.trace_context {
            let span_id = ctx.enter_span(SpanKind::AGENT, "agent_loop", None);
            ctx.set_attribute("agent.task", serde_json::json!(state.original_task));
            ctx.set_attribute(
                "agent.paradigm",
                serde_json::json!(paradigm_name(&state.active_paradigm)),
            );
            span_id
        } else {
            String::new()
        };

        // Run-cost token budget — consumed after each inference and checked
        // in the while condition as a cost guardrail. `None` (default) means
        // no cost cap; only `hard_max_iterations` guards against runaway.
        let mut run_budget = self.config.token_budget.clone();

        while !state.is_complete()
            && state.iterations < self.config.hard_max_iterations.unwrap_or(usize::MAX)
            && run_budget.as_ref().is_none_or(|b| b.remaining() > 0)
        {
            // ─── Check for external interrupt request ──────────────────────
            if self.interrupt_requested.load(Ordering::Relaxed) {
                self.interrupt_requested.store(false, Ordering::Relaxed);
                let reason = self.interrupt_reason.lock().await.take();
                let interrupt_point = InterruptPoint {
                    id: uuid::Uuid::new_v4().to_string(),
                    iteration: state.iterations,
                    reason: reason.unwrap_or(InterruptReason::Custom {
                        reason: "External interrupt requested".to_string(),
                    }),
                    checkpoint_id: None,
                };
                state.interrupt_points.push(interrupt_point.clone());
                state.pending_interrupt = Some(interrupt_point.clone());
                observer.on_interrupt(&interrupt_point);

                // Return partial result — the loop is paused for human feedback
                let result = state.into_result();
                observer.on_complete(&result);
                return Ok(result);
            }

            state.iterations += 1;

            tracing::info!(
                "AgentLoop iteration {} started (paradigm: {}, messages: {}, is_complete: {})",
                state.iterations,
                paradigm_name(&state.active_paradigm),
                state.conversation.messages.len(),
                state.is_complete()
            );

            // ─── Cadence-fired Reflect sub-agent (Phase 2.1 Stage A) ────────
            // Fire a background review sub-agent every `reflection_cadence`
            // iterations (mid-run), when not interrupted. The DirectAnswer
            // trigger fires separately at end-of-answer. See
            // `maybe_run_reflection`.
            //
            // Stage C: fire on the *cumulative* iteration count
            // (`cadence_baseline + iterations`), so a task resumed
            // cross-session continues from the prior session's last fire
            // boundary. The `cum > baseline` guard skips the boundary the
            // prior session already fired (baseline is itself a multiple of
            // cadence) — no redundant re-fire on resume.
            if let Some(cadence) = self.config.reflection_cadence {
                let cum = state.cadence_baseline + state.iterations as u64;
                if cadence > 0
                    && cum > state.cadence_baseline
                    && cum.is_multiple_of(cadence as u64)
                    && !self.interrupt_requested.load(Ordering::Relaxed)
                    && state.pending_interrupt.is_none()
                {
                    self.maybe_run_reflection(&mut state, observer, ReflectionTrigger::Cadence)
                        .await;
                }
            }

            // ─── Rate limiter check (wait if rate limit exceeded) ────────────
            if let Some(rate_limiter) = &self.config.rate_limiter {
                let provider_name = self.provider_name();
                let wait_time = rate_limiter
                    .wait_if_needed(&provider_name)
                    .await
                    .unwrap_or(std::time::Duration::ZERO);
                if wait_time > std::time::Duration::ZERO {
                    tracing::warn!(
                        "Rate limit exceeded for provider {}, waiting {}ms",
                        provider_name,
                        wait_time.as_millis()
                    );
                    tokio::time::sleep(wait_time).await;
                }
                let _ = rate_limiter.record_call(&provider_name).await;
            }

            // ─── Circuit breaker check (skip if provider is failing) ─────────
            if let Some(circuit_breaker) = &self.config.circuit_breaker {
                let provider_name = self.provider_name();
                let circuit_state = circuit_breaker.check(&provider_name);
                if circuit_state.is_failing() {
                    tracing::warn!(
                        "Circuit breaker is OPEN for provider {}, skipping call",
                        provider_name
                    );
                    // Skip this iteration — the loop will continue and may exit
                    // on hard_max_iterations if all calls are blocked
                    continue;
                }
            }

            observer.on_iteration_start(state.iterations, state.active_paradigm);

            // ─── Self-extension baseline (evolution-plan §3.4) ──────────
            // Snapshot the active (Footprint-gate `service_available()==true`)
            // tool set at the START of this iteration. After this turn's tool
            // batch finalizes, the diff compares the post-batch active set
            // against this baseline — so tools registered / gate-flipped
            // DURING this turn are surfaced (and the initial toolset, present
            // at iteration 1's start, is never mis-reported as "newly added").
            // Re-snapshotd every iteration; the post-batch diff is the only
            // consumer.
            {
                let tools = self.tools.read().await;
                let resolver: Option<&dyn oneai_core::traits::ExposureResolver> =
                    self.domain_pack.as_deref().map(|dp| {
                        let r: &dyn oneai_core::traits::ExposureResolver = dp;
                        r
                    });
                state.prev_active_tool_names = Some(
                    tools
                        .values()
                        .filter(|t| t.service_available())
                        // #27 — the self-extension baseline tracks
                        // schema-visible tools only, so the post-batch diff
                        // (which becomes a model-facing "new tools" note) never
                        // surfaces Hidden / Deferred / CodeModeOnly names.
                        .filter(|t| {
                            oneai_core::traits::effective_exposure(resolver, t.as_ref())
                                .is_model_visible_initial()
                        })
                        .map(|t| t.name().to_string())
                        .collect(),
                );
            }

            // ─── Trace: log iteration event ──────────────────────────
            if let Some(ctx) = &self.config.trace_context {
                ctx.log_event(
                    EventKind::WorkflowStepStart,
                    "agent.iteration",
                    HashMap::from([
                        (
                            "agent.iteration".to_string(),
                            serde_json::json!(state.iterations),
                        ),
                        (
                            "agent.paradigm".to_string(),
                            serde_json::json!(paradigm_name(&state.active_paradigm)),
                        ),
                    ]),
                );
            }

            // 1. Refresh domain context sources, then decide on compression.
            //
            // Durable/ephemeral separation: `state.conversation` is the durable
            // log (system prompt, user task, assistant replies, tool results)
            // that the loop appends to and persists. The ContextSource blocks
            // and the pinned TaskAnchor / PlanProgress / skill menu are
            // *ephemeral* — re-injected fresh every turn onto a clone of the
            // durable log, never written back. So pinned state survives context
            // compression by re-injection rather than by relying on the
            // compressor to keep it, and it doesn't accumulate over turns.
            //
            // When the request would overflow, compress the DURABLE log (not
            // the ephemeral assembly) so `discarded_messages` is real
            // transcript and the durable log stays bounded; the pinned blocks
            // are then re-injected onto the compressed durable for the request.
            // (Previously `assembled` was dropped on non-compression iterations
            // and the request used the bare durable log, so no ContextSource
            // injection ever reached the model on normal turns.)
            {
                let mut ca = self.context_assembler.write().await;
                ca.refresh_sources().await?;
            }

            // ─── Phase 2A: drain background-task progress each iteration ──
            // Forward buffered sub-agent events onto the observer so the UI
            // isn't blind during a background delegation. The parent does NOT
            // block on in-flight tasks (fire-and-auto-notify: results arrive
            // via the sink → a new turn when they're ready); it just keeps
            // working. Safe to skip when no runner is configured.
            if let Some(runner) = self.async_task_runner.as_ref() {
                runner.drain_progress(observer).await;
            }

            // Build the full request conversation: durable log clone + cached
            // ContextSource blocks + ephemeral pinned blocks (anchor / plan /
            // skills). Then check fit on this real request size; if it
            // overflows, compress the DURABLE log (so discarded_messages is
            // real transcript and the durable log stays bounded) and re-build
            // the request on top of the compressed durable.
            let mut conv_for_inference = self.context_assembler.write().await.assemble(&state)?;
            self.inject_pinned_blocks(&mut conv_for_inference, &state)
                .await;

            if self.context_budget.needs_compression(&conv_for_inference) {
                state.conversation = self
                    .context_budget
                    .compress(state.conversation.clone())
                    .await?;
                conv_for_inference = self.context_assembler.write().await.assemble(&state)?;
                self.inject_pinned_blocks(&mut conv_for_inference, &state)
                    .await;
            }

            // One-shot clear of the self-extension nudge — this turn's
            // request is now built (including any compression re-build), so
            // the new-tools note must not repeat next turn.
            state.pending_new_tools_note = None;

            // Model-aware context-fit guard (gap-analysis #3). The durable
            // compression above is budget-driven (keeps the log bounded +
            // extracts facts); this is the complementary model-window guard.
            // When a `ContextManager` is attached and `auto_trim` is on, check
            // the per-request conversation against the target model's window
            // and, if it doesn't fit, apply the configured trimming strategy
            // (TruncateOldest / ImportanceRanked / CompressMiddle /
            // SmartSummary) to the per-request clone. Previously the
            // `ContextManager` was constructed by `AppBuilder` but never
            // invoked — its 4 strategies were unreachable, so only the
            // `ContextCompressor` keep-recent path ran.
            //
            // Trimming is applied to `conv_for_inference` (the per-request
            // clone), NOT `state.conversation` — the durable log stays full
            // for persistence / replay; only this request is shrunk to fit.
            if let Some(cm) = &self.config.context_manager {
                if cm.auto_trim() {
                    let model = self
                        .provider
                        .config()
                        .model_name
                        .clone()
                        .unwrap_or_default();
                    let fit = cm.fits_context_window(&conv_for_inference, &model);
                    if !fit.fits {
                        tracing::debug!(
                            model = %model,
                            total_tokens = fit.total_tokens,
                            window = fit.context_window,
                            strategy = cm.profile_for_model(&model).trimming_strategy.name(),
                            "context doesn't fit model window; trimming per-request"
                        );
                        match cm.trim_for_model(&conv_for_inference, &model).await {
                            Ok(trimmed) => conv_for_inference = trimmed,
                            Err(e) => {
                                tracing::warn!(error = %e, "context-manager trim failed; sending untrimmed")
                            }
                        }
                    }
                }
            }

            // Sync the live plan_state into the durable log's metadata so it
            // survives compression (every compressor copies metadata verbatim)
            // and session reload — Q3 reseed. from_conversation restores it.
            if let Some(plan) = &state.plan_state {
                if let Some(serialized) = plan.to_metadata_string() {
                    state
                        .conversation
                        .metadata
                        .insert("plan_state".to_string(), serialized);
                } else {
                    state.conversation.metadata.remove("plan_state");
                }
            } else {
                state.conversation.metadata.remove("plan_state");
            }

            // 3. Build inference request (with paradigm-aware tool definitions)
            //
            // Scenario defaults for the agentic tool-use loop: when the user has
            // not configured a value, temperature falls back to 0.3 (the provider
            // API default of 1.0 is too random for reliable tool-use / coding),
            // and max_tokens/top_p defer to the provider (it knows its own model
            // ceiling — a fixed agent-side cap can exceed a model's max and error).
            // thinking_budget is opt-in (None here unless the user enabled it).
            let tool_defs = self
                .build_tool_definitions_for_paradigm(
                    state.active_paradigm_config.as_ref(),
                    state
                        .plan_state
                        .as_ref()
                        .is_some_and(|p| !p.steps.is_empty()),
                )
                .await;
            let mut request = InferenceRequest {
                conversation: conv_for_inference,
                tools: tool_defs,
                max_tokens: self.config.max_tokens,
                temperature: self.config.temperature.or(Some(0.3)),
                top_p: self.config.top_p,
                stop_sequences: self.config.stop_sequences.clone(),
                constrained_output: self.build_constrained_output(),
                thinking_budget: self.config.thinking_budget,
                metadata: HashMap::from([
                    // Pass the prompt-cache policy to the provider via
                    // metadata (providers don't depend on oneai-agent, so
                    // they read this string key instead of the typed config).
                    (
                        "prompt_cache_policy".to_string(),
                        self.config.prompt_cache_policy.as_str().to_string(),
                    ),
                ]),
            };

            // 3b. PreInfer interaction gate — the application layer can rewrite
            // the inference request (inject context / filter tools), ask for a
            // feedback-grounded retry, or skip this iteration. This replaces
            // the dead PreInfer `LifecycleHook` interactive path; in-process
            // hooks still run for audit/logging only (their Modify/Deny is not
            // applied — use the interaction gate for interactive control).
            {
                let registry = self.hook_registry.read().await;
                if registry.count_at(&HookPoint::PreInfer) > 0 {
                    let hook_context = HookContext {
                        point: HookPoint::PreInfer,
                        tool_name: None,
                        tool_args: None,
                        tool_output: None,
                        inference_request: Some(request.clone()),
                        inference_response: None,
                        iteration: state.iterations,
                        paradigm: paradigm_name(&state.active_paradigm).to_string(),
                    };
                    let _ = registry.run_hooks(HookPoint::PreInfer, hook_context).await;
                    // Hooks are non-interactive audit only; ignore Modify/Deny.
                }
            }
            if self.interaction_gate.enabled(InteractionPoint::PreInfer) {
                let resp = self
                    .interaction_gate
                    .request(InteractionRequest::PreInfer {
                        request: request.clone(),
                        iteration: state.iterations,
                        paradigm: paradigm_name(&state.active_paradigm).to_string(),
                    })
                    .await?;
                match resp {
                    InteractionResponse::Proceed => {}
                    InteractionResponse::ProceedWith { modification } => match modification {
                        InteractionModification::InjectSystemMessage(msg) => {
                            // Ephemeral injection for this iteration only — do
                            // NOT write to the durable log (would accumulate).
                            request.conversation.add_message(Message::system(msg));
                        }
                        InteractionModification::ReplaceRequest(new_req) => {
                            request = new_req;
                        }
                        _ => {}
                    },
                    InteractionResponse::Revise { feedback } => {
                        // User feedback is a durable user turn (persists + next
                        // iteration's assemble includes it) AND must appear in
                        // this iteration's request so the model sees it now.
                        state
                            .conversation
                            .add_message(Message::user(feedback.clone()));
                        request.conversation.add_message(Message::user(feedback));
                    }
                    InteractionResponse::Abort { reason } => {
                        state.conversation.add_message(Message::system(format!(
                            "Inference aborted by PreInfer gate: {}",
                            reason
                        )));
                        continue;
                    }
                    _ => {}
                }
            }

            // 3c. Compute context accounting from the assembled request
            // This uses HeuristicTokenCounter on the full assembled conversation + tool defs,
            // giving accurate per-category breakdown that the sidebar and /context command can display.
            //
            // IMPORTANT: Use the actual model name from provider config (e.g., "glm-5.1")
            // not the provider type name (e.g., "openai"). The model name determines:
            // - Context window size (glm-5.1 → 203K, gpt-4o → 200K, etc.)
            // - Tokenizer profile (chars-per-token ratios, overhead values)
            // - Provider-specific estimation parameters
            let model_name_for_accounting = self
                .provider
                .config()
                .model_name
                .as_deref()
                .unwrap_or("default");
            let accounting = oneai_core::ContextAccounting::account(
                &request.conversation,
                model_name_for_accounting,
                request.tools.len(),
            );
            observer.on_context_accounting(&accounting);

            // 4. Run inference
            // ─── Trace: start LLM span for inference ──────────────────
            let infer_span_id = if let Some(ctx) = &self.config.trace_context {
                let span_id = ctx.enter_span(SpanKind::LLM, "inference", None);
                ctx.log_event(
                    EventKind::InferenceStart,
                    "llm.inference.start",
                    HashMap::from([(
                        "agent.iteration".to_string(),
                        serde_json::json!(state.iterations),
                    )]),
                );
                span_id
            } else {
                String::new()
            };

            // Handle RateLimit errors gracefully — don't terminate the loop,
            // just wait and retry. This handles cases where provider-level retry
            // (ProviderRetryConfig) was exhausted but the rate limit might clear
            // after waiting longer.
            // Snapshot the final request before inference — the non-streaming
            // path moves `request` into `provider.infer`, but PostInfer (below)
            // needs it to build the interaction request.
            let request_snapshot = request.clone();
            let response_result = if self.config.use_streaming {
                self.run_streaming_iteration_async(&request, observer).await
            } else {
                // Wrap non-streaming inference in a cancel-aware select! so an
                // interrupt aborts the in-flight request promptly.
                tokio::select! {
                    biased;
                    _ = self.cancel_token.cancelled() => Err(oneai_core::error::OneAIError::Other(
                        "Agent interrupted during inference.".to_string(),
                    )),
                    res = self.provider.infer(request) => res,
                }
            };

            let mut response = match response_result {
                Ok(resp) => {
                    // Successful inference — reset consecutive rate limit counter
                    consecutive_rate_limit_errors = 0;
                    resp
                }
                Err(oneai_core::error::OneAIError::RateLimit(msg)) => {
                    consecutive_rate_limit_errors += 1;

                    // ─── Trace: record rate limit error ──────────────
                    if let Some(ctx) = &self.config.trace_context {
                        if !infer_span_id.is_empty() {
                            ctx.log_event_in_span(
                                &infer_span_id,
                                EventKind::Error,
                                "llm.rate_limit",
                                HashMap::from([
                                    ("error.message".to_string(), serde_json::json!(msg)),
                                    (
                                        "error.consecutive_count".to_string(),
                                        serde_json::json!(consecutive_rate_limit_errors),
                                    ),
                                ]),
                            );
                            ctx.exit_span(&infer_span_id, SpanStatus::Error);
                        }
                    }

                    if consecutive_rate_limit_errors >= MAX_CONSECUTIVE_RATE_LIMIT_ERRORS {
                        tracing::error!(
                            "AgentLoop: {} consecutive rate limit errors — terminating loop. Last error: {}",
                            consecutive_rate_limit_errors, msg
                        );
                        observer.on_interrupt(&InterruptPoint {
                            id: uuid::Uuid::new_v4().to_string(),
                            iteration: state.iterations,
                            reason: InterruptReason::Custom {
                                reason: format!(
                                    "Rate limit exceeded after {} consecutive failures: {}",
                                    consecutive_rate_limit_errors, msg
                                ),
                            },
                            checkpoint_id: None,
                        });
                        // Return partial result with error info
                        state.conversation.add_message(Message::assistant(format!(
                            "[Rate limit exceeded]: {}",
                            msg
                        )));
                        let result = state.into_result();
                        observer.on_complete(&result);
                        return Ok(result);
                    }

                    tracing::warn!(
                        "AgentLoop iteration {}: Rate limit error (consecutive: {}/{}), waiting 5s before retry. Error: {}",
                        state.iterations,
                        consecutive_rate_limit_errors,
                        MAX_CONSECUTIVE_RATE_LIMIT_ERRORS,
                        msg
                    );

                    // Wait 5 seconds before retrying — longer than provider-level backoff
                    // since this is the agent-level fallback after provider retry was exhausted
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

                    // Don't count this as a real iteration — decrement and continue
                    state.iterations -= 1;
                    continue;
                }
                Err(other_err) => {
                    // ─── Trace: record non-rate-limit error ──────────────
                    if let Some(ctx) = &self.config.trace_context {
                        if !infer_span_id.is_empty() {
                            ctx.log_event_in_span(
                                &infer_span_id,
                                EventKind::Error,
                                "llm.error",
                                HashMap::from([(
                                    "error.message".to_string(),
                                    serde_json::json!(other_err.to_string()),
                                )]),
                            );
                            ctx.exit_span(&infer_span_id, SpanStatus::Error);
                        }
                    }

                    // ─── OTEL metrics: count inference failures ────────
                    #[cfg(feature = "otel")]
                    if let Some(metrics) = &self.config.metrics_provider {
                        metrics.record_error();
                    }

                    // Other errors — propagate as before (terminates the loop)
                    return Err(other_err);
                }
            };

            // ─── Trace: end LLM span and log token usage ────────────
            if let Some(ctx) = &self.config.trace_context {
                if !infer_span_id.is_empty() {
                    ctx.log_event_in_span(
                        &infer_span_id,
                        EventKind::InferenceEnd,
                        "llm.inference.end",
                        HashMap::from([
                            (
                                "llm.prompt_tokens".to_string(),
                                serde_json::json!(response.usage.prompt_tokens),
                            ),
                            (
                                "llm.completion_tokens".to_string(),
                                serde_json::json!(response.usage.completion_tokens),
                            ),
                            (
                                "llm.total_tokens".to_string(),
                                serde_json::json!(
                                    response.usage.prompt_tokens + response.usage.completion_tokens
                                ),
                            ),
                            // Prompt-caching usage — summed by EfficiencyProfile::from_tree
                            // into cache_read_tokens / cache_creation_tokens for the cache
                            // hit ratio on the efficiency axis.
                            (
                                "llm.cache_read_tokens".to_string(),
                                serde_json::json!(response.usage.cache_read_tokens),
                            ),
                            (
                                "llm.cache_creation_tokens".to_string(),
                                serde_json::json!(response.usage.cache_creation_tokens),
                            ),
                        ]),
                    );
                    ctx.exit_span(&infer_span_id, SpanStatus::Ok);
                }
            }

            // ─── OTEL metrics: record the inference + token usage ────
            // Real counters (gap-analysis #4 — OtelMetricsProvider was never
            // instantiated before). Cheap atomic adds; no-op when not wired.
            #[cfg(feature = "otel")]
            if let Some(metrics) = &self.config.metrics_provider {
                metrics.record_inference_request();
                metrics.record_tokens(
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                );
            }

            // 4c. PostInfer interaction gate — the application layer can validate
            // / filter / REPLACE the response (fixing the old "logged but not
            // applied" dead path), or ask for a feedback-grounded retry. Hooks
            // still run for audit only.
            {
                let registry = self.hook_registry.read().await;
                if registry.count_at(&HookPoint::PostInfer) > 0 {
                    let hook_context = HookContext {
                        point: HookPoint::PostInfer,
                        tool_name: None,
                        tool_args: None,
                        tool_output: None,
                        inference_request: None,
                        inference_response: Some(response.clone()),
                        iteration: state.iterations,
                        paradigm: paradigm_name(&state.active_paradigm).to_string(),
                    };
                    let _ = registry.run_hooks(HookPoint::PostInfer, hook_context).await;
                }
            }
            if self.interaction_gate.enabled(InteractionPoint::PostInfer) {
                let resp = self
                    .interaction_gate
                    .request(InteractionRequest::PostInfer {
                        response: response.clone(),
                        request: request_snapshot.clone(),
                        iteration: state.iterations,
                        paradigm: paradigm_name(&state.active_paradigm).to_string(),
                    })
                    .await?;
                match resp {
                    InteractionResponse::Proceed => {}
                    InteractionResponse::ProceedWith {
                        modification: InteractionModification::ReplaceResponse(r),
                    } => {
                        response = r;
                    }
                    InteractionResponse::Revise { feedback } => {
                        state.conversation.add_message(response.message.clone());
                        state.conversation.add_message(Message::user(feedback));
                        continue;
                    }
                    InteractionResponse::Abort { reason } => {
                        tracing::warn!("PostInfer gate aborted response: {}", reason);
                        response = safe_fallback_response(&reason);
                    }
                    _ => {}
                }
            }

            // 4b. Notify observer of token usage and cost
            observer.on_token_usage_full(
                response.usage.prompt_tokens,
                response.usage.completion_tokens,
                response.usage.cache_read_tokens,
                response.usage.cache_creation_tokens,
            );

            // Resolve the token counts to record. Providers usually report usage
            // in their response (Anthropic streaming via message_delta, OpenAI with
            // stream_options). But some OpenAI-compatible providers (e.g. GLM) send
            // no usage in streaming — `response.usage` is all-zero. In that case,
            // fall back to counting tokens client-side with the TokenCounter so the
            // 成本 axis (tokens/cost) isn't silently zero (litellm/aider pattern).
            let provider_usage = response.usage.clone();
            let usage_is_missing =
                provider_usage.prompt_tokens == 0 && provider_usage.completion_tokens == 0;
            let (prompt_tokens, completion_tokens, is_estimated) = if usage_is_missing {
                if let Some(tc) = &self.config.token_counter {
                    let p = tc.count_conversation_tokens(&state.conversation, &response.model);
                    // Completion: sum text across content blocks (text + thinking +
                    // tool-call args). text_content() covers Text/Thinking; add tool
                    // call args explicitly since those are billed as completion tokens.
                    let mut c = tc.count_tokens(&response.message.text_content(), &response.model);
                    for block in &response.message.content {
                        if let oneai_core::ContentBlock::ToolCall { args, .. } = block {
                            c += tc.count_tokens(args, &response.model);
                        }
                    }
                    (p, c, true)
                } else {
                    // No counter configured — record zeros (preserves prior behavior).
                    (0, 0, false)
                }
            } else {
                (
                    provider_usage.prompt_tokens,
                    provider_usage.completion_tokens,
                    false,
                )
            };

            // 4c. Record usage in usage tracker (if configured)
            if let Some(usage_tracker) = &self.config.usage_tracker {
                let session_id = state.conversation.id.clone();
                let provider_name = self.provider_name();
                let mut record = oneai_core::UsageRecord::new(
                    session_id,
                    response.model.clone(),
                    provider_name,
                    prompt_tokens,
                    completion_tokens,
                );
                if is_estimated {
                    record.is_estimated = true;
                }
                // Surface prompt-cache tokens (Anthropic cache_read/creation) so
                // usage reports / TUI can show the cache-hit ratio. From the
                // provider-reported usage (0 when the provider didn't report
                // cache stats — e.g. OpenAI-compatible streams).
                record = record.with_cache_tokens(
                    provider_usage.cache_read_tokens,
                    provider_usage.cache_creation_tokens,
                );
                let _ = usage_tracker.record_usage(record).await;
            }

            // 4d. Record circuit breaker success (if configured)
            if let Some(circuit_breaker) = &self.config.circuit_breaker {
                circuit_breaker.record_success(&self.provider_name());
            }

            // 4e. Consume the run-cost token budget (termination guardrail).
            // Uses the same resolved prompt/completion counts as the usage
            // tracker above. When the budget is set, exhaustion terminates the
            // loop at the next while-check — closing the gap where a runaway
            // model burned tokens indefinitely up to hard_max_iterations.
            if let Some(b) = run_budget.as_mut() {
                b.record_usage(prompt_tokens, completion_tokens);
            }

            // 5. Parse decision
            let mut decision = self.parse_decision(&response)?;

            tracing::info!(
                "AgentLoop iteration {} decision: {} (content_blocks: {}, text_length: {}, tool_calls: {})",
                state.iterations,
                match &decision {
                    AgentDecision::DirectAnswer { .. } => "DirectAnswer".to_string(),
                    AgentDecision::ToolCalls { calls } => format!("ToolCalls({} calls)", calls.len()),
                    AgentDecision::Delegate { tasks } => format!("Delegate({} tasks)", tasks.len()),
                    AgentDecision::DelegateBackground { tasks } => {
                        format!("DelegateBackground({} tasks)", tasks.len())
                    }
                    AgentDecision::SwitchParadigm { .. } => "SwitchParadigm".to_string(),
                    AgentDecision::SwitchProject { dir, .. } => {
                        format!("SwitchProject({})", dir.display())
                    }
                },
                response.message.content.len(),
                response.message.text_content().len(),
                response.message.content.iter().filter(|b| matches!(b, ContentBlock::ToolCall { .. })).count(),
            );

            // 5b. Empty response retry — if the model produced 0 content blocks,
            // inject a clarification prompt and retry once. This handles:
            // 1) SSE format incompatibility (model returns data we can't parse)
            // 2) Model genuinely failing to respond (confused by context format)
            // 3) Streaming response that was empty/malformed
            //
            // The retry injects a follow-up message asking the model to respond,
            // giving it a second chance with a clearer prompt.
            const MAX_EMPTY_RETRIES: usize = 1;
            let mut empty_retry_count: usize = 0;
            while matches!(&decision, AgentDecision::DirectAnswer { text } if text.trim().is_empty())
                && empty_retry_count < MAX_EMPTY_RETRIES
            {
                empty_retry_count += 1;
                tracing::warn!(
                    "AgentLoop iteration {}: model produced empty response, retrying ({}/{}). \
                    This usually means the model didn't properly see the context or the \
                    streaming format caused parsing issues. Conversation has {} messages.",
                    state.iterations,
                    empty_retry_count,
                    MAX_EMPTY_RETRIES,
                    state.conversation.messages.len()
                );

                // Inject follow-up messages asking model to respond.
                // We add an empty assistant message (representing the model's
                // failed response) followed by a user message explicitly asking
                // for a response. This preserves OpenAI API format validity.
                state.conversation.add_message(Message {
                    role: Role::Assistant,
                    content: vec![], // Empty assistant response
                    metadata: HashMap::new(),
                });
                state.conversation.add_message(Message::user(
                    "You did not respond in the previous turn. Please provide a response now — \
                    either call a tool to accomplish the task, or give a direct answer."
                        .to_string(),
                ));

                // Re-build inference request with updated conversation
                let retry_tool_defs = self
                    .build_tool_definitions_for_paradigm(
                        state.active_paradigm_config.as_ref(),
                        state
                            .plan_state
                            .as_ref()
                            .is_some_and(|p| !p.steps.is_empty()),
                    )
                    .await;
                let retry_request = InferenceRequest {
                    conversation: state.conversation.clone(),
                    tools: retry_tool_defs,
                    max_tokens: self.config.max_tokens,
                    temperature: self.config.temperature.or(Some(0.3)),
                    top_p: self.config.top_p,
                    stop_sequences: self.config.stop_sequences.clone(),
                    constrained_output: self.build_constrained_output(),
                    thinking_budget: self.config.thinking_budget,
                    // Phase 1.5: carry the prompt-cache policy into the retry
                    // so the re-inference still hits the provider's prefix
                    // cache. Dropping it (the prior `HashMap::new()`) made
                    // every empty-response retry a cache miss — the retried
                    // prefix got re-billed at full price, same cost tension as
                    // 1.1. Mirrors the metadata the main request sets above.
                    metadata: HashMap::from([(
                        "prompt_cache_policy".to_string(),
                        self.config.prompt_cache_policy.as_str().to_string(),
                    )]),
                };

                // Re-run inference with the follow-up prompt
                let retry_response = if self.config.use_streaming {
                    self.run_streaming_iteration_async(&retry_request, observer)
                        .await?
                } else {
                    self.provider.infer(retry_request).await?
                };

                // Notify observer of retry token usage
                observer.on_token_usage_full(
                    retry_response.usage.prompt_tokens,
                    retry_response.usage.completion_tokens,
                    retry_response.usage.cache_read_tokens,
                    retry_response.usage.cache_creation_tokens,
                );

                // Record retry usage in usage tracker (if configured)
                if let Some(usage_tracker) = &self.config.usage_tracker {
                    let record = oneai_core::UsageRecord::new(
                        state.conversation.id.clone(),
                        retry_response.model.clone(),
                        self.provider_name(),
                        retry_response.usage.prompt_tokens,
                        retry_response.usage.completion_tokens,
                    )
                    .with_cache_tokens(
                        retry_response.usage.cache_read_tokens,
                        retry_response.usage.cache_creation_tokens,
                    );
                    let _ = usage_tracker.record_usage(record).await;
                }

                decision = self.parse_decision(&retry_response)?;

                tracing::info!(
                    "AgentLoop iteration {}: empty response retry {} produced decision: {} (content_blocks: {})",
                    state.iterations,
                    empty_retry_count,
                    match &decision {
                        AgentDecision::DirectAnswer { .. } => "DirectAnswer".to_string(),
                        AgentDecision::ToolCalls { calls } => {
                            format!("ToolCalls({} calls)", calls.len())
                        }
                        AgentDecision::Delegate { tasks } => {
                            format!("Delegate({} tasks)", tasks.len())
                        }
                        AgentDecision::DelegateBackground { tasks } => {
                            format!("DelegateBackground({} tasks)", tasks.len())
                        }
                        AgentDecision::SwitchParadigm { .. } => "SwitchParadigm".to_string(),
                        AgentDecision::SwitchProject { dir, .. } => {
                            format!("SwitchProject({})", dir.display())
                        }
                    },
                    retry_response.message.content.len(),
                );
            }

            // 5c. If retry also produced empty DirectAnswer, log and continue
            // (the loop will still end with an empty answer, but at least we tried)
            if matches!(&decision, AgentDecision::DirectAnswer { text } if text.trim().is_empty()) {
                tracing::warn!(
                    "AgentLoop iteration {}: model still produced empty DirectAnswer after retry. \
                    Giving up — loop will end with empty answer. Conversation has {} messages.",
                    state.iterations,
                    state.conversation.messages.len()
                );
            }

            // 6. Execute decision + notify observer
            // IMPORTANT: The assistant's response (containing tool calls, delegation, etc.)
            // MUST be added to the conversation BEFORE any tool results, so that the
            // OpenAI/Anthropic API format is valid: assistant message with tool_calls
            // precedes tool result messages that reference those call_ids.
            let was_complete = state.is_complete();
            match decision {
                AgentDecision::DirectAnswer { text } => {
                    observer.on_direct_answer(&text);

                    // ─── Trace: log DirectAnswer event ──────────────
                    if let Some(ctx) = &self.config.trace_context {
                        ctx.log_event(
                            EventKind::Thought,
                            "agent.direct_answer",
                            HashMap::from([(
                                "agent.answer_length".to_string(),
                                serde_json::json!(text.len()),
                            )]),
                        );
                    }

                    // ─── Structured output validation ──────────────────────────
                    // If StructuredOutputConfig is set, validate the model's final
                    // answer against the JSON Schema. If validation fails and
                    // re_prompt_on_failure is true, inject the error and continue
                    // (ModelRetry pattern from PydanticAI).
                    //
                    // Retry attempts don't count against the iteration budget —
                    // they're self-correction attempts, not new task iterations.
                    if let Some(config) = &self.config.structured_output {
                        let validation = validate_json_schema(&text, &config.schema);
                        if !validation.passed {
                            if config.re_prompt_on_failure
                                && structured_retry_count < config.max_retries
                            {
                                structured_retry_count += 1;
                                let retry = oneai_core::ModelRetry {
                                    error_message: validation.error_summary(),
                                    retry_count: structured_retry_count,
                                    expected_schema: config.schema.clone(),
                                    failed_output: text.clone(),
                                };
                                let retry_prompt = build_retry_prompt(config, &retry);
                                tracing::info!(
                                    "StructuredOutput validation failed (retry {}/{}): {}",
                                    structured_retry_count,
                                    config.max_retries,
                                    validation.error_summary()
                                );
                                // Inject the validation error as a system message
                                state
                                    .conversation
                                    .add_message(Message::system(retry_prompt));
                                // Don't finalize the answer — continue the loop for re-generation
                                // Note: we don NOT increment iterations for retries
                                continue;
                            } else {
                                // Max retries exhausted or re_prompt disabled — end with error
                                tracing::warn!(
                                    "StructuredOutput validation failed (max retries {} exhausted): {}",
                                    config.max_retries,
                                    validation.error_summary()
                                );
                                state.conversation.add_message(Message::assistant(&text));
                                state.set_final_answer(format!(
                                    "[StructuredOutput validation failed]: {}",
                                    validation.error_summary()
                                ));
                            }
                        } else {
                            // Validation passed — finalize the answer
                            state.conversation.add_message(Message::assistant(&text));
                            state.set_final_answer(text);
                        }
                    } else {
                        // No StructuredOutput config — finalize normally
                        state.conversation.add_message(Message::assistant(&text));
                        state.set_final_answer(text);
                    }
                }
                AgentDecision::ToolCalls { calls } => {
                    // ─── Malformed-args feedback (Reflexion) ──────────────────
                    // Drop tool calls whose args fail to parse and inject a
                    // self-correction `tool_result` for each, so the model
                    // retries with valid JSON instead of being silently
                    // dispatched with empty args (the old
                    // `unwrap_or(json!({}))` swallow — the "malformed output
                    // not fed back" hot-path gap).
                    let calls = self.filter_malformed_tool_args(
                        calls,
                        &mut state,
                        &response.message.content,
                    );

                    observer.on_tool_calls(&calls);

                    // ─── Trace: log tool calls ──────────────────────────
                    if let Some(ctx) = &self.config.trace_context {
                        for call in &calls {
                            ctx.log_event(
                                EventKind::Action,
                                "tool.call",
                                HashMap::from([
                                    ("tool.name".to_string(), serde_json::json!(call.name)),
                                    ("tool.call_id".to_string(), serde_json::json!(call.id)),
                                ]),
                            );
                        }
                    }

                    // ─── PreToolUse lifecycle hooks ─────────────────────────────
                    // Before executing each tool call, run PreToolUse hooks.
                    // Hooks can allow, deny, or modify the tool call args.
                    // This replaces some ApprovalGate use cases with programmatic hooks.
                    let mut filtered_calls = Vec::new();
                    {
                        let registry = self.hook_registry.read().await;
                        if registry.count_at(&HookPoint::PreToolUse) > 0 {
                            for call in &calls {
                                let hook_context = HookContext {
                                    point: HookPoint::PreToolUse,
                                    tool_name: Some(call.name.clone()),
                                    tool_args: Some(call.args.clone()),
                                    tool_output: None,
                                    inference_request: None,
                                    inference_response: None,
                                    iteration: state.iterations,
                                    paradigm: paradigm_name(&state.active_paradigm).to_string(),
                                };
                                let results = registry
                                    .run_hooks(HookPoint::PreToolUse, hook_context)
                                    .await;
                                let resolved = HookRegistry::resolve_pre_tool_use_results(
                                    &results, &call.args,
                                );
                                match resolved {
                                    ResolvedHookAction::Allow { args: _ } => {
                                        // Original args — proceed as-is
                                        filtered_calls.push(call.clone());
                                    }
                                    ResolvedHookAction::Deny { reason } => {
                                        // Hook denied this tool call — inject denial message
                                        tracing::info!(
                                            "PreToolUse hook denied tool '{}' ({})",
                                            call.name,
                                            reason
                                        );
                                        state.conversation.add_message(Message::tool_result(
                                            call.id.clone(),
                                            format!("Denied by lifecycle hook: {}", reason),
                                        ));
                                    }
                                    ResolvedHookAction::Modify { modified_args } => {
                                        // Hook modified args — use modified args
                                        tracing::info!(
                                            "PreToolUse hook modified args for tool '{}'",
                                            call.name
                                        );
                                        filtered_calls.push(ToolCallRequest {
                                            id: call.id.clone(),
                                            name: call.name.clone(),
                                            args: modified_args,
                                        });
                                    }
                                }
                            }
                        } else {
                            // No PreToolUse hooks registered — proceed with all calls
                            filtered_calls = calls.clone();
                        }
                    }

                    // Add the assistant's tool-call message to conversation FIRST
                    // (the model's response with tool calls must precede tool results)
                    state.conversation.add_message(response.message.clone());

                    // ─── Control-tool interception ───────────────────────────────
                    // task_create/task_update/task_list/exit_plan_mode are handled
                    // directly against LoopState.plan_state (per-run, agent-side
                    // state). They bypass the tool registry, approval gate, and the
                    // plan_mode block (so the model can call exit_plan_mode while
                    // planning). Control results are collected here; regular tools
                    // flow into `filtered_calls` below.
                    let mut control_results: Vec<ToolCallResult> = Vec::new();
                    let mut regular_calls: Vec<ToolCallRequest> = Vec::new();
                    for call in filtered_calls.drain(..) {
                        // Defensive backstop: delegate/switch_paradigm are
                        // model-driven meta-tools that `parse_decision` converts
                        // to `AgentDecision` *before* dispatch, so they should
                        // never reach here. If a future routing change lets one
                        // slip through, do NOT send it to the ToolExecutor
                        // (which would error "tool not found"); surface it as a
                        // tool result so the conversation stays balanced.
                        if crate::meta_tool::is_meta_tool(&call.name) {
                            tracing::warn!(
                                "Meta-tool '{}' reached the dispatch path — it should have been \
                                intercepted by parse_decision. Skipping ToolExecutor dispatch.",
                                call.name
                            );
                            control_results.push(ToolCallResult {
                                call_id: call.id.clone(),
                                tool_name: call.name.clone(),
                                output: oneai_core::ToolOutput {
                                    success: true,
                                    content: format!(
                                        "Internal meta-tool '{}' was not intercepted as expected. \
                                        Treat this as a no-op and continue.",
                                        call.name
                                    ),
                                    error: None,
                                    ..Default::default()
                                },
                            });
                            continue;
                        }
                        if !crate::plan_state::is_control_tool(&call.name) {
                            regular_calls.push(call);
                            continue;
                        }
                        // Compute the control-tool output FIRST (the exit_plan_mode
                        // gate may block awaiting the user's plan review).
                        let output = if call.name == crate::plan_state::TOOL_EXIT_PLAN_MODE {
                            let plan_text = call
                                .args
                                .get("plan")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let steps = crate::plan_state::extract_steps(&call.args);
                            // Populate the tracked plan state from the submitted
                            // steps so the panel shows them.
                            {
                                let mut ps = state.plan_state.take().unwrap_or_default();
                                ps.set_steps(steps.clone());
                                state.plan_state = Some(ps);
                            }
                            observer.on_plan_update(state.plan_state.as_ref());
                            // PlanReview via the interaction gate (single plan).
                            let resp =
                                if self.interaction_gate.enabled(InteractionPoint::PlanReview) {
                                    self.interaction_gate
                                        .request(InteractionRequest::PlanReview {
                                            plan: plan_text.clone(),
                                            steps: steps.clone(),
                                        })
                                        .await?
                                } else {
                                    InteractionResponse::Proceed
                                };
                            match resp {
                                InteractionResponse::Proceed => {
                                    self.set_plan_mode(false);
                                    self.ensure_working_state_task(&mut state, &steps, &plan_text)
                                        .await;
                                    oneai_core::ToolOutput {
                                        success: true,
                                        content: "Plan approved — proceeding with execution. \
                                            Use task_update to mark steps in_progress/completed as \
                                            you work."
                                            .to_string(),
                                        error: None,
                                        ..Default::default()
                                    }
                                }
                                InteractionResponse::ProceedWith { modification } => {
                                    if let InteractionModification::ReplacePlan {
                                        plan: new_plan,
                                        steps: new_steps,
                                    } = modification
                                    {
                                        // Apply the user's edits to the tracked plan.
                                        let mut ps = state.plan_state.take().unwrap_or_default();
                                        ps.set_steps(new_steps.clone());
                                        state.plan_state = Some(ps);
                                        observer.on_plan_update(state.plan_state.as_ref());
                                        self.set_plan_mode(false);
                                        self.ensure_working_state_task(
                                            &mut state, &new_steps, &new_plan,
                                        )
                                        .await;
                                        oneai_core::ToolOutput {
                                            success: true,
                                            content: format!(
                                                "Plan approved with edits — proceeding. \
                                                Use task_update to mark steps in_progress/completed \
                                                as you work. Edited plan:\n{}", new_plan),
                                            error: None,
                                         ..Default::default() }
                                    } else {
                                        self.set_plan_mode(false);
                                        self.ensure_working_state_task(
                                            &mut state, &steps, &plan_text,
                                        )
                                        .await;
                                        oneai_core::ToolOutput {
                                            success: true,
                                            content: "Plan approved — proceeding with execution."
                                                .to_string(),
                                            error: None,
                                            ..Default::default()
                                        }
                                    }
                                }
                                InteractionResponse::Revise { feedback } => {
                                    oneai_core::ToolOutput {
                                        success: true,
                                        content: format!(
                                            "Plan rejected with feedback: {}. \
                                            Revise the plan and call exit_plan_mode again.",
                                            feedback
                                        ),
                                        error: None,
                                        ..Default::default()
                                    }
                                }
                                InteractionResponse::Abort { reason } => oneai_core::ToolOutput {
                                    success: true,
                                    content: format!(
                                        "Plan aborted: {}. Stay in plan mode or revise.",
                                        reason
                                    ),
                                    error: None,
                                    ..Default::default()
                                },
                                _ => oneai_core::ToolOutput {
                                    success: true,
                                    content:
                                        "Plan review returned no action; staying in plan mode."
                                            .to_string(),
                                    error: None,
                                    ..Default::default()
                                },
                            }
                        } else if call.name == crate::plan_state::TOOL_ENTER_PLAN_MODE {
                            // enter_plan_mode: the model judged the task complex
                            // and escalates from normal execution into plan mode.
                            // Flip the plan_mode flag on so the NEXT iteration
                            // exposes the plan toolset (task_create /
                            // exit_plan_mode / …) and blocks execution tools.
                            // The plan sketch the model supplied is preserved as
                            // a system message so its complexity reasoning isn't
                            // lost when the system prompt is rewritten.
                            let sketch = call
                                .args
                                .get("plan")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            self.set_plan_mode(true);
                            if !sketch.is_empty() {
                                state.conversation.add_message(Message::system(format!(
                                    "[Entered plan mode — initial sketch]: {}",
                                    sketch
                                )));
                            }
                            oneai_core::ToolOutput {
                                success: true,
                                content: "Entered plan mode. Now call `task_create` to commit a \
                                    step-by-step plan, then `exit_plan_mode` to submit it for \
                                    approval. Execution tools are disabled until the plan is \
                                    approved."
                                    .to_string(),
                                error: None,
                                ..Default::default()
                            }
                        } else if call.name == crate::plan_state::TOOL_REQUEST_PLAN_DECISION {
                            // PlanDecision: the model hit a tradeoff and asks the
                            // user to choose. The reply is fed back as tool_result.
                            let decision_id = call
                                .args
                                .get("decision_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let question = call
                                .args
                                .get("question")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let context = call
                                .args
                                .get("context")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let options = call
                                .args
                                .get("options")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|o| {
                                            Some(oneai_core::DecisionOption {
                                                id: o.get("id")?.as_str()?.to_string(),
                                                label: o.get("label")?.as_str()?.to_string(),
                                                description: o
                                                    .get("description")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                tradeoffs: o
                                                    .get("tradeoffs")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                            })
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            let resp = if self
                                .interaction_gate
                                .enabled(InteractionPoint::PlanDecision)
                            {
                                self.interaction_gate
                                    .request(InteractionRequest::PlanDecision {
                                        decision_id: decision_id.clone(),
                                        question: question.clone(),
                                        context: context.clone(),
                                        options: options.clone(),
                                    })
                                    .await?
                            } else {
                                InteractionResponse::Proceed
                            };
                            match resp {
                                InteractionResponse::Choose { option_id } => {
                                    let label = options.iter().find(|o| o.id == option_id)
                                        .map(|o| o.label.clone()).unwrap_or_default();
                                    // Persist the settled decision to the
                                    // working-state event log so it survives
                                    // compaction / cross-session resume.
                                    if let (Some(store), Some(task_id)) =
                                        (self.working_state_store.clone(), state.task_id.clone())
                                    {
                                        let decision = oneai_core::Decision {
                                            id: decision_id.clone(),
                                            question: question.clone(),
                                            chosen: label.clone(),
                                            rationale: String::new(),
                                            alternatives: options.iter()
                                                .filter(|o| o.id != option_id)
                                                .map(|o| o.label.clone())
                                                .collect(),
                                            step_id: None,
                                            ts: String::new(),
                                        };
                                        if let Err(e) = store
                                            .append_event(
                                                &task_id,
                                                &state.session_id,
                                                None,
                                                oneai_core::TaskEventType::DecisionMade,
                                                oneai_core::TaskEventPayload::DecisionMade { decision },
                                            )
                                            .await
                                        {
                                            tracing::warn!("Failed to append decision_made event: {}", e);
                                        }
                                        // Bound the log's growth.
                                        self.compact_working_state_if_needed(&task_id).await;
                                        // Re-derive so [Decisions Made] pinned
                                        // block reflects it next turn.
                                        if let Ok(Some(ws)) = store.get_task(&task_id).await {
                                            state.working_state = Some(ws);
                                        }
                                    }
                                    oneai_core::ToolOutput {
                                        success: true,
                                        content: format!("User chose {} ({}). Bake this into the final plan.", option_id, label),
                                        error: None,
                                     ..Default::default() }
                                }
                                InteractionResponse::Revise { feedback } => {
                                    oneai_core::ToolOutput {
                                        success: true,
                                        content: format!("User custom decision: {}. Bake this into the final plan.", feedback),
                                        error: None,
                                     ..Default::default() }
                                }
                                InteractionResponse::Abort { reason } => {
                                    oneai_core::ToolOutput {
                                        success: true,
                                        content: format!("Decision aborted: {}. Pick a sensible default and continue planning.", reason),
                                        error: None,
                                     ..Default::default() }
                                }
                                _ => oneai_core::ToolOutput {
                                    success: true,
                                    content: "Decision auto-proceeded. Pick a sensible default and continue planning.".to_string(),
                                    error: None,
                                 ..Default::default() },
                            }
                        } else {
                            crate::plan_state::apply_control_tool(
                                &mut state.plan_state,
                                &call.name,
                                &call.args,
                            )
                        };
                        // Sync the (possibly mutated) step statuses into the
                        // durable working-state event log + re-derive the
                        // in-memory projection. No-op when no store is bound.
                        self.sync_step_status_to_working_state(&mut state).await;
                        // Now add the single tool_result message and notify the panel.
                        state.conversation.add_message(Message::tool_result(
                            call.id.clone(),
                            output.content.clone(),
                        ));
                        observer.on_plan_update(state.plan_state.as_ref());
                        control_results.push(ToolCallResult {
                            call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            output,
                        });
                    }
                    filtered_calls = regular_calls;

                    // Snapshot the regular calls' args by call_id so the
                    // recovery handler below can re-execute a transiently-failed
                    // tool call with backoff (RecoveryManager::Retry strategy).
                    // Plan-mode synthetic results never fail, so the snapshot
                    // only matters for the real-execution branch.
                    let recovery_args_by_call_id: std::collections::HashMap<
                        String,
                        serde_json::Value,
                    > = filtered_calls
                        .iter()
                        .map(|c| (c.id.clone(), c.args.clone()))
                        .collect();

                    // Plan mode — block tool execution entirely. Instead of running
                    // the tools, inject a synthetic result telling the model it must
                    // produce a plan, not execute. The model then stops calling tools
                    // and emits its plan as the final answer.
                    let mut results: Vec<ToolCallResult> =
                        if self.plan_mode() && !filtered_calls.is_empty() {
                            let plan_note = "Plan mode is active — tool execution is disabled. \
                            Do not call other tools; call `exit_plan_mode` with your plan, \
                            or present a step-by-step plan in your final answer.";
                            filtered_calls
                                .iter()
                                .map(|call| {
                                    state.conversation.add_message(Message::tool_result(
                                        call.id.clone(),
                                        plan_note.to_string(),
                                    ));
                                    ToolCallResult {
                                        call_id: call.id.clone(),
                                        tool_name: call.name.clone(),
                                        output: oneai_core::ToolOutput {
                                            success: true,
                                            content: plan_note.to_string(),
                                            error: None,
                                            ..Default::default()
                                        },
                                    }
                                })
                                .collect()
                        } else if !filtered_calls.is_empty() {
                            self.execute_tool_calls(filtered_calls).await?
                        } else {
                            // All calls were denied by hooks — no results to feed
                            Vec::new()
                        };
                    // Merge control-tool results (already handled above) with
                    // the regular tool results, preserving order roughly.
                    control_results.extend(results);
                    results = control_results;
                    for r in &results {
                        observer.on_tool_result(&r.call_id, &r.tool_name, &r.output);

                        // ─── PostToolUse lifecycle hooks ──────────────────────────
                        // After each tool execution, run PostToolUse hooks.
                        // Hooks can audit/log/transform the output.
                        {
                            let registry = self.hook_registry.read().await;
                            if registry.count_at(&HookPoint::PostToolUse) > 0 {
                                let hook_context = HookContext {
                                    point: HookPoint::PostToolUse,
                                    tool_name: Some("".to_string()), // Would need the tool name from the call
                                    tool_args: None,
                                    tool_output: Some(r.output.clone()),
                                    inference_request: None,
                                    inference_response: None,
                                    iteration: state.iterations,
                                    paradigm: paradigm_name(&state.active_paradigm).to_string(),
                                };
                                let _results = registry
                                    .run_hooks(HookPoint::PostToolUse, hook_context)
                                    .await;
                                // PostToolUse hooks are informational (audit/log) —
                                // their results don't change the tool output for now.
                                // In a future version, Modify could transform the output.
                            }
                        }
                    }

                    // Error recovery: check for failed tool calls.
                    //
                    // Transient failures (timeout / network / rate_limit) are
                    // *re-executed* here with jittered backoff — not merely
                    // announced as a system message. Other strategies
                    // (Escalate / ExternalFeedback) remain informational
                    // injections since they require model-level decisions.
                    let failed_count = results.iter().filter(|r| !r.output.success).count();
                    if failed_count > 0 {
                        tracing::warn!(
                            "{} tool calls failed in iteration {}",
                            failed_count,
                            state.iterations
                        );

                        if let Some(rm) = self.recovery_manager.clone() {
                            // Snapshot the tool registry so we can re-execute by
                            // name; dropped before any await to avoid holding the
                            // read guard across tool.execute().
                            let tool_by_name: std::collections::HashMap<
                                String,
                                std::sync::Arc<dyn oneai_core::traits::Tool>,
                            > = {
                                let tools_map = self.tools.read().await;
                                results
                                    .iter()
                                    .filter(|r| !r.output.success)
                                    .filter_map(|r| {
                                        tools_map
                                            .get(&r.tool_name)
                                            .map(|t| (r.tool_name.clone(), t.clone()))
                                    })
                                    .collect()
                            };

                            for r in results.iter_mut().filter(|r| !r.output.success) {
                                let strategy = self.select_recovery_strategy(r);
                                let context = crate::error_recovery::ValidationContext {
                                    task: state.original_task.clone(),
                                    result: r
                                        .output
                                        .error
                                        .as_deref()
                                        .unwrap_or("Unknown error")
                                        .to_string(),
                                    variables: std::collections::HashMap::from([
                                        ("tool_name".to_string(), r.tool_name.clone()),
                                        ("iteration".to_string(), state.iterations.to_string()),
                                    ]),
                                };

                                let outcome = rm.apply(&strategy, &context).await?;
                                match outcome {
                                    crate::error_recovery::RecoveryOutcome::RetryScheduled {
                                        max_retries,
                                    } => {
                                        // Actually re-execute the tool with jittered
                                        // backoff — gated by the policy's
                                        // should_retry so non-transient errors
                                        // aren't pointlessly retried.
                                        let policy = crate::error_recovery::RetryPolicy {
                                            max_retries,
                                            ..crate::error_recovery::RetryPolicy::default()
                                        };
                                        let Some(args) = recovery_args_by_call_id.get(&r.call_id)
                                        else {
                                            // No args snapshot (e.g. plan-mode
                                            // synthetic or control tool) — can't
                                            // re-execute; surface honestly.
                                            state.conversation.add_message(Message::system(
                                                format!(
                                                "Recovery: cannot retry '{}' (no args snapshot)",
                                                r.tool_name
                                            ),
                                            ));
                                            continue;
                                        };
                                        let Some(tool) = tool_by_name.get(&r.tool_name) else {
                                            continue; // tool no longer registered
                                        };

                                        // Safety gate: only retry idempotent,
                                        // read-only tools (RiskLevel::Low). A
                                        // "timeout" on a state-mutating tool is
                                        // ambiguous — the side effect may have
                                        // applied but the response was lost — so
                                        // re-execution could double-apply. Don't
                                        // risk it; surface the error instead.
                                        if tool.risk_level() != oneai_core::RiskLevel::Low {
                                            state.conversation.add_message(Message::system(
                                                format!(
                                            "Recovery: transient error on '{}' (non-idempotent, risk={:?}) not retried to avoid side effects: {}",
                                                    r.tool_name, tool.risk_level(),
                                                    r.output.error.as_deref().unwrap_or("unknown"))
                                            ));
                                            continue;
                                        }

                                        let mut last_error =
                                            r.output.error.clone().unwrap_or_default();
                                        for attempt in 0..max_retries {
                                            if !policy.should_retry(&last_error) {
                                                break;
                                            }
                                            tracing::info!(
                                                "Recovery retry {} for '{}' (attempt {}/{})",
                                                r.tool_name,
                                                r.tool_name,
                                                attempt + 1,
                                                max_retries
                                            );
                                            tokio::time::sleep(policy.compute_delay(attempt)).await;
                                            match tool.execute(args.clone()).await {
                                                Ok(out) => {
                                                    if out.success {
                                                        r.output = out;
                                                        tracing::info!(
                                            "Recovery: '{}' succeeded after {} retries",
                                            r.tool_name, attempt + 1
                                                        );
                                                        break;
                                                    }
                                                    last_error =
                                                        out.error.clone().unwrap_or_default();
                                                    r.output = out;
                                                }
                                                Err(e) => {
                                                    last_error = e.to_string();
                                                    r.output = oneai_core::ToolOutput {
                                                        success: false,
                                                        content: String::new(),
                                                        error: Some(last_error.clone()),
                                                        ..Default::default()
                                                    };
                                                }
                                            }
                                        }
                                        if !r.output.success {
                                            state.conversation.add_message(Message::system(
                                                format!(
                                                    "Recovery: '{}' failed after {} retries: {}",
                                                    r.tool_name,
                                                    max_retries,
                                                    r.output.error.as_deref().unwrap_or("unknown")
                                                ),
                                            ));
                                        }
                                    }
                                    crate::error_recovery::RecoveryOutcome::RollbackTo {
                                        checkpoint_id,
                                    } => {
                                        // The checkpoint system was removed in favor
                                        // of the append-only working-state event log
                                        // (see docs/working-state-mechanism.md).
                                        // State rollback is not available — fail
                                        // honestly rather than pretending.
                                        tracing::warn!(
                                            "Recovery rollback to checkpoint '{}' requested, but the \
                                            checkpoint system has been removed; skipping rollback.",
                                            checkpoint_id
                                        );
                                        state.conversation.add_message(Message::system(
                                            format!("Recovery: rollback to checkpoint '{}' unavailable (checkpoint system removed); re-derive state from the task event log instead.", checkpoint_id)
                                        ));
                                    }
                                    crate::error_recovery::RecoveryOutcome::ValidationFailed {
                                        feedback,
                                    } => {
                                        state.conversation.add_message(Message::system(format!(
                                            "Recovery feedback: {}",
                                            feedback
                                        )));
                                    }
                                    crate::error_recovery::RecoveryOutcome::Escalated {
                                        summary,
                                    } => {
                                        state.conversation.add_message(Message::system(format!(
                                            "Error escalated: {}",
                                            summary
                                        )));
                                    }
                                    _ => {
                                        // Other outcomes are informational — just log
                                        tracing::debug!("Recovery outcome: {:?}", outcome);
                                    }
                                }
                            }
                        }
                    }

                    // Check if any tool call was denied by the approval gate.
                    // If so, stop the agent loop to prevent repeated permission requests.
                    let has_denied = results.iter().any(|r| {
                        !r.output.success
                            && r.output
                                .error
                                .as_deref()
                                .is_some_and(|e| e.starts_with("Denied"))
                    });

                    // ─── OTEL metrics: record tool-call success/failure ──
                    // Real counters per executed tool (gap-analysis #4). Borrows
                    // `results` so the move into feed_tool_results still works.
                    #[cfg(feature = "otel")]
                    if let Some(metrics) = &self.config.metrics_provider {
                        for r in &results {
                            metrics.record_tool_call(&r.tool_name, r.output.success);
                        }
                    }

                    // Collect tool names self-reported as newly added this batch
                    // (before `feed_tool_results` moves `results`). Dedup,
                    // preserve first-seen order. Used by the self-extension
                    // diff below.
                    let reported: Vec<String> = {
                        let mut seen = std::collections::HashSet::new();
                        let mut v: Vec<String> = Vec::new();
                        for r in &results {
                            for name in &r.output.added_tool_names {
                                if seen.insert(name.clone()) {
                                    v.push(name.clone());
                                }
                            }
                        }
                        v
                    };

                    if has_denied {
                        state.set_final_answer(
                            "Task stopped: a required tool call was denied by the user."
                                .to_string(),
                        );
                        // Still feed results so the model sees the denial
                        state.feed_tool_results(results);
                    } else {
                        state.feed_tool_results(results);
                    }

                    // ─── Self-extension diff (evolution-plan §3.4) ────────
                    // Surface tools that became active this batch: the union of
                    // (a) what the executed tools self-reported via
                    // `ToolOutput::added_tool_names` and (b) the live registry
                    // diff vs. the post-last-batch baseline. The diff is
                    // authoritative (catches mid-turn registrations / gate flips
                    // that didn't self-report); the field is the explicit signal.
                    // Runs after `feed_tool_results` moved `results`, so the
                    // reported names are collected from the pre-move `results`
                    // above (OTEL block) — here we only read the registry.
                    let now_active: std::collections::HashSet<String> = {
                        let tools = self.tools.read().await;
                        let resolver: Option<&dyn oneai_core::traits::ExposureResolver> =
                            self.domain_pack.as_deref().map(|dp| {
                                let r: &dyn oneai_core::traits::ExposureResolver = dp;
                                r
                            });
                        tools
                            .values()
                            .filter(|t| t.service_available())
                            // #27 — schema-visible only, mirroring the
                            // `prev_active_tool_names` baseline so the diff
                            // never produces a model-facing note naming a
                            // Hidden / Deferred / CodeModeOnly tool.
                            .filter(|t| {
                                oneai_core::traits::effective_exposure(resolver, t.as_ref())
                                    .is_model_visible_initial()
                            })
                            .map(|t| t.name().to_string())
                            .collect()
                    };
                    let newly_active: Vec<String> = match &state.prev_active_tool_names {
                        Some(prev) => now_active
                            .iter()
                            .filter(|n| !prev.contains(*n))
                            .cloned()
                            .collect(),
                        None => Vec::new(), // first batch — establishing baseline
                    };
                    // Self-reported names, filtered to those actually present +
                    // active now (Footprint integrity: don't surface names that
                    // aren't really registered / gate-on).
                    let reported_present: Vec<String> = reported
                        .iter()
                        .filter(|n| now_active.contains(*n))
                        .cloned()
                        .collect();
                    let mut surfaced: Vec<String> = Vec::new();
                    {
                        let mut seen = std::collections::HashSet::new();
                        for n in newly_active.iter().chain(reported_present.iter()) {
                            if seen.insert(n.clone()) {
                                surfaced.push(n.clone());
                            }
                        }
                    }
                    if !surfaced.is_empty() {
                        tracing::info!(
                            "AgentLoop iteration {}: self-extension surfaced {} new tool(s): {:?}",
                            state.iterations,
                            surfaced.len(),
                            surfaced,
                        );
                        observer.on_tools_added(&surfaced);
                        state.pending_new_tools_note = Some(surfaced);
                    }
                    state.prev_active_tool_names = Some(now_active);

                    tracing::info!(
                        "AgentLoop iteration {}: ToolCalls completed. has_denied={}, conversation now has {} messages. \
                        Loop will continue with next iteration (is_complete={}).",
                        state.iterations,
                        has_denied,
                        state.conversation.messages.len(),
                        state.is_complete()
                    );
                }
                AgentDecision::Delegate { tasks } => {
                    // One `on_delegate` per task so the UI shows each sub-agent's
                    // lifecycle (start → completion) even when several are
                    // delegated in the same turn.
                    for task in &tasks {
                        observer.on_delegate(&task.id, &task.task, &task.agent_type);
                    }

                    // ─── Trace: log delegation batch event ──────────────
                    if let Some(ctx) = &self.config.trace_context {
                        ctx.log_event(
                            EventKind::WorkflowStepStart,
                            "agent.delegate",
                            HashMap::from([
                                (
                                    "agent.delegate_count".to_string(),
                                    serde_json::json!(tasks.len()),
                                ),
                                (
                                    "agent.delegate_tasks".to_string(),
                                    serde_json::json!(tasks
                                        .iter()
                                        .map(|t| serde_json::json!({
                                            "id": t.id,
                                            "task": t.task,
                                            "agent_type": format!("{:?}", t.agent_type),
                                            "depends_on": t.depends_on,
                                        }))
                                        .collect::<Vec<_>>()),
                                ),
                            ]),
                        );
                    }
                    // For delegate/switch_paradigm, these are internal meta-commands,
                    // not real tools. Convert the response to a plain text assistant
                    // message (stripping the internal ToolCall blocks) to avoid
                    // orphaned tool calls with no matching tool results.
                    let text_content = response.message.text_content();
                    if !text_content.is_empty() {
                        state
                            .conversation
                            .add_message(Message::assistant(&text_content));
                    }
                    // Schedule the batch: independent tasks run concurrently,
                    // dependent tasks run after their deps and receive the deps'
                    // summaries prepended to their task text.
                    //
                    // Opt 4 Fork-lite: for tasks that set `inherit_context`,
                    // snapshot the parent's trailing non-system messages here
                    // (COW clone — the parent durable log is untouched). Done
                    // in the handler rather than `parse_decision` because the
                    // parent conversation lives in `state`, which parse_decision
                    // doesn't see. `parse_decision` only records the flags.
                    let mut tasks = tasks;
                    for task in &mut tasks {
                        if task.inherit_context {
                            let n = if task.inherit_last_n == 0 {
                                6
                            } else {
                                task.inherit_last_n
                            };
                            let seed: Vec<oneai_core::Message> = {
                                let all_non_sys: Vec<oneai_core::Message> = state
                                    .conversation
                                    .messages
                                    .iter()
                                    .filter(|m| m.role != Role::System)
                                    .cloned()
                                    .collect();
                                let len = all_non_sys.len();
                                if len > n {
                                    all_non_sys[len - n..].to_vec()
                                } else {
                                    all_non_sys
                                }
                            };
                            if !seed.is_empty() {
                                task.seed_messages = Some(seed);
                            }
                        }
                    }
                    // Capture each delegation's tool-call id (the streaming
                    // path already fired `on_tool_calls` for the `delegate`
                    // meta-tool calls, so the frontend has a pending tool-call
                    // card per delegation; `delegate` is intercepted here and
                    // never dispatched to the ToolExecutor, so without a
                    // synthetic `tool_result` those cards would stay "running"
                    // forever). `summaries` come back in input order, so the
                    // zip lines each summary up with its call id.
                    let call_ids: Vec<String> = tasks.iter().map(|t| t.call_id.clone()).collect();
                    let delegate_ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
                    let summaries = self.spawn_sub_agents_batch(tasks, observer).await?;
                    for i in 0..summaries.len() {
                        let summary = &summaries[i];
                        let call_id = &call_ids[i];
                        let delegate_id = &delegate_ids[i];
                        state.feed_sub_agent_result(summary.clone());
                        observer.on_delegate_complete(delegate_id, summary);
                        // Synthetic tool_result so the frontend's `delegate`
                        // tool-call card resolves to "done" with the
                        // sub-agent's summary as its output (mirrors how
                        // `switch_project` feeds back a confirmation).
                        let tool_output = oneai_core::ToolOutput {
                            success: summary.completed,
                            content: summary.summary.clone(),
                            error: None,
                            added_tool_names: Vec::new(),
                            ..Default::default()
                        };
                        observer.on_tool_result(call_id, "delegate", &tool_output);
                    }
                }
                AgentDecision::DelegateBackground { tasks } => {
                    // Phase 2A non-blocking background delegation. Mirrors the
                    // Delegate arm's prep (on_delegate lifecycle + text-content
                    // assistant message + inherit_context seed snapshot) but
                    // submits each task to the AsyncTaskRunner and returns
                    // immediately — the loop does NOT wait for the sub-agents.
                    for task in &tasks {
                        observer.on_delegate(&task.id, &task.task, &task.agent_type);
                    }
                    if let Some(ctx) = &self.config.trace_context {
                        ctx.log_event(
                            EventKind::WorkflowStepStart,
                            "agent.delegate_background",
                            HashMap::from([(
                                "agent.delegate_background_count".to_string(),
                                serde_json::json!(tasks.len()),
                            )]),
                        );
                    }
                    let text_content = response.message.text_content();
                    if !text_content.is_empty() {
                        state
                            .conversation
                            .add_message(Message::assistant(&text_content));
                    }
                    // Opt 4 Fork-lite seed snapshot (same as the Delegate arm).
                    let mut tasks = tasks;
                    for task in &mut tasks {
                        if task.inherit_context {
                            let n = if task.inherit_last_n == 0 {
                                6
                            } else {
                                task.inherit_last_n
                            };
                            let seed: Vec<oneai_core::Message> = {
                                let all_non_sys: Vec<oneai_core::Message> = state
                                    .conversation
                                    .messages
                                    .iter()
                                    .filter(|m| m.role != Role::System)
                                    .cloned()
                                    .collect();
                                let len = all_non_sys.len();
                                if len > n {
                                    all_non_sys[len - n..].to_vec()
                                } else {
                                    all_non_sys
                                }
                            };
                            if !seed.is_empty() {
                                task.seed_messages = Some(seed);
                            }
                        }
                    }
                    let runner = match self.async_task_runner.as_ref() {
                        Some(r) => r,
                        None => {
                            for task in &tasks {
                                let tool_output = oneai_core::ToolOutput {
                                    success: false,
                                    content: String::new(),
                                    error: Some(
                                        "Background delegation runner not configured".to_string(),
                                    ),
                                    ..Default::default()
                                };
                                observer.on_tool_result(&task.call_id, "delegate", &tool_output);
                            }
                            continue;
                        }
                    };
                    let mut submitted_ids: Vec<String> = Vec::new();
                    for task in tasks {
                        let call_id = task.call_id.clone();
                        let id = runner.submit_delegate(task).await?;
                        submitted_ids.push(id.clone());
                        // Synthetic tool_result so the frontend's
                        // `delegate_background` tool-call card resolves to
                        // "done" immediately (the sub-agent runs detached).
                        let tool_output = oneai_core::ToolOutput {
                            success: true,
                            content: format!(
                                "Launched background task '{id}'. It runs detached; you will be \
                                 notified automatically when it finishes (a new message with its \
                                 result will arrive). DO NOT poll, call task_status, or duplicate \
                                 this task's work. Either work on a DIFFERENT non-overlapping \
                                 task, or briefly tell the user what you launched and END your \
                                 response now."
                            ),
                            error: None,
                            ..Default::default()
                        };
                        observer.on_tool_result(&call_id, "delegate", &tool_output);
                    }
                    // ─── In-conversation submission record ───────────────────
                    // The tool_result above is an observer callback (UI only);
                    // it does NOT reach the model's context. Without this
                    // durable record the model re-infers seeing only its own
                    // plan text + no confirmation that tasks are running, and
                    // re-delegates the same work in a loop. This assistant
                    // message is the in-context signal that the tasks are
                    // launched and that the model should end its turn.
                    state.conversation.add_message(Message::assistant(format!(
                        "[Launched background sub-agents: {}. They are running detached; you WILL \
                         be notified automatically when each finishes (a new message carrying its \
                         result will arrive and resume you). Do NOT re-delegate this work or poll \
                         for status — either work on a DIFFERENT non-overlapping task, or end your \
                         response now and wait for the completion notifications.]",
                        submitted_ids.join(", ")
                    )));
                    // Forward any progress that already arrived this iteration.
                    runner.drain_progress(observer).await;
                }
                AgentDecision::SwitchParadigm { paradigm } => {
                    observer.on_paradigm_switch(paradigm);

                    // ─── Trace: log paradigm switch event ──────────────────
                    if let Some(ctx) = &self.config.trace_context {
                        ctx.log_event(
                            EventKind::WorkflowStepStart,
                            "agent.paradigm_switch",
                            HashMap::from([
                                (
                                    "agent.new_paradigm".to_string(),
                                    serde_json::json!(paradigm_name(&paradigm)),
                                ),
                                (
                                    "agent.old_paradigm".to_string(),
                                    serde_json::json!(paradigm_name(&state.active_paradigm)),
                                ),
                            ]),
                        );
                    }
                    let text_content = response.message.text_content();
                    if !text_content.is_empty() {
                        state
                            .conversation
                            .add_message(Message::assistant(&text_content));
                    }
                    // Try to execute a predefined StateGraph for this paradigm,
                    // fall back to semantic paradigm switch if no graph is available.
                    let result = self
                        .apply_paradigm_switch_with_graph(paradigm, &mut state)
                        .await?;
                    state.feed_paradigm_result(paradigm, result);
                }
                AgentDecision::SwitchProject { call_id, dir } => {
                    // Canonicalize so path-contains checks in the file-tool
                    // sandbox and the sources' `find`/`git` cd resolve the
                    // same absolute path the model meant. Fall back to the raw
                    // arg if canonicalization fails (dir doesn't exist yet).
                    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);

                    // ─── Trace: log project switch event ─────────────────
                    if let Some(ctx) = &self.config.trace_context {
                        ctx.log_event(
                            EventKind::WorkflowStepStart,
                            "agent.project_switch",
                            HashMap::from([(
                                "agent.project_dir".to_string(),
                                serde_json::json!(dir.to_string_lossy().to_string()),
                            )]),
                        );
                    }

                    // Surface any preamble text the model emitted before the
                    // switch call, then rebind every path-bound context source.
                    let text_content = response.message.text_content();
                    if !text_content.is_empty() {
                        state
                            .conversation
                            .add_message(Message::assistant(&text_content));
                    }
                    let rebound = self
                        .context_assembler
                        .write()
                        .await
                        .rebind_project_dir(&dir);
                    tracing::info!(
                        "AgentLoop switch_project → {} ({} sources rebound)",
                        dir.display(),
                        rebound
                    );
                    // Feed back a tool_result so the model sees the switch was
                    // honored — and knows the new context lands next iteration.
                    let confirmation = format!(
                        "Project context re-bound to {} ({} sources rebound). \
                         Next iteration injects the new project's instructions / \
                         repo map / file tree / config / git status. \
                         Note: the file-tool and shell sandboxes stay scoped to \
                         the startup project — use absolute paths via shell for \
                         file operations on the new project.",
                        dir.display(),
                        rebound
                    );
                    state
                        .conversation
                        .add_message(Message::tool_result(call_id.clone(), confirmation.clone()));
                    let tool_output = ToolOutput {
                        success: true,
                        content: confirmation,
                        error: None,
                        added_tool_names: Vec::new(),
                        ..Default::default()
                    };
                    observer.on_tool_result(&call_id, "switch_project", &tool_output);
                }
            }

            // ─── DirectAnswer-triggered Reflect (Phase 2.1 Stage A) ─────────
            // If the loop just completed this iteration (was not complete
            // before the match, is complete after) and cadence is configured,
            // fire a final background reflection on the delivered answer —
            // unless the user interrupted. Mid-run cadence firing happened
            // above at the iteration boundary; this is the end-of-run one.
            if !was_complete
                && state.is_complete()
                && self.config.reflection_cadence.is_some()
                && !self.interrupt_requested.load(Ordering::Relaxed)
                && state.pending_interrupt.is_none()
            {
                self.maybe_run_reflection(&mut state, observer, ReflectionTrigger::DirectAnswer)
                    .await;
            }

            // 7. Per-iteration checkpoint tick. Working-state persistence now
            // happens incrementally at control-tool points (exit_plan_mode /
            // task_update / decision), not at the iteration boundary — so
            // there's no full-state snapshot to save here. The observer tick
            // is retained for UI continuity.
            observer.on_checkpoint(state.iterations);
        }

        // If the loop exited due to token-budget exhaustion (not natural
        // completion), surface a clear note instead of a silent empty result
        // — so a cost-capped termination is distinguishable from a bug.
        if !state.is_complete {
            if let Some(b) = &run_budget {
                if b.remaining() == 0 {
                    state.final_answer = Some(format!(
                        "[Token budget of {} tokens exhausted. Run terminated to limit cost after {} iterations.]",
                        b.total, state.iterations
                    ));
                    tracing::warn!(
                        total = b.total,
                        iterations = state.iterations,
                        "run-cost token budget exhausted — terminating loop"
                    );
                }
            }
        }

        let result = state.into_result();

        tracing::info!(
            "AgentLoop completed: iterations={}, completed={}, final_answer_len={}, final_answer_preview={}",
            result.iterations,
            result.completed,
            result.final_answer.len(),
            if result.final_answer.len() > 100 {
                // Use char-boundary-safe truncation to avoid panic on CJK strings
                let end = result.final_answer.char_indices()
                    .take_while(|(i, _)| *i < 100)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                format!("{}...", &result.final_answer[..end])
            } else {
                result.final_answer.clone()
            }
        );

        // ─── Trace: end AGENT span for the loop ──────────────────
        if let Some(ctx) = &self.config.trace_context {
            if !loop_span_id.is_empty() {
                ctx.set_attribute_on_span(
                    &loop_span_id,
                    "agent.iterations",
                    serde_json::json!(result.iterations),
                );
                ctx.set_attribute_on_span(
                    &loop_span_id,
                    "agent.completed",
                    serde_json::json!(result.completed),
                );
                ctx.exit_span(
                    &loop_span_id,
                    if result.completed {
                        SpanStatus::Ok
                    } else {
                        SpanStatus::Error
                    },
                );
            }
        }

        observer.on_complete(&result);
        Ok(result)
    }

    /// Run the Agentic Loop without an observer (silent mode).
    pub async fn run(&self, task: &str) -> Result<AgentLoopResult> {
        struct SilentObserver;
        impl AgentLoopObserver for SilentObserver {
            fn on_iteration_start(&self, _: usize, _: ParadigmKind) {}
            fn on_direct_answer(&self, _: &str) {}
            fn on_tool_calls(&self, _: &[ToolCallRequest]) {}
            fn on_tool_result(&self, _: &str, _: &str, _: &ToolOutput) {}
            fn on_delegate(&self, _: &str, _: &str, _: &SubAgentKind) {}
            fn on_paradigm_switch(&self, _: ParadigmKind) {}
            fn on_checkpoint(&self, _: usize) {}
            fn on_complete(&self, _: &AgentLoopResult) {}
            fn on_thinking(&self, _: &str) {}
        }
        self.run_with_observer(task, &SilentObserver).await
    }

    // ─── StateGraph-driven Execution ─────────────────────────────────────────

    /// Run the AgentLoop using a StateGraph as the execution skeleton.
    ///
    /// This is the P2-2 "闭环" execution mode — when a DomainPack has a
    /// predefined StateGraph (e.g., "react-loop" for ReAct), the AgentLoop
    /// can execute it as an alternative to the standard while loop. The graph
    /// nodes delegate to the AgentLoop's own infrastructure (hooks, permission,
    /// domain pack, tool definitions) via `AgentLoopGraphActionExecutor`.
    ///
    /// This makes StateGraph execution a first-class mode of the AgentLoop,
    /// not a separate disconnected system. The key benefits:
    /// - LlmInfer nodes get proper tool definitions (filtered by paradigm config)
    /// - ToolCall nodes go through PreToolUse/PostToolUse hooks and domain permissions
    /// - Edge routing uses parsed_decision (GraphDecision) instead of string matching
    /// - SwitchParadigm nodes change the active paradigm for subsequent nodes
    ///
    /// If no StateGraph matching `graph_key` is found, falls back to the
    /// standard AgentLoop execution (`run_with_observer()`).
    pub async fn run_with_state_graph(
        &self,
        task: &str,
        graph_key: &str,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        // 1. Look up the StateGraph from DomainPack
        let graph = self
            .domain_pack
            .as_ref()
            .and_then(|dp| dp.get_state_graph(graph_key))
            .cloned();

        if graph.is_none() {
            tracing::info!(
                "No StateGraph '{}' found in DomainPack. Falling back to standard AgentLoop execution.",
                graph_key
            );
            // Fall back to standard execution
            return self.run_with_observer(task, observer).await;
        }

        let graph = graph.unwrap();
        tracing::info!(
            "Found StateGraph '{}' with {} nodes. Starting StateGraph-driven execution.",
            graph.name,
            graph.node_count()
        );

        // 2. Build GraphActionExecutor bridge
        let action_executor: Arc<dyn oneai_workflow::GraphActionExecutor> =
            Arc::new(AgentLoopGraphActionExecutor {
                provider: self.provider.clone(),
                tools: self.tools.clone(),
                parser: self.parser.clone(),
                interaction_gate: self.interaction_gate.clone(),
                domain_pack: self.domain_pack.clone(),
                hook_registry: self.hook_registry.clone(),
                recovery_manager: self.recovery_manager.clone(),
                config: self.config.clone(),
            });

        // 3. Build DelegateFactory bridge
        let delegate_factory: Arc<dyn oneai_workflow::DelegateFactory> = Arc::new(
            crate::sub_agent::SubAgentDelegateFactory::new(self.sub_agent_factory.clone()),
        );

        // 4. Build initial GraphState from task
        let mut initial_state = oneai_workflow::GraphState::new();
        initial_state
            .conversation
            .add_message(Message::user(task.to_string()));
        if !initial_state
            .conversation
            .messages
            .iter()
            .any(|m| m.role == Role::System)
        {
            initial_state
                .conversation
                .add_message(Message::system(self.config.system_prompt.clone()));
        }
        initial_state
            .variables
            .insert("task".to_string(), task.to_string());
        initial_state.active_paradigm = Some("react".to_string()); // Default paradigm for StateGraph

        // Set budget if available
        initial_state.token_budget_remaining = 100_000; // Default budget for StateGraph execution

        // 5. Create StateGraphExecutor with the bridge
        let executor = oneai_workflow::StateGraphExecutor::new(
            action_executor,
            delegate_factory,
            self.interaction_gate.clone(),
            self.config.hard_max_iterations.unwrap_or(50),
        );

        // 6. Execute the graph
        observer.on_iteration_start(1, ParadigmKind::ReAct);

        let graph_result = executor.execute(&graph, initial_state).await?;

        // 7. Convert GraphExecutionResult → AgentLoopResult
        let result = AgentLoopResult {
            conversation: graph_result.final_state.conversation,
            final_answer: graph_result
                .final_state
                .last_result
                .clone()
                .unwrap_or_default(),
            global_state: oneai_core::GlobalState::new(),
            iterations: graph_result.iterations,
            completed: graph_result.completed,
            active_paradigm: match graph_result.final_state.active_paradigm.as_deref() {
                Some("plan") => ParadigmKind::Plan,
                Some("reflect") => ParadigmKind::Reflect,
                Some("explore") => ParadigmKind::Explore,
                _ => ParadigmKind::ReAct,
            },
            sub_agent_results: Vec::new(),
        };

        observer.on_complete(&result);
        Ok(result)
    }

    // ─── Interrupt/Resume ────────────────────────────────────────────────

    /// Request an interrupt at the next iteration boundary.
    ///
    /// The loop will pause after completing the current iteration,
    /// emit `on_interrupt()`, and return a partial `AgentLoopResult`.
    /// The interrupt reason is stored and included in the `InterruptPoint`.
    ///
    /// The caller can then call `resume_from_interrupt()` to inject
    /// human feedback and continue execution.
    ///
    /// This is inspired by LangGraph's interrupt() pattern:
    /// the loop pauses at a clean boundary point, preserving all state,
    /// and resumes when the human provides guidance.
    pub fn request_interrupt(&self, reason: InterruptReason) {
        self.interrupt_requested.store(true, Ordering::Relaxed);
        // Fire the cancellation token so any in-flight provider.infer / stream
        // / tool execution wrapped in `tokio::select!` aborts immediately,
        // rather than the user waiting for the current call to finish.
        self.cancel_token.cancel();
        // Store the reason — use try_lock since this is a synchronous method.
        // If the lock is held by the async loop, we'll still set the AtomicBool flag,
        // and the loop will check it at the next iteration boundary.
        if let Ok(mut guard) = self.interrupt_reason.try_lock() {
            *guard = Some(reason);
        }
    }

    /// The cancellation token — used by `tokio::select!` branches around
    /// inference/streaming/tool execution so they abort on `request_interrupt`.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Resume the agent loop from an interrupt point.
    ///
    /// This method creates a new LoopState from the interrupt context,
    /// injects the human feedback as a system message, and continues
    /// the loop execution.
    ///
    /// The `ResumeSignal` contains:
    /// - The interrupt ID being resumed from
    /// - Human feedback text
    /// - A `ResumeAction` (Continue, Modify, or Stop)
    ///
    /// Based on the ResumeAction:
    /// - **Continue**: Inject feedback and continue the loop
    /// - **Modify**: Inject feedback and modify the approach
    /// - **Stop**: Set a final answer and terminate the loop
    pub async fn resume_from_interrupt(
        &self,
        signal: ResumeSignal,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        observer.on_resume(&signal);

        // Create a new LoopState from the interrupt context
        // The conversation should already contain prior messages
        // (we start fresh with a new task that includes the feedback)
        let feedback_task = format!("[Human feedback]: {}", signal.feedback);

        match signal.action {
            ResumeAction::Continue => {
                // Continue execution with the feedback injected
                self.run_with_observer(&feedback_task, observer).await
            }
            ResumeAction::Modify { modified_args } => {
                // Modify the approach based on feedback
                let modify_msg = if let Some(args) = modified_args {
                    format!(
                        "[Human feedback]: {}. Modified approach: {}",
                        signal.feedback, args
                    )
                } else {
                    format!(
                        "[Human feedback]: {}. Please adjust your approach.",
                        signal.feedback
                    )
                };
                self.run_with_observer(&modify_msg, observer).await
            }
            ResumeAction::Stop => {
                // Human decided to abort — return a final result
                let result = AgentLoopResult {
                    conversation: Conversation::new(),
                    final_answer: format!("Task stopped by human: {}", signal.feedback),
                    global_state: oneai_core::GlobalState::new(),
                    iterations: 0,
                    completed: true,
                    active_paradigm: ParadigmKind::ReAct,
                    sub_agent_results: Vec::new(),
                };
                observer.on_complete(&result);
                Ok(result)
            }
        }
    }

    /// Get a reference to the hook registry for registering lifecycle hooks.
    ///
    /// Hooks can be registered before the loop starts running.
    /// They will be called at their registered lifecycle points.
    pub fn hook_registry(&self) -> Arc<tokio::sync::RwLock<HookRegistry>> {
        self.hook_registry.clone()
    }

    // ─── Internal methods ──────────────────────────────────────────────

    fn parse_decision(&self, response: &InferenceResponse) -> Result<AgentDecision> {
        let mut tool_calls = Vec::new();
        let mut text_parts = Vec::new();
        // All `delegate` calls in this turn are accumulated into a batch so the
        // model can fan out several sub-agents per iteration. They are resolved
        // into an `AgentDecision::Delegate` *after* the loop, once every id is
        // known (so `depends_on` references can be validated against the full set).
        let mut delegate_tasks: Vec<DelegateTask> = Vec::new();
        // Phase 2A: `delegate_background` calls are accumulated separately so
        // they don't mix with blocking `delegate` in one decision. Resolved
        // into an `AgentDecision::DelegateBackground` after the loop.
        let mut bg_tasks: Vec<DelegateTask> = Vec::new();

        for block in &response.message.content {
            match block {
                ContentBlock::ToolCall { id, name, args } => {
                    // Parse tool args via the shared helper. Malformed args
                    // fall back to empty `{}` here; the `ToolCalls` branch
                    // re-derives the raw string and feeds a clear error back
                    // to the model (Reflexion) rather than silently dispatching.
                    let args_value: serde_json::Value =
                        self.parse_tool_args(args).unwrap_or_else(|err| {
                            tracing::warn!(
                                tool = %name,
                                call_id = %id,
                                error = %err,
                                "malformed tool args at parse_decision"
                            );
                            serde_json::json!({})
                        });
                    if name == "delegate" {
                        if let Some(task) = args_value.get("task").and_then(|v| v.as_str()) {
                            let agent_type_str = args_value
                                .get("agent_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Code");
                            let budget_tokens = args_value
                                .get("budget_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(5000);
                            // Prefer the model-supplied id; fall back to the
                            // tool-call id so every delegation has a stable key.
                            let task_id = args_value
                                .get("id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| id.clone());
                            let depends_on: Vec<String> = args_value
                                .get("depends_on")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let custom_role = args_value
                                .get("custom_role")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let system_prompt_override = args_value
                                .get("system_prompt")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let tools_override = args_value
                                .get("tools")
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect::<Vec<String>>()
                                });
                            let inherit_context = args_value
                                .get("inherit_context")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let inherit_last_n = args_value
                                .get("inherit_last_n_messages")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize)
                                .unwrap_or(0);
                            // Map the agent_type string → SubAgentKind. "Custom"
                            // carries its `custom_role` name (defaulting to
                            // "custom" when the model omitted it).
                            let agent_type = match agent_type_str {
                                "Custom" => crate::sub_agent::SubAgentKind::Custom(
                                    custom_role.clone().unwrap_or_else(|| "custom".to_string()),
                                ),
                                other => crate::sub_agent::SubAgentKind::from_str(other),
                            };
                            // Phase 2A: `background=true` routes to fire-and-
                            // auto-notify (DelegateBackground); default false
                            // is the blocking batch (Delegate). depends_on only
                            // applies to foreground — strip it in background
                            // mode (the model sequences across turn notifications).
                            let background = args_value
                                .get("background")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let task_entry = DelegateTask {
                                id: task_id,
                                task: task.to_string(),
                                agent_type,
                                budget: oneai_core::budget::TokenBudget::new(budget_tokens as u32),
                                depends_on,
                                custom_role,
                                system_prompt_override,
                                tools_override,
                                inherit_context,
                                inherit_last_n,
                                seed_messages: None,
                                call_id: id.clone(),
                            };
                            if background {
                                let mut bg = task_entry;
                                if !bg.depends_on.is_empty() {
                                    tracing::info!(
                                        "delegate '{}' background=true with depends_on {:?} — \
                                         ignored (background mode sequences via turn notifications)",
                                        bg.id,
                                        bg.depends_on
                                    );
                                    bg.depends_on.clear();
                                }
                                bg_tasks.push(bg);
                            } else {
                                delegate_tasks.push(task_entry);
                            }
                        }
                        // A `delegate` block is never dispatched to the
                        // ToolExecutor — it is intercepted here. Continue scanning
                        // for further delegate calls in the same turn.
                        continue;
                    }
                    if name == "switch_paradigm" {
                        if let Some(p) = args_value.get("paradigm").and_then(|v| v.as_str()) {
                            let paradigm = match p {
                                "plan" => ParadigmKind::Plan,
                                "react" => ParadigmKind::ReAct,
                                "reflect" => ParadigmKind::Reflect,
                                "explore" => ParadigmKind::Explore,
                                _ => ParadigmKind::ReAct,
                            };
                            return Ok(AgentDecision::SwitchParadigm { paradigm });
                        }
                    }
                    if name == "switch_project" {
                        // Re-bind the project context to a different project root.
                        // Like `switch_paradigm`, intercepted here and never
                        // dispatched to the ToolExecutor; the loop handler feeds
                        // back a `tool_result` confirmation and the next
                        // iteration injects the new project's context. Any other
                        // tool calls in the same turn are dropped (they would
                        // run against the old/stale project context).
                        let dir_str = args_value
                            .get("project_dir")
                            .and_then(|v| v.as_str())
                            .or_else(|| args_value.get("path").and_then(|v| v.as_str()));
                        if let Some(dir_str) = dir_str {
                            let dir = PathBuf::from(dir_str);
                            return Ok(AgentDecision::SwitchProject {
                                call_id: id.clone(),
                                dir,
                            });
                        }
                    }
                    tool_calls.push(ToolCallRequest {
                        id: id.clone(),
                        name: name.clone(),
                        args: args_value,
                    });
                }
                ContentBlock::Text { text } => {
                    text_parts.push(text.clone());
                }
                _ => {}
            }
        }

        if !bg_tasks.is_empty() {
            // `depends_on` is accepted on `DelegateTask` (shared with blocking
            // `delegate`) but ignored in background mode — fire-and-auto-notify
            // sequences across turns (the model delegates, the result arrives
            // in a new turn, then it delegates the dependent with that context).
            // Strip any depends_on so it doesn't mislead the runner.
            for task in &mut bg_tasks {
                if !task.depends_on.is_empty() {
                    tracing::info!(
                        "delegate_background '{}' has depends_on {:?} — ignored in background mode (sequence via turn notifications instead)",
                        task.id,
                        task.depends_on
                    );
                    task.depends_on.clear();
                }
            }
            return Ok(AgentDecision::DelegateBackground { tasks: bg_tasks });
        }

        if !delegate_tasks.is_empty() {
            // Validate `depends_on` against the known id set. References to
            // unknown ids are dropped (with a warning) rather than failing the
            // turn — the model's output is unreliable, and a partial DAG is
            // more useful than an error.
            let known_ids: std::collections::HashSet<String> =
                delegate_tasks.iter().map(|t| t.id.clone()).collect();
            for task in &mut delegate_tasks {
                let before = task.depends_on.len();
                task.depends_on.retain(|dep| {
                    let known = known_ids.contains(dep);
                    if !known {
                        tracing::warn!(
                            "Delegate '{}' depends_on unknown id '{}' — dropping dependency",
                            task.id,
                            dep
                        );
                    }
                    known
                });
                if task.depends_on.len() != before {
                    tracing::info!(
                        "Delegate '{}' depends_on trimmed from {} to {} valid references",
                        task.id,
                        before,
                        task.depends_on.len()
                    );
                }
            }
            return Ok(AgentDecision::Delegate {
                tasks: delegate_tasks,
            });
        }
        if !tool_calls.is_empty() {
            return Ok(AgentDecision::ToolCalls { calls: tool_calls });
        }
        Ok(AgentDecision::DirectAnswer {
            text: text_parts.join("\n"),
        })
    }

    /// Parse a tool-call's raw args string into a JSON value. Returns the
    /// parse/repair error string on failure. Routes through the injected
    /// `OutputParser` so Layer 2 fuzzy repair (closing unclosed brackets,
    /// extracting embedded JSON) recovers mildly-malformed args instead of
    /// failing outright — the gap-analysis hot-path fix that makes the
    /// ThreeLayerParser's Layer 2 actually reachable. Unrepairable args
    /// still error; the caller (`filter_malformed_tool_args`) feeds that
    /// error back to the model as a Reflexion-style self-correction prompt.
    fn parse_tool_args(&self, raw: &str) -> std::result::Result<serde_json::Value, String> {
        self.parser.repair_tool_args(raw).map_err(|e| e.to_string())
    }

    /// Re-derive the raw args string for a tool-call id from the response's
    /// content blocks. `parse_decision` consumed the block into a parsed
    /// `Value` (empty on failure); the `ToolCalls` branch re-reads the raw
    /// string to detect malformed args for Reflexion feedback.
    fn raw_args_for<'a>(content: &'a [oneai_core::ContentBlock], id: &str) -> Option<&'a str> {
        content.iter().find_map(|block| match block {
            oneai_core::ContentBlock::ToolCall { id: bid, args, .. } if bid == id => {
                Some(args.as_str())
            }
            _ => None,
        })
    }

    /// Drop tool calls whose args fail to parse, injecting a self-correction
    /// `tool_result` for each (the Reflexion/SWE-agent "feed errors back"
    /// pattern). The model learns its args were malformed and retries,
    /// instead of the call being silently dispatched with empty args (the old
    /// `serde_json::from_str(args).unwrap_or(json!({}))` swallow). Mirrors
    /// the PreToolUse deny path's `tool_result` injection.
    ///
    /// Empty/absent raw args are treated as well-formed (explicit no-args
    /// calls) to avoid false positives on tools that take no arguments.
    fn filter_malformed_tool_args(
        &self,
        calls: Vec<ToolCallRequest>,
        state: &mut LoopState,
        content: &[oneai_core::ContentBlock],
    ) -> Vec<ToolCallRequest> {
        let mut kept = Vec::with_capacity(calls.len());
        for call in calls {
            let well_formed = match Self::raw_args_for(content, &call.id) {
                None => true, // nothing to recheck — assume parse_decision accepted it
                Some(raw) if raw.trim().is_empty() => true, // explicit no-args
                Some(raw) => match self.parse_tool_args(raw) {
                    Ok(_) => true,
                    Err(err) => {
                        tracing::warn!(
                            tool = %call.name,
                            call_id = %call.id,
                            error = %err,
                            "malformed tool args — feeding back to model for self-correction"
                        );
                        state.conversation.add_message(Message::tool_result(
                            call.id.clone(),
                            format!(
                                "Error: malformed arguments for tool `{}` ({err}). \
                                 Please reissue the `{}` tool call with valid JSON arguments.",
                                call.name, call.name
                            ),
                        ));
                        false
                    }
                },
            };
            if well_formed {
                kept.push(call);
            }
        }
        kept
    }

    async fn execute_tool_calls(&self, calls: Vec<ToolCallRequest>) -> Result<Vec<ToolCallResult>> {
        // ─── Smart Tool Router ──────────────────────────────────────────────────
        // Intercept shell calls that are actually file operations and redirect them
        // to the appropriate specialized tool. This is a programmatic fallback that
        // works regardless of model intelligence — even if the model (GLM/Qwen)
        // ignores system prompt tool preference rules, we still route correctly.
        //
        // This addresses the "shell优先级过高" problem at the runtime level.
        // Pattern: "shell cat file.rs" → redirect to read_file
        // Pattern: "shell sed 's/old/new/' file" → redirect to edit_file
        // Pattern: "shell ls dir" → redirect to list_directory
        // Pattern: "shell grep pattern file" → redirect to grep
        // Pattern: "shell find . -name '*.rs'" → redirect to glob
        // Pattern: "shell mkdir dir" → redirect to shell (no mkdir tool, keep)
        let routed_calls: Vec<ToolCallRequest> = calls
            .into_iter()
            .map(|call| {
                if call.name == "shell" {
                    Self::route_shell_to_specialized(call)
                } else {
                    call
                }
            })
            .collect();

        let mut results = Vec::new();

        // Pre-check domain PermissionProfile for each call
        let domain_permission_checks: Vec<Option<PermissionAction>> = routed_calls
            .iter()
            .map(|call| {
                // Permission inheritance: a sub-agent whose `permission_pack`
                // is set (the parent's domain pack) consults it first, so it
                // inherits the parent's auto-approve / require-confirmation
                // policy (e.g. CodingPack auto-approves web_search/web_fetch
                // → a delegated Explore sub-agent's web calls don't prompt).
                // Falls back to the loop's own `domain_pack` (the parent loop
                // path), then None (bare agents → tool's own permission_level).
                if let Some(pack) = self.permission_pack.as_ref() {
                    Some(pack.resolve_permission(&call.name, &call.args))
                } else {
                    self.domain_pack
                        .as_ref()
                        .map(|dp| dp.resolve_permission(&call.name, &call.args))
                }
            })
            .collect();

        // Clone the tool Arcs (and per-call permission checks) out of the
        // read guard, then DROP the guard before any tool executes. A tool
        // whose side effect is to register/activate other tools
        // (self-extension, evolution-plan §3.4) may acquire the registry's
        // write lock during `execute()`; holding this read guard across
        // execute would deadlock against that write. The futures only need
        // the cloned `Arc<dyn Tool>` — never the guard itself.
        type ResolvedCall = (
            ToolCallRequest,
            Option<Arc<dyn Tool>>,
            Option<PermissionAction>,
        );
        let resolved: Vec<ResolvedCall> = {
            let tools_map = self.tools.read().await;
            routed_calls
                .into_iter()
                .zip(domain_permission_checks)
                .map(|(call, perm_check)| {
                    let tool_opt = tools_map.get(&call.name).cloned();
                    (call, tool_opt, perm_check)
                })
                .collect()
        };
        // `tools_map` read guard dropped here — before any execute() below.

        // #27 — capture the exposure resolver once for the per-call guard
        // below (cloning the `Arc` is cheap). `None` when no DomainPack is
        // loaded → `effective_exposure` falls back to `Tool::exposure()`.
        let exposure_resolver: Option<Arc<dyn oneai_core::traits::ExposureResolver>> =
            self.domain_pack.clone().map(|dp| {
                let r: Arc<dyn oneai_core::traits::ExposureResolver> = dp;
                r
            });

        let futures: Vec<_> = resolved
            .into_iter()
            .map(|(call, tool_opt, perm_check)| {
                let tool_name = call.name.clone();
                let call_id = call.id.clone();
                let args = call.args.clone();
                let interaction_gate = self.interaction_gate.clone();
                let exposure_resolver = exposure_resolver.clone();
                async move {
                    // Step 0 (#27): exposure guard — a model call naming a
                    // `Hidden` or `CodeModeOnly` tool is rejected. The schema
                    // filter already keeps these out of the model's tool list,
                    // so reaching here means a hallucinated/guessed name —
                    // defense-in-depth, the tool is never executed.
                    if let Some(ref tool) = tool_opt {
                        let e = oneai_core::traits::effective_exposure(
                            exposure_resolver.as_deref(),
                            tool.as_ref(),
                        );
                        if !e.is_model_dispatchable() {
                            return Ok(ToolCallResult {
                                call_id,
                                tool_name,
                                output: ToolOutput {
                                    success: false,
                                    content: String::new(),
                                    error: Some(format!(
                                        "tool '{}' is not model-dispatchable \
                                        (exposure={:?}) — it is not in the schema \
                                        and was not reached via tool_search",
                                        tool.name(),
                                        e,
                                    )),
                                    ..Default::default()
                                },
                            });
                        }
                    }
                    // Step 1: Check domain PermissionProfile (highest priority)
                    match perm_check {
                        Some(PermissionAction::Deny { reason }) => Ok(ToolCallResult {
                            call_id,
                            tool_name,
                            output: ToolOutput {
                                success: false,
                                content: String::new(),
                                error: Some(format!("Denied by domain policy: {}", reason)),
                                ..Default::default()
                            },
                        }),
                        Some(PermissionAction::AutoApprove) => {
                            // Domain says auto-approve — skip approval gate
                            match tool_opt {
                                Some(tool) => {
                                    let output = tool.execute(args).await?;
                                    Ok::<ToolCallResult, oneai_core::error::OneAIError>(
                                        ToolCallResult {
                                            call_id,
                                            tool_name,
                                            output,
                                        },
                                    )
                                }
                                None => {
                                    let err_msg = format!("Tool '{}' not found", tool_name);
                                    Ok(ToolCallResult {
                                        call_id,
                                        tool_name,
                                        output: ToolOutput {
                                            success: false,
                                            content: String::new(),
                                            error: Some(err_msg),
                                            ..Default::default()
                                        },
                                    })
                                }
                            }
                        }
                        Some(PermissionAction::RequireConfirmation) => {
                            // Domain says always require confirmation
                            match tool_opt {
                                Some(tool) => {
                                    let request = oneai_core::ApprovalRequest {
                                        tool_name: tool_name.clone(),
                                        args: args.clone(),
                                        risk_level: oneai_core::RiskLevel::High,
                                        permission_level: Some(oneai_core::PermissionLevel::Full),
                                        justification: format!(
                                            "Domain policy requires confirmation for '{}'",
                                            tool_name
                                        ),
                                    };
                                    Self::handle_approval(
                                        interaction_gate,
                                        request,
                                        tool,
                                        args,
                                        call_id,
                                        tool_name,
                                    )
                                    .await
                                }
                                None => {
                                    let err_msg = format!("Tool '{}' not found", tool_name);
                                    Ok(ToolCallResult {
                                        call_id,
                                        tool_name,
                                        output: ToolOutput {
                                            success: false,
                                            content: String::new(),
                                            error: Some(err_msg),
                                            ..Default::default()
                                        },
                                    })
                                }
                            }
                        }
                        Some(PermissionAction::UseDefaultPermission { level }) => {
                            // Domain provides a specific level — use it
                            match tool_opt {
                                Some(tool) => {
                                    if level == oneai_core::PermissionLevel::Full {
                                        let request = oneai_core::ApprovalRequest {
                                            tool_name: tool_name.clone(),
                                            args: args.clone(),
                                            risk_level: tool.risk_level(),
                                            permission_level: Some(level),
                                            justification: format!(
                                                "Full-permission tool '{}' requires approval",
                                                tool_name
                                            ),
                                        };
                                        Self::handle_approval(
                                            interaction_gate,
                                            request,
                                            tool,
                                            args,
                                            call_id,
                                            tool_name,
                                        )
                                        .await
                                    } else {
                                        let output = tool.execute(args).await?;
                                        Ok::<ToolCallResult, oneai_core::error::OneAIError>(
                                            ToolCallResult {
                                                call_id,
                                                tool_name,
                                                output,
                                            },
                                        )
                                    }
                                }
                                None => {
                                    let err_msg = format!("Tool '{}' not found", tool_name);
                                    Ok(ToolCallResult {
                                        call_id,
                                        tool_name,
                                        output: ToolOutput {
                                            success: false,
                                            content: String::new(),
                                            error: Some(err_msg),
                                            ..Default::default()
                                        },
                                    })
                                }
                            }
                        }
                        None => {
                            // No domain rule — fall back to tool's risk_level()
                            match tool_opt {
                                Some(tool) => {
                                    let perm_level = oneai_core::PermissionLevel::from_risk_level(
                                        tool.risk_level(),
                                    );
                                    if perm_level == oneai_core::PermissionLevel::Full {
                                        let request = oneai_core::ApprovalRequest {
                                            tool_name: tool_name.clone(),
                                            args: args.clone(),
                                            risk_level: tool.risk_level(),
                                            permission_level: Some(perm_level),
                                            justification: format!(
                                                "Full-permission tool '{}' requires approval",
                                                tool_name
                                            ),
                                        };
                                        Self::handle_approval(
                                            interaction_gate,
                                            request,
                                            tool,
                                            args,
                                            call_id,
                                            tool_name,
                                        )
                                        .await
                                    } else {
                                        let output = tool.execute(args).await?;
                                        Ok(ToolCallResult {
                                            call_id,
                                            tool_name,
                                            output,
                                        })
                                    }
                                }
                                None => {
                                    let err_msg = format!("Tool '{}' not found", tool_name);
                                    Ok(ToolCallResult {
                                        call_id,
                                        tool_name,
                                        output: ToolOutput {
                                            success: false,
                                            content: String::new(),
                                            error: Some(err_msg),
                                            ..Default::default()
                                        },
                                    })
                                }
                            }
                        }
                    }
                }
            })
            .collect();
        let outcomes = futures::future::join_all(futures).await;
        for outcome in outcomes {
            match outcome {
                Ok(result) => results.push(result),
                Err(e) => results.push(ToolCallResult {
                    call_id: String::new(),
                    tool_name: String::new(),
                    output: ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Tool execution error: {}", e)),
                        ..Default::default()
                    },
                }),
            }
        }
        Ok(results)
    }

    /// Smart Tool Router — intercept shell calls for file operations and
    /// redirect to specialized tools.
    ///
    /// This is a programmatic fallback that works regardless of model intelligence.
    /// When the model (especially GLM/Qwen) calls shell with commands like
    /// "cat file.rs" or "sed 's/old/new/' file.rs", this router detects the
    /// actual intent and redirects to read_file or edit_file respectively.
    ///
    /// Only redirects when the specialized tool exists in the tools_map.
    /// If the specialized tool doesn't exist, the original shell call is kept.
    ///
    /// Inspired by Claude Code's approach where specialized tools are always
    /// preferred, and SWE-agent's Agent-Computer Interface pattern where
    /// raw shell access is constrained to purpose-built commands.
    fn route_shell_to_specialized(call: ToolCallRequest) -> ToolCallRequest {
        // Only intercept shell calls
        if call.name != "shell" {
            return call;
        }

        // Extract the command string from args
        let command = call
            .args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if command.is_empty() {
            return call;
        }

        // Parse the first word (the actual command) and its arguments
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return call;
        }

        let cmd = parts[0];
        let cmd_args: Vec<&str> = if parts.len() > 1 {
            parts[1..].to_vec()
        } else {
            Vec::new()
        };

        // ─── Redirect patterns ──────────────────────────────────────────────────
        // Map common shell commands to their specialized tool equivalents.

        match cmd {
            // cat → read_file
            "cat" | "head" | "tail" | "less" | "more" | "bat" => {
                let file_path = cmd_args.iter().find(|a| !a.starts_with('-')).unwrap_or(&"");
                if !file_path.is_empty() {
                    return ToolCallRequest {
                        id: call.id,
                        name: "read_file".to_string(),
                        args: serde_json::json!({
                            "path": file_path,
                        }),
                    };
                }
            }

            // sed → keep as shell (too complex to parse reliably)
            "sed" => {
                // sed patterns are too varied to reliably parse into edit_file format
            }

            // ls → list_directory
            "ls" | "dir" => {
                let dir_path = cmd_args
                    .iter()
                    .find(|a| !a.starts_with('-'))
                    .unwrap_or(&".");
                return ToolCallRequest {
                    id: call.id,
                    name: "list_directory".to_string(),
                    args: serde_json::json!({
                        "path": dir_path,
                    }),
                };
            }

            // grep (shell grep) → grep tool
            "grep" | "rg" | "ag" | "ack" => {
                // Parse: grep [options] pattern [path]
                let non_option_args: Vec<&str> = cmd_args
                    .iter()
                    .filter(|a| !a.starts_with('-'))
                    .copied()
                    .collect();
                if !non_option_args.is_empty() {
                    let pattern = non_option_args[0];
                    let path = non_option_args.get(1).copied().unwrap_or(".");
                    return ToolCallRequest {
                        id: call.id,
                        name: "grep".to_string(),
                        args: serde_json::json!({
                            "pattern": pattern,
                            "path": path,
                        }),
                    };
                }
            }

            // find → glob
            "find" | "locate" => {
                let path = cmd_args
                    .iter()
                    .find(|a| !a.starts_with('-'))
                    .unwrap_or(&".");
                let name_idx = cmd_args
                    .iter()
                    .position(|a| *a == "-name" || *a == "-iname");
                if let Some(idx) = name_idx {
                    if idx + 1 < cmd_args.len() {
                        let pattern = cmd_args[idx + 1].replace("\"", "");
                        return ToolCallRequest {
                            id: call.id,
                            name: "glob".to_string(),
                            args: serde_json::json!({
                                "pattern": pattern,
                                "path": path,
                            }),
                        };
                    }
                }
                // find without -name → list_directory
                return ToolCallRequest {
                    id: call.id,
                    name: "list_directory".to_string(),
                    args: serde_json::json!({
                        "path": path,
                    }),
                };
            }

            // pwd → environment
            "pwd" | "whoami" | "uname" | "which" => {
                return ToolCallRequest {
                    id: call.id,
                    name: "environment".to_string(),
                    args: serde_json::json!({}),
                };
            }

            // echo (simple, no redirect) → environment-like
            "echo" => {
                // If it has > or >>, it's a write operation → keep as shell
                if cmd_args.iter().any(|a| a.contains(">") || a.contains(">>")) {
                    return call;
                }
            }

            // tree → list_directory
            "tree" => {
                let dir_path = cmd_args
                    .iter()
                    .find(|a| !a.starts_with('-'))
                    .unwrap_or(&".");
                return ToolCallRequest {
                    id: call.id,
                    name: "list_directory".to_string(),
                    args: serde_json::json!({
                        "path": dir_path,
                    }),
                };
            }

            // file → read_file
            "file" => {
                let file_path = cmd_args.iter().find(|a| !a.starts_with('-')).unwrap_or(&"");
                if !file_path.is_empty() {
                    return ToolCallRequest {
                        id: call.id,
                        name: "read_file".to_string(),
                        args: serde_json::json!({
                            "path": file_path,
                        }),
                    };
                }
            }

            // curl/wget → web_fetch (for simple URL fetches only)
            "curl" | "wget" => {
                let url_arg = cmd_args
                    .iter()
                    .find(|a| a.starts_with("http://") || a.starts_with("https://"));
                if let Some(url) = url_arg {
                    // Only redirect simple URL fetches (not POST/PUT/etc.)
                    if !cmd_args.iter().any(|a| {
                        *a == "-X" || *a == "-d" || *a == "--data" || *a == "-F" || *a == "-T"
                    }) {
                        return ToolCallRequest {
                            id: call.id,
                            name: "web_fetch".to_string(),
                            args: serde_json::json!({
                                "url": url,
                            }),
                        };
                    }
                }
            }

            // date → environment
            "date" => {
                return ToolCallRequest {
                    id: call.id,
                    name: "environment".to_string(),
                    args: serde_json::json!({}),
                };
            }

            _ => {
                // Unknown command — keep as shell (git, cargo, npm, python, etc.)
            }
        }

        // No redirect matched — keep original shell call
        call
    }

    // ─── Cadence-fired Reflect sub-agent (Phase 2.1 Stage A) ────────────────

    /// Build the bounded digest the reflect sub-agent reviews. Compact by
    /// design — keeps the sub-agent's own context small so it cheaply
    /// distills a few durable learnings rather than re-reading the whole
    /// transcript.
    fn build_reflection_review_task(&self, state: &LoopState) -> String {
        let mut out = String::new();
        out.push_str(
            "[Background reflection — distill DURABLE learnings; ignore \
             transient / environment failures. Do not converse with the user.]\
             \n\n",
        );
        out.push_str(&format!("Original task: {}\n", state.original_task));
        out.push_str(&format!("Iterations so far: {}\n", state.iterations));
        if let Some(answer) = &state.final_answer {
            out.push_str(&format!("Final answer delivered: {}\n", answer));
        }
        // Last ≤8 messages as a compact transcript (role + a short text
        // excerpt + any tool_call name). Bounded so the reflect sub-agent's
        // context stays small.
        let recent = state
            .conversation
            .messages
            .iter()
            .rev()
            .take(8)
            .collect::<Vec<_>>();
        if !recent.is_empty() {
            out.push_str("\nRecent activity (last ≤8 messages):\n");
            for m in recent.into_iter().rev() {
                let role = format!("{:?}", m.role).to_lowercase();
                let excerpt = m.text_content().chars().take(200).collect::<String>();
                let tool_calls: Vec<String> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        oneai_core::ContentBlock::ToolCall { name, .. } => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
                out.push_str(&format!("- [{role}] {excerpt}"));
                if !tool_calls.is_empty() {
                    out.push_str(&format!("  (tools: {})", tool_calls.join(", ")));
                }
                out.push('\n');
            }
        }
        out
    }

    /// Spawn a `Reflect` sub-agent with the parent provider + a memory-only
    /// tool whitelist, run it, and surface its summary via
    /// `on_reflection`. The summary is deliberately NOT injected into the
    /// parent conversation — the whole point of sub-agent delegation is to
    /// keep the parent context clean. Failures are logged and swallowed: a
    /// background reflection must never break the parent loop.
    ///
    /// The sub-agent inherits the parent provider (warm) via
    /// `DefaultSubAgentFactory`. Its own `AgentLoop` is built with
    /// `SubAgentFactoryNone`, so the `delegate` meta-tool is auto-stripped
    /// from its schema (no recursive nudge) and `hard_max_iterations=16`
    /// bounds it.
    async fn maybe_run_reflection(
        &self,
        state: &mut LoopState,
        observer: &dyn AgentLoopObserver,
        trigger: ReflectionTrigger,
    ) {
        // Footprint guard: don't fire if neither the memory tools nor the
        // `skill_manage` tool the reflect sub-agent needs are registered. The
        // reviewer persists durable learnings via *either* path — memory facts
        // (Stage A) or skill-library curation (Stage B). Fire only when at
        // least one path is available; if both are absent, skip entirely (the
        // strict-whitelist factory would hand it zero tools).
        let memory_tools = [
            "memory_search",
            "core_memory_edit",
            "archival_memory_insert",
        ];
        {
            let tools = self.tools.read().await;
            let memory_ok = memory_tools.iter().all(|n| tools.contains_key::<str>(*n));
            let skill_manage_ok = tools.contains_key::<str>("skill_manage");
            if !memory_ok && !skill_manage_ok {
                tracing::info!(
                    trigger = ?trigger,
                    "Skipping reflect sub-agent: neither memory tools nor skill_manage registered",
                );
                return;
            }
        }

        state.reflections_fired += 1;
        // Stage C — persist a `ReflectionFired` event to the working-state
        // log so a resumed task hydrates this cumulative count + baseline.
        // Best-effort: a persistence failure must never break the parent
        // loop (mirrors the swallow-failures contract of this fn).
        let cum_iter = state.cadence_baseline + state.iterations as u64;
        if let Some(store) = self.working_state_store.clone() {
            if let Some(task_id) = state.task_id.as_ref().cloned() {
                let session_id = state.session_id.clone();
                if let Err(e) = store
                    .append_event(
                        &task_id,
                        &session_id,
                        None,
                        oneai_core::TaskEventType::ReflectionFired,
                        oneai_core::TaskEventPayload::ReflectionFired {
                            iteration: cum_iter,
                        },
                    )
                    .await
                {
                    tracing::warn!("Failed to persist ReflectionFired event (swallowed): {e}");
                }
            }
        }
        let review_task = self.build_reflection_review_task(state);
        tracing::info!(
            trigger = ?trigger,
            iteration = state.iterations,
            "Spawning cadence-fired Reflect sub-agent"
        );

        // Budget: a small slice — the reviewer reads a digest + writes a few
        // memory facts. Generous enough for one inference + a couple tool
        // calls, tight enough to stay cheap.
        let budget = TokenBudget::new(2000);
        match self
            .sub_agent_factory
            .create(crate::sub_agent::SubAgentKind::Reflect, budget)
            .await
        {
            Ok(sub_agent) => match sub_agent.run(&review_task).await {
                Ok(summary) => {
                    tracing::info!(
                        summary = %summary.summary,
                        "Reflect sub-agent finished ({} key findings, budget_exceeded={})",
                        summary.key_findings.len(),
                        summary.budget_exceeded
                    );
                    observer.on_reflection(&summary.summary);
                }
                Err(e) => {
                    tracing::warn!("Reflect sub-agent run failed (swallowed): {e}");
                }
            },
            Err(e) => {
                // Factory unavailable (e.g. SubAgentFactoryNone — no provider
                // / factory wired). Not an error for the parent loop.
                tracing::info!("Reflect sub-agent not spawned (factory unavailable): {e}");
            }
        }
    }

    /// Schedule a batch of delegations with dependency-aware concurrency,
    /// bounded live concurrency, optional global budget, progress
    /// forwarding, and parent-interrupt propagation.
    ///
    /// Implements a wave-based (Kahn's algorithm) scheduler over the
    /// `DelegateTask` DAG:
    ///
    /// - Each iteration selects the *wave* = all tasks whose `depends_on` ids
    ///   are already in `completed`. These run **concurrently** via a tokio
    ///   `JoinSet` — independent sub-agents execute in parallel.
    /// - **Concurrency cap (Opt 2)**: a `Semaphore(max_concurrent)` gates the
    ///   number of sub-agents running LLM inference at once within the wave.
    /// - A task with unsatisfied dependencies is held back until its deps
    ///   finish, so dependent tasks run **serially** after their upstream.
    /// - Before a dependent task starts, its `task` text is prefixed with the
    ///   summaries of its dependencies (one `[Dependency '<id>' result]: …`
    ///   block each), so the dependent sub-agent receives upstream results
    ///   without the model re-stating them.
    /// - **Progress forwarding (Opt 1)**: each spawned sub-agent runs with a
    ///   `ForwardingObserver` that ships its iteration/tool/usage events over
    ///   an mpsc channel; the parent drains the channel (via `select!` while
    ///   awaiting the wave) and calls `observer.on_delegate_progress` — so
    ///   the parent UI is not blind during a long delegation.
    /// - **Interrupt propagation (Opt 1)**: each sub-agent gets a child
    ///   `CancellationToken` of the parent's; a parent interrupt lands at the
    ///   sub-agent's next iteration boundary.
    /// - **Partial failure (Opt 1)**: a single sub-agent failure or panic no
    ///   longer aborts the whole batch — it records a `completed:false`
    ///   failure summary so dependent tasks proceed (prefixed with the
    ///   upstream's failure note) and siblings complete. Cycles (no task can
    ///   make progress) still surface as an error.
    ///
    /// Returns summaries in **input order** (the order tasks appeared in the
    /// turn), regardless of completion order — this keeps the fed-back results
    /// deterministic and matches the model's mental model of the batch.
    async fn spawn_sub_agents_batch(
        &self,
        tasks: Vec<DelegateTask>,
        observer: &dyn AgentLoopObserver,
    ) -> Result<Vec<SubAgentSummary>> {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        use tokio::sync::{mpsc, Semaphore};
        use tokio::task::JoinSet;

        if tasks.is_empty() {
            return Ok(Vec::new());
        }

        // Preserve input order for deterministic result feed-back.
        let order: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();

        // Opt 2: concurrency cap + optional global budget pool.
        let sem = Arc::new(Semaphore::new(self.delegation_policy.max_concurrent));
        let pool: Option<Arc<AtomicU32>> = self
            .delegation_policy
            .budget_pool
            .as_ref()
            .map(|b| Arc::new(AtomicU32::new(b.total)));

        // Opt 1: progress channel — spawned ('static) sub-agent tasks ship
        // events here; the parent drains them onto the borrowed `observer`.
        let (tx, mut rx) = mpsc::unbounded_channel::<(
            String,
            crate::sub_agent::SubAgentKind,
            DelegateProgressEvent,
        )>();
        // Keep tx alive for the whole batch so rx.recv() arm stays armed even
        // between waves (sub-agents only send while running, but the select
        // needs a live sender reference model). Cloned per-task below.
        let _tx_keepalive = tx.clone();

        // Pending tasks keyed by id (clone-able for the spawned closure).
        let mut pending: HashMap<String, DelegateTask> =
            tasks.into_iter().map(|t| (t.id.clone(), t)).collect();

        // id → completed summary (success OR failure — failures recorded so
        // dependents proceed rather than tripping the cycle guard).
        let mut completed: HashMap<String, SubAgentSummary> = HashMap::new();

        while !pending.is_empty() {
            // Build the current wave: tasks whose every dependency is done.
            let wave_ids: Vec<String> = pending
                .iter()
                .filter(|(_, t)| t.depends_on.iter().all(|dep| completed.contains_key(dep)))
                .map(|(id, _)| id.clone())
                .collect();

            if wave_ids.is_empty() {
                // No task can make progress → the remaining deps form a cycle.
                let remaining: Vec<String> = pending.keys().cloned().collect();
                return Err(oneai_core::error::OneAIError::Agent(format!(
                    "Delegate batch has a dependency cycle among tasks: {}",
                    remaining.join(", ")
                )));
            }

            tracing::info!(
                "Delegate batch: running wave of {} task(s) in parallel (max_concurrent={}): [{}]",
                wave_ids.len(),
                self.delegation_policy.max_concurrent,
                wave_ids.join(", ")
            );

            let mut join_set: JoinSet<(
                String,
                crate::sub_agent::SubAgentKind,
                std::result::Result<SubAgentSummary, oneai_core::error::OneAIError>,
            )> = JoinSet::new();

            for id in wave_ids {
                let task = pending.remove(&id).expect("wave id present in pending");
                // Prepend dependency summaries to the task text so the
                // dependent sub-agent receives upstream results (or failure
                // notes for upstream that broke).
                let mut augmented_task = String::new();
                for dep in &task.depends_on {
                    if let Some(dep_summary) = completed.get(dep) {
                        augmented_task.push_str(&format!(
                            "[Dependency '{}' result]: {}\n",
                            dep, dep_summary.summary
                        ));
                        if !dep_summary.key_findings.is_empty() {
                            augmented_task.push_str(&format!(
                                "  Key findings: {}\n",
                                dep_summary.key_findings.join("; ")
                            ));
                        }
                        augmented_task.push('\n');
                    }
                }
                if !augmented_task.is_empty() {
                    augmented_task.push_str("Your task: ");
                }
                augmented_task.push_str(&task.task);

                // Opt 3: carry specialization into the factory.
                let spec = crate::sub_agent::DelegationSpec {
                    system_prompt: task.system_prompt_override.clone(),
                    tools: task.tools_override.clone(),
                    seed_messages: task.seed_messages.clone(),
                };
                let agent_type = task.agent_type.clone();
                let budget = task.budget.clone();
                let factory = self.sub_agent_factory.clone();
                let task_id = id.clone();
                let kind = task.agent_type.clone();
                let tx2 = tx.clone();
                let sem2 = sem.clone();
                let pool2 = pool.clone();
                // Opt 1: child cancellation token — parent interrupt propagates.
                // We pass the parent's own token; the SubAgentWrapper watcher
                // awaits it and fires the sub-agent loop's own token at the
                // next iteration boundary.
                let child_cancel = self.cancel_token.clone();

                join_set.spawn(async move {
                    let inner = async {
                        // Opt 2: concurrency cap.
                        let _permit = match sem2.acquire_owned().await {
                            Ok(p) => p,
                            Err(e) => {
                                return Err(oneai_core::error::OneAIError::Agent(format!(
                                    "Delegate semaphore closed: {e}"
                                )));
                            }
                        };
                        // Opt 2: global budget pool gate — reserve this task's
                        // allocated budget upfront; if the pool can't cover it,
                        // short-circuit with an exhausted summary (so a
                        // too-small pool caps how many sub-agents actually run).
                        // Reservation is conservative (budget cap, not actual
                        // usage) — no post-completion refund, keeping the
                        // accounting simple and the cap a hard sum-of-budgets.
                        if let Some(p) = &pool2 {
                            let need = budget.total;
                            let prev = p.fetch_sub(need, Ordering::Relaxed);
                            if prev < need {
                                // Underflow: pool couldn't cover this task.
                                p.store(0, Ordering::Relaxed);
                                return Ok(failure_summary(
                                    kind.clone(),
                                    "[budget pool exhausted]",
                                ));
                            }
                        }
                        let forwarder = ForwardingObserver {
                            delegate_id: task_id.clone(),
                            kind: kind.clone(),
                            tx: tx2,
                            turn_id: String::new(),
                            bus: None,
                        };
                        let sub_agent = factory
                            .create_with_spec(agent_type.clone(), budget, spec)
                            .await?;
                        let summary = sub_agent
                            .run_with_observer(
                                &augmented_task,
                                Some(&forwarder),
                                Some(child_cancel),
                            )
                            .await?;
                        Ok(summary)
                    }
                    .await;
                    (task_id, kind, inner)
                });
            }

            // Await the wave while concurrently forwarding progress to the
            // parent observer (Opt 1). The `select!` drains the channel
            // between join completions so the UI sees live sub-agent events.
            loop {
                tokio::select! {
                    jr = join_set.join_next() => match jr {
                        None => break,
                        Some(Ok((id, _kind, Ok(summary)))) => {
                            // Budget pool is reserved upfront (pre-run), so no
                            // post-completion decrement here.
                            completed.insert(id, summary);
                        }
                        Some(Ok((id, kind, Err(e)))) => {
                            // Opt 1 partial-failure: record a failure summary
                            // so dependents proceed (prefixed with the note)
                            // instead of aborting the whole batch.
                            tracing::warn!("Delegate sub-agent '{}' failed (recorded, batch continues): {e}", id);
                            completed.insert(id, failure_summary(kind, &format!("[failed: {e}]")));
                        }
                        Some(Err(join_err)) => {
                            // A spawned task panicked or was runtime-cancelled
                            // (not a parent-interrupt — that surfaces as an
                            // `Ok(Err(..))` via the sub-agent's cancel check).
                            // Rare; surface it rather than guess which id broke.
                            tracing::error!("Delegate sub-agent task panicked/cancelled: {join_err}");
                            return Err(oneai_core::error::OneAIError::Agent(format!(
                                "Delegate sub-agent task panicked or was cancelled: {join_err}"
                            )));
                        }
                    },
                    Some((id, kind, ev)) = rx.recv() => {
                        observer.on_delegate_progress(&id, &kind, &ev);
                    }
                }
            }

            // Drain any progress events buffered while the last tasks finished.
            while let Ok((id, kind, ev)) = rx.try_recv() {
                observer.on_delegate_progress(&id, &kind, &ev);
            }
            tracing::info!(
                "delegate wave complete; {} task(s) resolved so far",
                completed.len()
            );
        }

        // Emit in input order.
        let summaries: Vec<SubAgentSummary> = order
            .iter()
            .filter_map(|id| completed.get(id).cloned())
            .collect();
        tracing::info!(
            batch_size = order.len(),
            completed = summaries.iter().filter(|s| s.completed).count(),
            "delegate batch complete"
        );
        Ok(summaries)
    }

    /// Apply a paradigm switch — produces real, observable behavior changes.
    ///
    /// This addresses the "范式切换语义空洞" gap. Previously, `run_paradigm()`
    /// just returned a formatted string like "Plan paradigm activated" — no
    /// actual behavior change. Now, paradigm switching does three things:
    ///
    /// 1. **Replaces the system prompt** in the conversation — removes the
    ///    old system message and adds a paradigm-specific one.
    /// 2. **Stores ParadigmConfig** in LoopState — `build_tool_definitions()`
    ///    uses the config's tool_filter to only send relevant tools to the model.
    /// 3. **Injects a decision hint** — a brief system message telling the model
    ///    what kind of decisions to make in this paradigm.
    ///
    /// Inspired by Aider's Architect/Editor dual-model pattern where each
    /// "role" has its own prompt and tool set. OneAI extends this to 4 paradigms.
    fn apply_paradigm_switch(&self, paradigm: ParadigmKind, state: &mut LoopState) -> String {
        let config = ParadigmConfig::for_paradigm(paradigm);

        // Cache-stable system prompt + paradigm suffixation (Phase 1.1).
        //
        // The durable system layer has two parts:
        //   1. **Stable prefix** — the first system message, built once at
        //      session start as `build_system_prompt() + runtime_context_block()`
        //      (agent identity, `{{TOOL_PREFERENCE_RULES}}`, current date,
        //      web-search nudge). Byte-stable for the session so the
        //      provider's prompt-prefix cache survives across iterations and
        //      paradigm switches — switching paradigm must NOT rewrite it.
        //   2. **Paradigm tail** — paradigm-specific system messages added by
        //      this method (the paradigm prompt + decision hint), tagged with
        //      metadata[`PARADIGM_TAIL_KEY`] so they can be removed surgically.
        //
        // Previously this method did `retain(|m| m.role != Role::System)`,
        // nuking the ENTIRE durable system layer on every switch. That
        // (a) invalidated the prompt-prefix cache each switch — cost doubled
        // on the very next iteration — and (b) dropped `runtime_context_block`
        // (the date + web-search guidance) for the rest of the session, a
        // correctness bug. Now we remove only the tagged paradigm tail,
        // preserving the stable prefix and any other legitimately-injected
        // system messages (feedback retry prompts, etc.).
        const PARADIGM_TAIL_KEY: &str = "paradigm_tail";
        state
            .conversation
            .messages
            .retain(|m| !(m.role == Role::System && m.metadata.contains_key(PARADIGM_TAIL_KEY)));

        // Append the new (tagged) paradigm tail.
        let mut prompt_msg = Message::system(&config.system_prompt);
        prompt_msg
            .metadata
            .insert(PARADIGM_TAIL_KEY.to_string(), "1".to_string());
        state.conversation.add_message(prompt_msg);

        // Decision hint — also a tagged tail message so a later switch
        // replaces it along with the prompt.
        if !config.decision_hint.is_empty() {
            let mut hint_msg =
                Message::system(format!("[Paradigm switch]: {}", config.decision_hint));
            hint_msg
                .metadata
                .insert(PARADIGM_TAIL_KEY.to_string(), "1".to_string());
            state.conversation.add_message(hint_msg);
        }

        // Store ParadigmConfig for tool filtering
        state.active_paradigm = paradigm;
        state.active_paradigm_config = Some(config.clone());

        // Return a concise summary for the loop
        format!(
            "{} paradigm activated — paradigm tail swapped, tools filtered to: [{}]",
            paradigm_name(&paradigm),
            config.tool_filter.join(", ")
        )
    }

    /// Activate the paradigm a frontend forced via `Directive::SwitchParadigm`.
    ///
    /// The directive (handled off the engine thread by the bus directive pump)
    /// writes the chosen paradigm to `conversation.metadata["active_paradigm"]`
    /// so it survives across turns (sticky until the frontend changes it again,
    /// unlike a model-driven mid-turn switch which is per-turn). This method,
    /// called at the start of `run_with_conversation`, materializes that choice
    /// into the live `LoopState`: swaps the paradigm tail system messages +
    /// sets `active_paradigm_config` so `build_tool_definitions_for_paradigm`
    /// applies the paradigm's tool filter. Idempotent — `apply_paradigm_switch`
    /// removes any prior tagged tail before appending.
    ///
    /// Silent (no `on_paradigm_switch` observer callback): the bus directive
    /// pump already emitted `EngineYield::ParadigmSwitch` when the directive
    /// arrived, so firing the observer here would duplicate the yield. The
    /// paradigm is confirmed to frontends by the next `IterationStart` yield.
    fn activate_forced_paradigm(&self, state: &mut LoopState) -> Option<ParadigmKind> {
        let raw = state.conversation.metadata.get("active_paradigm")?;
        let paradigm = paradigm_from_metadata(raw)?;
        self.apply_paradigm_switch(paradigm, state);
        Some(paradigm)
    }

    /// Apply paradigm switch — with optional StateGraph execution.
    ///
    /// When a DomainPack has a predefined StateGraph matching the paradigm
    /// (e.g., "react-loop" for ReAct), this method first applies the
    /// semantic paradigm switch, then attempts to execute the StateGraph.
    /// If the StateGraph executes successfully, its output is injected
    /// into the conversation as an assistant message.
    ///
    /// If no StateGraph is found, or execution fails, this falls back
    /// to the purely semantic paradigm switch (apply_paradigm_switch).
    async fn apply_paradigm_switch_with_graph(
        &self,
        paradigm: ParadigmKind,
        state: &mut LoopState,
    ) -> Result<String> {
        // First, apply the semantic paradigm switch (always happens)
        let base_result = self.apply_paradigm_switch(paradigm, state);

        // Look for a predefined StateGraph for this paradigm in the DomainPack
        let graph_key = match paradigm {
            ParadigmKind::ReAct => "react-loop",
            ParadigmKind::Plan => "plan-workflow",
            ParadigmKind::Reflect => "reflect-workflow",
            ParadigmKind::Explore => "explore-workflow",
        };

        let graph = self
            .domain_pack
            .as_ref()
            .and_then(|dp| dp.get_state_graph(graph_key))
            .cloned();

        if let Some(graph) = graph {
            tracing::info!(
                "Found predefined StateGraph '{}' for paradigm {}. Attempting execution.",
                graph.name,
                paradigm_name(&paradigm)
            );

            // Build a StateGraphExecutor from the AgentLoop's dependencies
            // Use the AgentLoop's SubAgentFactory as the DelegateFactory bridge
            let delegate_factory: Arc<dyn oneai_workflow::DelegateFactory> = Arc::new(
                crate::sub_agent::SubAgentDelegateFactory::new(self.sub_agent_factory.clone()),
            );

            // Use the FULL bridge (AgentLoopGraphActionExecutor) — the same one
            // `run_with_state_graph` uses — so an inline paradigm switch runs
            // its graph through the loop's own hooks / domain permissions /
            // OutputParser / tool-definition builder. This removes the prior
            // consistency hole where the inline path used the stripped-down
            // DirectProviderActionExecutor (no hooks, no domain decorators,
            // no OutputParser). The executor only holds cloned Arcs (read-only
            // infrastructure); results are fed back into LoopState below.
            let action_executor: Arc<dyn oneai_workflow::GraphActionExecutor> =
                Arc::new(AgentLoopGraphActionExecutor {
                    provider: self.provider.clone(),
                    tools: self.tools.clone(),
                    parser: self.parser.clone(),
                    interaction_gate: self.interaction_gate.clone(),
                    domain_pack: self.domain_pack.clone(),
                    hook_registry: self.hook_registry.clone(),
                    recovery_manager: self.recovery_manager.clone(),
                    config: self.config.clone(),
                });
            let executor = oneai_workflow::StateGraphExecutor::new(
                action_executor,
                delegate_factory,
                self.interaction_gate.clone(),
                self.config.hard_max_iterations.unwrap_or(50),
            );

            // Build initial state from the current conversation
            let mut initial_state = oneai_workflow::GraphState::new();
            initial_state.conversation = state.conversation.clone();
            // Copy relevant variables from LoopState into graph state
            initial_state
                .variables
                .insert("task".to_string(), state.original_task.clone());

            let graph_result = executor.execute(&graph, initial_state).await;

            match graph_result {
                Ok(result) => {
                    if result.completed {
                        tracing::info!(
                            "StateGraph '{}' completed successfully after {} iterations. Terminal: {}",
                            result.name, result.iterations,
                            result.terminal_node.as_deref().unwrap_or("none")
                        );
                        // Inject the StateGraph's final output into the loop conversation
                        if let Some(output) = &result.final_state.last_result {
                            state.conversation.add_message(Message::assistant(format!(
                                "[StateGraph {} result]: {}",
                                result.name, output
                            )));
                        }
                        // Merge any new variables from the graph state back
                        for (key, value) in &result.final_state.variables {
                            if !key.starts_with("_") {
                                // Skip internal variables
                                state
                                    .global_state
                                    .context
                                    .insert(key.clone(), value.clone());
                            }
                        }
                        return Ok(format!(
                            "{} paradigm + StateGraph '{}' executed ({} iterations). {}",
                            paradigm_name(&paradigm),
                            result.name,
                            result.iterations,
                            base_result
                        ));
                    } else {
                        tracing::warn!(
                            "StateGraph '{}' did not reach a terminal node after {} iterations.",
                            result.name,
                            result.iterations
                        );
                        // Still useful — inject partial results
                        if let Some(output) = &result.final_state.last_result {
                            state.conversation.add_message(Message::assistant(format!(
                                "[StateGraph {} partial]: {}",
                                result.name, output
                            )));
                        }
                        return Ok(format!(
                            "{} paradigm + StateGraph '{}' incomplete ({} iterations). {}",
                            paradigm_name(&paradigm),
                            result.name,
                            result.iterations,
                            base_result
                        ));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "StateGraph '{}' execution failed: {}. Falling back to semantic paradigm switch.",
                        graph.name, e
                    );
                    // Fall back — the semantic switch was already applied
                    return Ok(format!(
                        "{} paradigm activated (StateGraph '{}' failed: {}). {}",
                        paradigm_name(&paradigm),
                        graph.name,
                        e,
                        base_result
                    ));
                }
            }
        }

        // No predefined StateGraph — semantic switch only (already applied)
        Ok(base_result)
    }

    /// Run a streaming iteration — uses `provider.infer_stream()` and
    /// emits text chunks via the observer's `on_stream_chunk()` for typewriter effect.
    ///
    /// Collects the full stream, then returns the assembled InferenceResponse.
    async fn run_streaming_iteration_async(
        &self,
        request: &InferenceRequest,
        observer: &dyn AgentLoopObserver,
    ) -> Result<InferenceResponse> {
        use futures::StreamExt;

        // Establish the stream (or abort immediately if already cancelled).
        // The call to `infer_stream` itself is bounded by the idle timeout:
        // without this, a provider/proxy that accepts the connection but
        // never sends the first byte (or the response headers) hangs here
        // FOREVER — the per-chunk `stream.next()` timeout below can't fire
        // because we never get a stream to iterate. That manifests as a
        // multi-minute "stuck" with no error and no tokens (observed 2–20
        // min stalls in the macOS app). Bounding it surfaces a retryable
        // error instead. STREAM_IDLE_TIMEOUT is reused so the budget is the
        // same as for mid-stream stalls.
        const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        let mut stream = tokio::select! {
            biased;
            _ = self.cancel_token.cancelled() => {
                return Err(oneai_core::error::OneAIError::Other(
                    "Agent interrupted during inference.".to_string(),
                ));
            }
            res = tokio::time::timeout(STREAM_IDLE_TIMEOUT, self.provider.infer_stream(request.clone())) => match res {
                Ok(s) => s?,
                Err(_elapsed) => {
                    tracing::warn!(
                        "infer_stream did not start within {}s — provider/proxy stalled on the request; aborting with retryable error",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    );
                    return Err(oneai_core::error::OneAIError::Other(format!(
                        "模型响应超时({}秒未开始),可能为网络/服务端中断,请重试。",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    )));
                }
            },
        };

        // Use the IncrementalStreamParser for proper incremental parsing
        let mut parser = IncrementalStreamParser::new();
        let mut usage = oneai_core::TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            ..Default::default()
        };
        let mut model = String::new();

        loop {
            // Check for cancellation between chunks so a mid-stream interrupt
            // aborts promptly instead of draining the whole stream. Also guard
            // against a *stalled* stream: if the provider holds the connection
            // open without sending a chunk or closing (common under provider
            // load / proxy / network blips), `stream.next()` would otherwise
            // block forever — the UI would show partial text + a blinking cursor
            // and appear frozen with no recovery short of the Stop button. The
            // idle timeout aborts with a retryable error so the user keeps the
            // partial output and can retry. (Per-chunk timeout: each received
            // chunk resets the timer, so slow-but-progressing streams survive.)
            const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
            let chunk = tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => break,
                // No data for the idle window → stream stalled. Surface as a
                // retryable error (the partial text already streamed stays in
                // the UI bubble; on_complete is NOT emitted, so the VM's error
                // path attaches a retry affordance).
                res = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next()) => match res {
                    Ok(Some(c)) => c,
                    Ok(None) => break,            // stream closed cleanly
                    Err(_elapsed) => {
                        tracing::warn!(
                            "stream idle timeout ({}s) — provider stalled mid-stream; \
                             aborting with retryable error",
                            STREAM_IDLE_TIMEOUT.as_secs()
                        );
                        return Err(oneai_core::error::OneAIError::Other(format!(
                            "模型流式输出停滞({}秒无数据),可能为网络/服务端中断,请重试。",
                            STREAM_IDLE_TIMEOUT.as_secs()
                        )));
                    }
                },
            };
            // Save chunk metadata before processing
            let is_final = chunk.is_final;
            let chunk_usage = chunk.usage.clone();
            let chunk_model = chunk.model.clone();

            // Process each chunk through the IncrementalStreamParser
            let events = parser.process_chunk(chunk);

            // Handle stream events → notify observer
            for event in events {
                match event {
                    crate::streaming::StreamEvent::TextFragment { text } => {
                        observer.on_stream_chunk(&text);
                    }
                    crate::streaming::StreamEvent::ThinkingFragment { text } => {
                        observer.on_thinking(&text);
                    }
                    crate::streaming::StreamEvent::ToolIntentDetected { .. } => {
                        // Tool intent detected mid-stream. We do NOT surface
                        // this to the observer: emitting it as a stream chunk
                        // ("▸ preparing {tool}…") polluted the assistant's
                        // answer text on clients that fold stream chunks into
                        // the bubble (the macOS app rendered "preparing 工具"
                        // as body text). The fully-assembled call arrives
                        // moments later via `ToolCallComplete` → `on_tool_calls`,
                        // which is what the UI's tool card renders. The intent
                        // hint was a TUI-only nicety; dropping it keeps the
                        // answer text clean on every port.
                    }
                    crate::streaming::StreamEvent::ToolCallComplete {
                        call_id,
                        tool_name,
                        args,
                    } => {
                        // Tool call is fully assembled — notify observer with complete args
                        observer.on_tool_calls(&[ToolCallRequest {
                            id: call_id,
                            name: tool_name,
                            args: serde_json::from_str(&args)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        }]);
                    }
                    crate::streaming::StreamEvent::StreamComplete { .. } => {
                        // Stream is done — parser has assembled all content
                    }
                }
            }

            // Check for final chunk with usage
            if is_final {
                if let Some(usage_data) = chunk_usage {
                    usage = usage_data;
                }
                if let Some(model_data) = chunk_model {
                    model = model_data;
                }
            }
        }

        // Finalize — get all assembled content blocks from the parser
        let content_blocks = parser.finalize();

        // NOTE: Do NOT re-notify the observer with thinking content here.
        // During streaming, every ThinkingFragment already called
        // observer.on_thinking() with the incremental delta, and the TUI
        // appended those deltas into the thinking bubble. The content_blocks
        // here carry the FULL assembled thinking snapshot (used to build the
        // InferenceResponse below). Re-emitting it as an observer event would
        // make the TUI append the entire thinking text a second time — the
        // "thinking displays twice" bug.

        tracing::info!(
            "Streaming iteration completed: {} content blocks (text: {} chars, tool_calls: {}, thinking: {} chars)",
            content_blocks.len(),
            content_blocks.iter().filter_map(|b| match b { ContentBlock::Text { text } => Some(text.len()), _ => None }).sum::<usize>(),
            content_blocks.iter().filter(|b| matches!(b, ContentBlock::ToolCall { .. })).count(),
            content_blocks.iter().filter_map(|b| match b { ContentBlock::Thinking { text } => Some(text.len()), _ => None }).sum::<usize>(),
        );

        Ok(InferenceResponse {
            message: Message {
                role: Role::Assistant,
                content: content_blocks,
                metadata: HashMap::new(),
            },
            usage,
            model,
            metadata: HashMap::new(),
        })
    }

    /// Persist a working-state `task_created` + `step_added` events when the
    /// model submits a plan via `exit_plan_mode`. Creates the durable task (in
    /// the cross-session event log) the first time, binds `state.task_id`, and
    /// re-derives the in-memory `working_state` projection so the pinned blocks
    /// read the durable source of truth from the next turn. No-op when no
    /// `WorkingStateStore` is configured. Errors are non-fatal (logged): a
    /// persistence hiccup must not abort the agent loop.
    async fn ensure_working_state_task(
        &self,
        state: &mut LoopState,
        steps: &[oneai_core::PlanStep],
        goal: &str,
    ) {
        let Some(store) = self.working_state_store.clone() else {
            return;
        };
        // Create the task once per run.
        if state.task_id.is_none() {
            match store
                .create_task(&state.user_id, &state.project, goal, "", &state.session_id)
                .await
            {
                Ok(id) => {
                    state.task_id = Some(id.clone());
                    state
                        .conversation
                        .metadata
                        .insert("task_id".to_string(), id);
                }
                Err(e) => {
                    tracing::warn!("Failed to create working-state task: {}", e);
                    return;
                }
            }
        }
        let task_id = match state.task_id.clone() {
            Some(t) => t,
            None => return,
        };
        // Append a step_added event per submitted step (the store dedups by id
        // on derive, so re-submits are harmless).
        for (i, s) in steps.iter().enumerate() {
            let step = oneai_core::Step {
                id: s.id.clone(),
                description: s.description.clone(),
                status: s.status.into(),
                depends_on: s.depends_on.clone(),
                order: (i + 1) as u32,
                active_form: s.active_form.clone(),
                updated_at: String::new(),
            };
            if let Err(e) = store
                .append_event(
                    &task_id,
                    &state.session_id,
                    None,
                    oneai_core::TaskEventType::StepAdded,
                    oneai_core::TaskEventPayload::StepAdded { step },
                )
                .await
            {
                tracing::warn!("Failed to append step_added event: {}", e);
            }
        }
        // Bound the log's growth: fold into a snapshot past the threshold.
        self.compact_working_state_if_needed(&task_id).await;
        // Re-derive the in-memory projection so pinned blocks read the durable
        // state from the next turn.
        match store.get_task(&task_id).await {
            Ok(Some(ws)) => state.working_state = Some(ws),
            Ok(None) => tracing::warn!("Working-state task '{}' vanished after create", task_id),
            Err(e) => tracing::warn!("Failed to re-derive working state: {}", e),
        }
    }

    /// Sync `task_update` step-status changes into the durable working-state
    /// event log. Diffs the live `plan_state` step statuses against the
    /// in-memory `working_state.steps` and appends a `step_status_changed`
    /// event for each that moved, then re-derives the projection. No-op when
    /// no store is configured or no task is bound.
    async fn sync_step_status_to_working_state(&self, state: &mut LoopState) {
        let Some(store) = self.working_state_store.clone() else {
            return;
        };
        let Some(task_id) = state.task_id.clone() else {
            return;
        };
        let Some(plan) = state.plan_state.clone() else {
            return;
        };
        let known: std::collections::HashMap<String, oneai_core::StepStatus> = state
            .working_state
            .as_ref()
            .map(|ws| ws.steps.iter().map(|s| (s.id.clone(), s.status)).collect())
            .unwrap_or_default();
        for s in &plan.steps {
            let new_status: oneai_core::StepStatus = s.status.into();
            let old_status = known.get(&s.id).copied();
            if old_status != Some(new_status) {
                if let Err(e) = store
                    .append_event(
                        &task_id,
                        &state.session_id,
                        None,
                        oneai_core::TaskEventType::StepStatusChanged,
                        oneai_core::TaskEventPayload::StepStatusChanged {
                            step_id: s.id.clone(),
                            status: new_status,
                            active_form: s.active_form.clone(),
                        },
                    )
                    .await
                {
                    tracing::warn!("Failed to append step_status_changed event: {}", e);
                }
            }
        }
        // Bound the log's growth: fold into a snapshot past the threshold.
        self.compact_working_state_if_needed(&task_id).await;
        // Re-derive the projection.
        match store.get_task(&task_id).await {
            Ok(Some(ws)) => state.working_state = Some(ws),
            Ok(None) => {}
            Err(e) => tracing::warn!("Failed to re-derive working state: {}", e),
        }
    }

    /// Fold the per-task event log into a `Snapshot` once it crosses the
    /// domain's compaction threshold (reference doc §7.3 / §8.4 — bounded
    /// growth via in-log snapshots). Called after each working-state append
    /// so the log never grows unbounded across a long task. No-op under the
    /// threshold; errors are non-fatal (logged) — a compaction hiccup must
    /// not abort the agent loop.
    async fn compact_working_state_if_needed(&self, task_id: &str) {
        let Some(store) = self.working_state_store.clone() else {
            return;
        };
        if let Err(e) = store.compact_if_needed(task_id).await {
            tracing::warn!("Working-state compaction failed for '{}': {}", task_id, e);
        }
    }

    /// Build the tool-preference rules block dynamically from the actual tool
    /// registry, so the system prompt never promises the model tools it cannot
    /// call. Each rule is emitted only when its referenced tool is registered;
    /// if none of the known coding tools are present, a generic nudge is
    /// emitted instead. This is the dynamic replacement for the
    /// `{{TOOL_PREFERENCE_RULES}}` marker in the default system prompt.
    async fn tool_preference_block(&self) -> String {
        let tools = self.tools.read().await;
        build_tool_preference_block(&tools)
    }

    /// Resolve the effective system prompt: the configured prompt with the
    /// `{{TOOL_PREFERENCE_RULES}}` marker (if present) replaced by the
    /// registry-derived preference block. Domain-provided prompts that do not
    /// contain the marker are returned unchanged, so this only mutates the
    /// Derive the Layer-1 `ConstrainedOutputConfig` to attach to an inference
    /// request, bridging `structured_output.schema` with the tier-gating policy.
    ///
    /// Returns `None` when there is no `StructuredOutputConfig`, or when the
    /// policy disables constrained decoding for this provider tier. Post-hoc
    /// `validate_json_schema` + `ModelRetry` run regardless (they are driven by
    /// `structured_output`, not by this value).
    fn build_constrained_output(&self) -> Option<ConstrainedOutputConfig> {
        let so = self.config.structured_output.as_ref()?;
        let want = match self.config.constrained_output_policy {
            ConstrainedOutputPolicy::Auto => self.provider.prefers_constrained_output(),
            ConstrainedOutputPolicy::Always => true,
            ConstrainedOutputPolicy::Never => false,
            // non_exhaustive: unknown future variants fall back to Auto's behavior.
            _ => self.provider.prefers_constrained_output(),
        };
        if !want {
            return None;
        }
        Some(ConstrainedOutputConfig {
            schema: so.schema.clone(),
            mode: ConstrainedMode::JsonSchema,
        })
    }

    /// default prompt path — preserving domain prompt behavior.
    async fn build_system_prompt(&self) -> String {
        let prompt = self.config.system_prompt.clone();
        if prompt.contains("{{TOOL_PREFERENCE_RULES}}") {
            let block = self.tool_preference_block().await;
            prompt.replace("{{TOOL_PREFERENCE_RULES}}", &block)
        } else {
            prompt
        }
    }

    #[allow(dead_code)]
    async fn build_tool_definitions(&self) -> Vec<ToolDefinition> {
        let tools_map = self.tools.read().await;

        // Determine the active paradigm config for tool filtering.
        // If a paradigm config is active, only send tools in the config's
        // tool_filter to the model. This is the real behavioral change
        // that makes paradigm switching meaningful.
        //
        // When no paradigm config is active (initial state before any switch),
        // all tools are available (default ReAct behavior).
        let _tool_filter: Option<&[String]> = None; // Will be checked from LoopState in run_loop

        // Apply domain pack tool decorators if present
        if let Some(domain) = &self.domain_pack {
            // Coerce for the #27 exposure gate (see build_tool_definitions_for_paradigm).
            let resolver: Option<&dyn oneai_core::traits::ExposureResolver> = Some(domain.as_ref());
            tools_map
                .values()
                // Footprint gate — see `build_tool_definitions_for_paradigm`.
                .filter(|tool| tool.service_available())
                // #27 exposure gate — Deferred/DeferredModelOnly/CodeModeOnly/Hidden
                // leave the initial schema.
                .filter(|tool| {
                    oneai_core::traits::effective_exposure(resolver, tool.as_ref())
                        .is_model_visible_initial()
                })
                .map(|tool| {
                    // Check if there's a decorator for this tool
                    let decorator = domain.find_decorator(tool.name());
                    match decorator {
                        Some(dec) => {
                            // Use decorator overrides
                            let description = dec
                                .description_override
                                .as_deref()
                                .unwrap_or_else(|| tool.description());
                            // Merge parameters schema with extra_params
                            let schema = if dec.extra_params.is_null()
                                || dec.extra_params == serde_json::json!({})
                            {
                                tool.parameters_schema()
                            } else {
                                oneai_domain::merge_tool_schemas(
                                    tool.parameters_schema(),
                                    dec.extra_params.clone(),
                                )
                            };
                            ToolDefinition {
                                name: tool.name().to_string(),
                                description: description.to_string(),
                                parameters_schema: schema,
                            }
                        }
                        None => ToolDefinition {
                            name: tool.name().to_string(),
                            description: tool.description().to_string(),
                            parameters_schema: tool.parameters_schema(),
                        },
                    }
                })
                .collect()
        } else {
            // No domain pack — use raw tool definitions
            tools_map
                .values()
                // Footprint gate — see `build_tool_definitions_for_paradigm`.
                .filter(|tool| tool.service_available())
                .map(|tool| ToolDefinition {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters_schema: tool.parameters_schema(),
                })
                .collect()
        }
    }

    /// Build tool definitions filtered by paradigm config.
    ///
    /// This is called from run_loop() where we have access to the
    /// LoopState's active_paradigm_config. Paradigm-configured tool
    /// filtering is the key behavioral change that makes paradigm
    /// switching meaningful — Plan mode shouldn't see edit tools,
    /// Explore mode shouldn't see execution tools.
    async fn build_tool_definitions_for_paradigm(
        &self,
        paradigm_config: Option<&ParadigmConfig>,
        has_committed_plan: bool,
    ) -> Vec<ToolDefinition> {
        let tools_map = self.tools.read().await;

        // If a paradigm config is active, filter tools by its tool_filter list.
        // Only tools in the filter are sent to the model — this prevents the
        // model from calling tools that aren't appropriate for the current paradigm.
        let filtered_tools: Vec<&Arc<dyn Tool>> = if let Some(config) = paradigm_config {
            if config.tool_filter.is_empty() {
                // Empty filter means "all tools available" — no restriction
                tools_map.values().collect()
            } else {
                // Filter: only include tools that are in the paradigm's tool_filter.
                // If the filter names tools that aren't registered (e.g. a coding
                // paradigm's `[read_file, grep, glob]` against a registry without
                // those tools — typical when no DomainPack is loaded), fall back to
                // all tools rather than silently handing the model an empty toolset.
                let matched: Vec<&Arc<dyn Tool>> = tools_map
                    .values()
                    .filter(|tool| config.tool_filter.contains(&tool.name().to_string()))
                    .collect();
                if matched.is_empty() {
                    tools_map.values().collect()
                } else {
                    matched
                }
            }
        } else {
            // No paradigm config — all tools available (default ReAct behavior)
            tools_map.values().collect()
        };

        // ─── Footprint gate (check_fn) ──────────────────────────────────────────
        // A tool whose backing service is missing (`service_available() ==
        // false`) is excluded from the schema sent to the model **entirely** —
        // zero footprint, not merely "disabled". This keeps the per-domain /
        // per-config tool table focused: the model never sees a broken option
        // (no API key, MCP server down, feature off) it would otherwise try to
        // call. See the Footprint Ladder in `CLAUDE.md`. We surface each hidden
        // tool via a warn log so "configured but prerequisite missing" is
        // discoverable rather than silent.
        let filtered_tools: Vec<&Arc<dyn Tool>> = filtered_tools
            .into_iter()
            .filter(|tool| {
                if tool.service_available() {
                    true
                } else {
                    tracing::warn!(
                        "Footprint gate: tool '{}' excluded from model schema — \
                        service_available() == false (prerequisite missing). It stays \
                        registered and reappears automatically once its check passes.",
                        tool.name()
                    );
                    false
                }
            })
            // ─── #27 ToolExposure gate ─────────────────────────────────────────
            // A tool whose effective exposure is NOT model-visible-initial
            // (`Deferred` / `DeferredModelOnly` / `CodeModeOnly` / `Hidden`)
            // leaves the initial schema. Deferred ones are reached on demand
            // via the `tool_search` tool; the rest stay registered &
            // dispatchable but out of the model's view. The effective exposure
            // is the DomainPack's `tool_exposure` override (if any) or the
            // tool's own `Tool::exposure()`. Deferred tools are excluded
            // silently (it's an intentional delay, not a missing prerequisite).
            .filter(|tool| {
                // Coerce `Option<&MergedDomainPack>` → `Option<&dyn ExposureResolver>`
                // (an unsized coercion needs an explicit coercion site — a typed
                // `let` inside the closure body).
                let resolver: Option<&dyn oneai_core::traits::ExposureResolver> =
                    self.domain_pack.as_deref().map(|dp| {
                        let r: &dyn oneai_core::traits::ExposureResolver = dp;
                        r
                    });
                oneai_core::traits::effective_exposure(resolver, tool.as_ref())
                    .is_model_visible_initial()
            })
            .collect();

        // ─── Tool ordering strategy ──────────────────────────────────────────
        // Research shows LLMs exhibit significant position bias (15-30% accuracy
        // drop when correct tool moves from first to later position). Chinese models
        // (GLM/Qwen) are especially susceptible. To guide the model toward using
        // specialized tools instead of shell for file operations, we sort tools
        // strategically: specialized tools FIRST, shell LAST.
        //
        // Priority tiers:
        //   Tier 1 (highest): read_file, grep, glob, list_directory  (read-only, most specific)
        //   Tier 2 (high):    edit_file, apply_patch, notebook_edit (edit-specific)
        //   Tier 3 (medium):  web_fetch, environment, calculator   (general but not shell)
        //   Tier 4 (lowest):  shell                                (fallback, least specific)
        //   Tier 5 (default): any tool not in above tiers          (unknown tools)
        let tier_order = |name: &str| -> u32 {
            match name {
                // Tier 1: Read-only, most specific — always prefer over shell
                "read_file" | "file_read" => 1,
                "grep" | "search" => 1,
                "glob" | "file_glob" => 1,
                "list_directory" => 1,
                // Tier 2: Edit-specific — prefer over shell for modifications
                "edit_file" | "file_edit" => 2,
                "apply_patch" => 2,
                "notebook_edit" => 2,
                // Tier 3: General but not shell
                "web_fetch" => 3,
                "environment" => 3,
                "calculator" => 3,
                // Tier 4: Shell — ALWAYS LAST (most general, most overused)
                "shell" => 10,
                // Tier 5: Unknown/custom tools — after specialized, before shell
                _ => 5,
            }
        };

        let mut sorted_tools: Vec<&Arc<dyn Tool>> = filtered_tools;
        sorted_tools.sort_by_key(|tool| tier_order(tool.name()));

        // Apply domain pack tool decorators if present
        let mut defs: Vec<ToolDefinition> = if let Some(domain) = &self.domain_pack {
            sorted_tools
                .iter()
                .map(|tool| {
                    // Check if there's a decorator for this tool
                    let decorator = domain.find_decorator(tool.name());
                    match decorator {
                        Some(dec) => {
                            // Use decorator overrides
                            let description = dec
                                .description_override
                                .as_deref()
                                .unwrap_or_else(|| tool.description());
                            // Merge parameters schema with extra_params
                            let schema = if dec.extra_params.is_null()
                                || dec.extra_params == serde_json::json!({})
                            {
                                tool.parameters_schema()
                            } else {
                                oneai_domain::merge_tool_schemas(
                                    tool.parameters_schema(),
                                    dec.extra_params.clone(),
                                )
                            };
                            ToolDefinition {
                                name: tool.name().to_string(),
                                description: description.to_string(),
                                parameters_schema: schema,
                            }
                        }
                        None => ToolDefinition {
                            name: tool.name().to_string(),
                            description: tool.description().to_string(),
                            parameters_schema: tool.parameters_schema(),
                        },
                    }
                })
                .collect()
        } else {
            // No domain pack — use raw tool definitions (still sorted)
            sorted_tools
                .iter()
                .map(|tool| ToolDefinition {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters_schema: tool.parameters_schema(),
                })
                .collect()
        };

        // Prepend the plan/task control tools. Which ones are exposed depends
        // on the mode — "工具即指令": advertising `task_create` in normal mode
        // nudges the model to over-engineer simple tasks (issue #7), so:
        //
        //  - Plan mode: the full plan toolset (task_create / task_update /
        //    task_list / request_plan_decision / exit_plan_mode) so the model
        //    can build and submit a plan. `enter_plan_mode` is omitted (the
        //    model is already in plan mode).
        //  - Normal mode + a committed plan (post exit_plan_mode approval):
        //    only task_update / task_list, so the model can track execution
        //    progress on the committed steps. Re-planning tools
        //    (task_create / request_plan_decision / exit_plan_mode /
        //    enter_plan_mode) stay hidden — the plan is committed, execute.
        //  - Normal mode, no plan yet: ONLY enter_plan_mode. The model must
        //    judge complexity and escalate explicitly; simple tasks never see
        //    task tools at all.
        //
        // These are intercepted by the loop, never dispatched to the tool
        // registry — but the model must see their definitions to call them.
        let control_defs = if self.plan_mode() {
            crate::plan_state::control_tool_definitions()
                .into_iter()
                .filter(|d| d.name != crate::plan_state::TOOL_ENTER_PLAN_MODE)
                .collect::<Vec<_>>()
        } else if has_committed_plan {
            crate::plan_state::control_tool_definitions()
                .into_iter()
                .filter(|d| {
                    matches!(
                        d.name.as_str(),
                        crate::plan_state::TOOL_TASK_UPDATE | crate::plan_state::TOOL_TASK_LIST
                    )
                })
                .collect::<Vec<_>>()
        } else {
            crate::plan_state::control_tool_definitions()
                .into_iter()
                .filter(|d| d.name == crate::plan_state::TOOL_ENTER_PLAN_MODE)
                .collect::<Vec<_>>()
        };
        let mut all = control_defs;
        all.append(&mut defs);
        // Inject the model-driven meta-tools (delegate / switch_paradigm) so the
        // model can actually call them. Like the control tools above, these are
        // intercepted by `parse_decision` and never dispatched to the
        // ToolExecutor. In plan mode the model should focus on planning, so we
        // only expose them outside plan mode.
        if !self.plan_mode() {
            let mut meta =
                crate::meta_tool::meta_tool_definitions(self.async_task_runner.is_some());
            // Don't advertise `delegate` when the sub-agent factory can't fulfill
            // it (e.g. group-chat persona members use `SubAgentFactoryNone`).
            // Otherwise the model decides to delegate a subtask, the loop emits
            // "▸ preparing delegate…", and `spawn_sub_agent` errors out — leaving
            // the UI stuck on "preparing delegate" with no path forward.
            // `switch_paradigm` is independent of the factory, so it stays.
            if self.sub_agent_factory.available_kinds().is_empty() {
                meta.retain(|d| d.name != crate::meta_tool::TOOL_DELEGATE);
                // No factory ⇒ no background delegation either.
                meta.retain(|d| !crate::meta_tool::is_loop_only_meta_tool(&d.name));
            }
            // Phase 2A: the three background-delegation tools
            // (`delegate_background` / `task_status` / `collect_results`)
            // are advertised only while the AsyncTaskRunner is configured.
            // A loop without background delegation must not offer the model
            // tools it can't honor.
            if self.async_task_runner.is_none() {
                meta.retain(|d| !crate::meta_tool::is_loop_only_meta_tool(&d.name));
            }
            // `switch_project` only makes sense when at least one context
            // source is path-bound (a DomainPack is active). On a no-domain
            // build (mobile / macOS native) there are no path-bound sources, so
            // advertising it would be a no-op the model might call uselessly.
            if !self.context_assembler.read().await.has_path_bound_sources() {
                meta.retain(|d| d.name != crate::meta_tool::TOOL_SWITCH_PROJECT);
            }
            // Phase 2A diagnostic: log which meta-tools are advertised so a
            // missing-from-schema report can be distinguished from the model
            // merely failing to enumerate them in chat.
            tracing::debug!(
                meta_tools = ?meta.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
                runner_present = self.async_task_runner.is_some(),
                factory_kinds = self.sub_agent_factory.available_kinds().len(),
                "meta-tools advertised this iteration"
            );
            all.append(&mut meta);
        }
        all
    }

    /// Build the Tier1 "Available skills" menu — a compact system message listing
    /// every registered skill's name + description. Injected every turn so the
    /// model can discover skills and invoke the `skill` tool. Returns None when
    /// the registry is empty (no menu needed).
    pub(crate) async fn build_skill_menu(&self) -> Option<String> {
        let skills = self.skill_registry.list().await;
        if skills.is_empty() {
            return None;
        }
        // Stage B: hide Archived skills (retired = invisible to the model).
        // The curator's retirements take effect next turn without a restart.
        let mut visible: Vec<_> = skills;
        if let Some(store) = &self.skill_metadata_store {
            let archived = store.list().await;
            visible.retain(|s| {
                archived
                    .get(&s.name)
                    .map(|m| m.state != oneai_skill::SkillState::Archived)
                    .unwrap_or(true)
            });
        }
        if visible.is_empty() {
            return None;
        }
        let mut lines = Vec::with_capacity(visible.len() + 2);
        lines.push(
            "# Available skills\n\
             Invoke a skill by calling the `skill` tool with its exact name. \
             The tool returns the skill's full instructions — follow them. \
             Only call a skill when it is clearly relevant to the task."
                .to_string(),
        );
        for skill in &visible {
            lines.push(format!("- {}: {}", skill.name, skill.description));
        }
        Some(lines.join("\n"))
    }

    /// Inject the ephemeral pinned blocks onto a conversation clone — the
    /// original-task anchor (Q2), the live plan/progress (Q1), and the skill
    /// menu / active skill (progressive disclosure). These are re-injected
    /// every turn and are NOT written to the durable `state.conversation`, so
    /// they survive compression by re-injection and don't accumulate.
    ///
    /// **Data source**: when `state.working_state` is bound (a
    /// `WorkingStateStore` is configured and a task is active), the pinned
    /// blocks read the in-memory working-state projection — the cross-session
    /// source of truth held in `LoopState`, zero IO per turn. The durable
    /// source is the event log, not the conversation transcript. When no
    /// working state is bound, fall back to the legacy metadata-based blocks
    /// (`task_anchor` from `original_task`, `plan_state` from the live plan).
    /// The `plan_state` is also mirrored to `conversation.metadata["plan_state"]`
    /// (persisted + copied by every compressor) for legacy Q3 reseed on reload.
    async fn inject_pinned_blocks(&self, conv: &mut Conversation, state: &LoopState) {
        if let Some(ws) = &state.working_state {
            conv.add_message(Message::system(
                crate::context_assembler::task_anchor_block_from_working_state(ws),
            ));
            conv.add_message(Message::system(
                crate::context_assembler::plan_progress_block_from_working_state(ws),
            ));
            let decisions = crate::context_assembler::decisions_block(ws);
            if !decisions.is_empty() {
                conv.add_message(Message::system(decisions));
            }
            let blockers = crate::context_assembler::blockers_block(ws);
            if !blockers.is_empty() {
                conv.add_message(Message::system(blockers));
            }
        } else {
            // Legacy path — no WorkingStateStore configured (or no task bound).
            conv.add_message(Message::system(
                crate::context_assembler::task_anchor_block(
                    &state.original_task,
                    &state.conversation.metadata,
                ),
            ));
            if let Some(plan) = &state.plan_state {
                conv.add_message(Message::system(
                    crate::context_assembler::plan_progress_block(&state.original_task, plan),
                ));
            }
        }
        if self.config.inject_skills {
            if let Some(menu) = self.build_skill_menu().await {
                conv.add_message(Message::system(menu));
            }
            if let Some(name) = &self.active_skill {
                if let Some(skill) = self.skill_registry.find_by_name(name).await {
                    let inject = format!(
                        "# Active skill: {}\n{}\n\n(Follow these instructions for this task.)",
                        skill.name, skill.prompt_template
                    );
                    conv.add_message(Message::system(inject));
                } else {
                    tracing::warn!("Active skill '{}' not in registry; clearing", name);
                }
            }
        }
        // Self-extension nudge (evolution-plan §3.4): if a previous tool batch
        // surfaced new tools, tell the model they exist. One-shot — cleared
        // by the caller after this turn's request is built (so a compression
        // re-build within the same turn still sees it, but the next turn does
        // not). Read-only here; the caller owns the clear.
        if let Some(names) = &state.pending_new_tools_note {
            if !names.is_empty() {
                let list = names
                    .iter()
                    .map(|n| format!("- `{n}`"))
                    .collect::<Vec<_>>()
                    .join("\n");
                conv.add_message(Message::system(format!(
                    "# Newly available tools\n\
                     The following tools became available after the last step:\n\
                     {list}\n\n\
                     Review their definitions above; use one if it helps complete \
                     the task. There is no obligation to call them."
                )));
            }
        }
        // ── Phase 2A: live background-task status ───────────────────────
        // The model delegated subtasks to detached background sub-agents
        // (fire-and-auto-notify). Without a live status block it can't tell
        // which are still running and re-delegates the same work in a loop
        // (see /tmp/oneai-web.log: iter2 saw only its own "Launched" text and
        // re-issued `gomoku_board_2`, `_review`, …). Injecting the runner's
        // task snapshot each iteration gives the model the visibility it needs
        // to either continue with DIFFERENT non-overlapping work (preserving
        // the parallel main+sub execution) or, if idle, end its response and
        // be auto-resumed when results arrive. Ephemeral — not written to the
        // durable log (same as the new-tools note above).
        if let Some(runner) = &self.async_task_runner {
            let snapshot = runner.snapshot_with_meta().await;
            if !snapshot.is_empty() {
                let body = snapshot
                    .iter()
                    .map(|(id, kind, desc, st)| {
                        // Char-boundary-safe trim so CJK task descriptions stay valid.
                        let trimmed: String = if desc.chars().count() > 80 {
                            let taken: String = desc.chars().take(77).collect();
                            format!("{taken}…")
                        } else {
                            desc.clone()
                        };
                        format!("- `{id}` ({}, {}): {}", kind.name(), st.label(), trimmed)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                conv.add_message(Message::system(format!(
                    "[Background tasks] (live)\n{body}\n\
                     These sub-agents are running detached in PARALLEL with you. You WILL be \
                     auto-resumed (a new turn) when each finishes — its result will arrive as a \
                     new message. DO NOT re-delegate any task listed above as `Running` or \
                     `Completed`; that is wasted duplicate work. Instead, continue with \
                     DIFFERENT non-overlapping work now (you have all tools), or if you have \
                     nothing else to do, end your response and the results will wake you."
                )));
            }
        }
    }

    /// Select a recovery strategy based on the type of tool call failure.
    ///
    /// Maps error patterns to appropriate RecoveryStrategy types:
    /// - Network/timeout errors → Retry (transient, may succeed on retry)
    /// - Permission denied → Escalate (requires human intervention)
    /// - Tool not found → ConditionalFallback (route to alternative tool)
    /// - Execution errors → ExternalFeedback (use validator to judge)
    ///
    /// This is a basic mapping — more sophisticated strategy selection
    /// can be added based on DomainPack recovery configurations.
    fn select_recovery_strategy(
        &self,
        failed: &ToolCallResult,
    ) -> crate::error_recovery::RecoveryStrategy {
        let error_msg = failed.output.error.as_deref().unwrap_or("");

        if error_msg.contains("timeout")
            || error_msg.contains("timed out")
            || error_msg.contains("network")
            || error_msg.contains("rate_limit")
        {
            // Transient errors → Retry with exponential backoff
            crate::error_recovery::RecoveryStrategy::Retry {
                policy: crate::error_recovery::RetryPolicy::default(),
            }
        } else if error_msg.starts_with("Denied") || error_msg.contains("permission") {
            // Permission denied → Escalate to human
            crate::error_recovery::RecoveryStrategy::Escalate {
                error_summary: format!("Tool call denied: {}", error_msg),
            }
        } else if error_msg.contains("not found") {
            // Tool not found → Fallback to alternative
            crate::error_recovery::RecoveryStrategy::ConditionalFallback {
                error_node: "tool_call".to_string(),
                fix_node: "alternative_approach".to_string(),
            }
        } else {
            // Default: escalate — let the main agent decide
            crate::error_recovery::RecoveryStrategy::Escalate {
                error_summary: format!("Tool execution error: {}", error_msg),
            }
        }
    }

    /// Handle the interaction-gate approval for a tool call.
    ///
    /// Routes the tool approval through the unified `InteractionGate`
    /// (`ToolApproval` point). `Proceed`/`ProceedWith{ReplaceToolArgs}` execute
    /// the tool (optionally with rewritten args); `Revise{feedback}` rejects
    /// execution and feeds the corrective guidance back as the tool result;
    /// `Abort{reason}` is the hard deny.
    async fn handle_approval(
        interaction_gate: Arc<dyn InteractionGate>,
        request: oneai_core::ApprovalRequest,
        tool: Arc<dyn Tool>,
        args: serde_json::Value,
        call_id: String,
        tool_name: String,
    ) -> Result<ToolCallResult> {
        let resp = interaction_gate
            .request(InteractionRequest::ToolApproval { approval: request })
            .await;
        match resp {
            Ok(InteractionResponse::Proceed) => {
                let output = tool.execute(args).await?;
                Ok(ToolCallResult {
                    call_id,
                    tool_name,
                    output,
                })
            }
            Ok(InteractionResponse::ProceedWith { modification }) => match modification {
                InteractionModification::ReplaceToolArgs(new_args) => {
                    let output = tool.execute(new_args).await?;
                    Ok(ToolCallResult {
                        call_id,
                        tool_name,
                        output,
                    })
                }
                _ => {
                    let output = tool.execute(args).await?;
                    Ok(ToolCallResult {
                        call_id,
                        tool_name,
                        output,
                    })
                }
            },
            Ok(InteractionResponse::Revise { feedback }) => Ok(ToolCallResult {
                call_id,
                tool_name,
                output: ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!("User rejected: {}", feedback)),
                    ..Default::default()
                },
            }),
            Ok(InteractionResponse::Abort { reason }) => Ok(ToolCallResult {
                call_id,
                tool_name,
                output: ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!("Denied: {}", reason)),
                    ..Default::default()
                },
            }),
            Ok(_) => Ok(ToolCallResult {
                call_id,
                tool_name,
                output: ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some("Unsupported interaction response for tool approval".to_string()),
                    ..Default::default()
                },
            }),
            Err(e) => Ok(ToolCallResult {
                call_id,
                tool_name,
                output: ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!("Interaction error: {}", e)),
                    ..Default::default()
                },
            }),
        }
    }
}

// ─── AgentLoopGraphActionExecutor ──────────────────────────────────────────

/// Concrete `GraphActionExecutor` that delegates to AgentLoop's full infrastructure.
///
/// This is the P2-2 bridge — when a StateGraph is active, the AgentLoop
/// creates this executor which uses the loop's own:
/// - LLM provider (with context assembly + tool definitions)
/// - Tool registry (with domain pack decorators)
/// - Permission gate (with domain permission profile)
/// - Hook registry (PreInfer, PostInfer, PreToolUse, PostToolUse)
/// - Output parser (for GraphDecision parsing)
/// - Recovery manager (for error recovery on failed tool calls)
///
/// The key difference from `DirectProviderActionExecutor` is that LlmInfer
/// nodes get proper tool definitions (filtered by paradigm config and domain
/// pack decorators), and ToolCall nodes go through the full permission and
/// hooks pipeline. This makes StateGraph execution truly integrated with
/// the AgentLoop, not a separate disconnected system.
///
/// The struct type is now used by both the top-level `run_with_state_graph`
/// path and the inline `apply_paradigm_switch_with_graph` path, so the two
/// share the same executor. The `parser`, `hook_registry`, and
/// `recovery_manager` fields are cloned in but not yet read inside the
/// `GraphActionExecutor` impl — they are retained so the full-bridge
/// PreInfer/PostInfer hook firing, OutputParser-based decision parsing, and
/// tool-call error recovery can be wired in without another constructor
/// change. Wiring them is tracked as follow-up; until then these fields stay
/// `#[allow(dead_code)]` to keep the build clean.
#[allow(dead_code)]
pub struct AgentLoopGraphActionExecutor {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    parser: Arc<dyn OutputParser>,
    interaction_gate: Arc<dyn InteractionGate>,
    domain_pack: Option<Arc<MergedDomainPack>>,
    hook_registry: Arc<RwLock<HookRegistry>>,
    recovery_manager: Option<Arc<crate::error_recovery::RecoveryManager>>,
    config: AgentLoopConfig,
}

#[async_trait::async_trait]
impl oneai_workflow::GraphActionExecutor for AgentLoopGraphActionExecutor {
    /// Execute an LLM inference node using AgentLoop's full pipeline.
    ///
    /// This method:
    /// 1. Determines the active paradigm from GraphState or NodeAction
    /// 2. Builds tool definitions filtered by paradigm config and domain pack decorators
    /// 3. Runs PreInfer hooks (if any registered)
    /// 4. Calls provider.infer() with the full inference request
    /// 5. Runs PostInfer hooks (if any registered)
    /// 6. Parses the response into a GraphDecision using the same OutputParser
    /// 7. Stores the parsed_decision in GraphState for edge condition routing
    async fn execute_llm_infer(
        &self,
        action: &oneai_workflow::NodeAction,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_workflow::ActionResult> {
        // Extract LlmInfer fields
        let (
            system_prompt_override,
            include_tool_definitions,
            tool_filter_override,
            thinking_budget,
            temperature,
            max_tokens,
        ) = match action {
            oneai_workflow::NodeAction::LlmInfer {
                system_prompt_override,
                include_tool_definitions,
                tool_filter_override,
                thinking_budget,
                temperature,
                max_tokens,
                ..
            } => (
                system_prompt_override.clone(),
                *include_tool_definitions,
                tool_filter_override.clone(),
                *thinking_budget,
                *temperature,
                *max_tokens,
            ),
            _ => {
                return Err(oneai_core::error::OneAIError::Workflow(
                    "Expected LlmInfer action".to_string(),
                ))
            }
        };

        // Build system prompt — use override or default from config. When the
        // default config is in use (no override), resolve the
        // `{{TOOL_PREFERENCE_RULES}}` marker against the actual tool registry so
        // the StateGraph path, like the main loop, never promises tools the
        // model cannot call and never leaks the marker verbatim.
        let base_prompt =
            system_prompt_override.unwrap_or_else(|| self.config.system_prompt.clone());
        let system_prompt = if base_prompt.contains("{{TOOL_PREFERENCE_RULES}}") {
            let tools = self.tools.read().await;
            resolve_tool_preference_marker(&base_prompt, &tools)
        } else {
            base_prompt
        };

        let mut conversation = state.conversation.clone();
        // Inject system prompt if not already present
        if !conversation.messages.iter().any(|m| m.role == Role::System) {
            conversation.add_message(Message::system(&system_prompt));
        }

        // Build tool definitions if requested
        let tool_defs = if include_tool_definitions {
            self.build_tool_definitions_for_state(&tool_filter_override, &state.active_paradigm)
                .await
        } else {
            vec![]
        };

        // Build inference request.
        //
        // Layering: action-level override > AgentLoopConfig (user-configured
        // GenerationConfig) > scenario builtin. The builtin temperature 0.3
        // avoids the provider API default of 1.0 (too random for tool-use);
        // max_tokens 4096 bounds a single workflow node's output.
        let request = InferenceRequest {
            conversation,
            tools: tool_defs,
            max_tokens: max_tokens.or(self.config.max_tokens).or(Some(4096)),
            temperature: temperature.or(self.config.temperature).or(Some(0.3)),
            top_p: self.config.top_p,
            stop_sequences: self.config.stop_sequences.clone(),
            constrained_output: None,
            thinking_budget: thinking_budget.or(self.config.thinking_budget),
            // Carry the prompt-cache policy so StateGraph-node inference also
            // hits the provider prefix cache — same rationale as the main loop
            // request (Phase 1.5).
            metadata: HashMap::from([(
                "prompt_cache_policy".to_string(),
                self.config.prompt_cache_policy.as_str().to_string(),
            )]),
        };

        // Run inference
        let response = self.provider.infer(request).await?;
        let output = response.message.text_content();

        // Update conversation
        state.conversation.add_message(response.message.clone());

        // Parse decision and store in state
        let _decision = self.parse_decision(&response, state).await?;

        Ok(oneai_workflow::ActionResult {
            output,
            error: None,
        })
    }

    /// Execute a tool call node using AgentLoop's permission and hooks pipeline.
    async fn execute_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_workflow::ActionResult> {
        // Find the tool
        let tools_map = self.tools.read().await;
        let tool = tools_map.get(tool_name).ok_or_else(|| {
            oneai_core::error::OneAIError::Workflow(format!(
                "Tool '{}' not found for ToolCall node",
                tool_name
            ))
        })?;

        // Check domain permission profile (if domain_pack is available)
        if let Some(domain) = &self.domain_pack {
            let perm_action = domain.resolve_permission(tool_name, args);
            match perm_action {
                oneai_domain::PermissionAction::Deny { reason } => {
                    return Ok(oneai_workflow::ActionResult {
                        output: String::new(),
                        error: Some(format!("Denied by domain policy: {}", reason)),
                    });
                }
                oneai_domain::PermissionAction::AutoApprove => {
                    // Skip approval gate — domain says auto-approve
                    let output = tool.execute(args.clone()).await?;
                    state.conversation.add_message(Message::tool_result(
                        format!("graph_tool_{}", tool_name),
                        output.content.clone(),
                    ));
                    return Ok(oneai_workflow::ActionResult {
                        output: output.content,
                        error: output.error,
                    });
                }
                oneai_domain::PermissionAction::RequireConfirmation => {
                    // Need interaction-gate approval
                    let request = oneai_core::ApprovalRequest {
                        tool_name: tool_name.to_string(),
                        args: args.clone(),
                        risk_level: oneai_core::RiskLevel::High,
                        permission_level: Some(oneai_core::PermissionLevel::Full),
                        justification: format!(
                            "Domain policy requires confirmation for '{}'",
                            tool_name
                        ),
                    };
                    return self
                        .graph_tool_approval(request, tool.clone(), args.clone(), tool_name, state)
                        .await;
                }
                oneai_domain::PermissionAction::UseDefaultPermission { level } => {
                    if level == oneai_core::PermissionLevel::Full {
                        let request = oneai_core::ApprovalRequest {
                            tool_name: tool_name.to_string(),
                            args: args.clone(),
                            risk_level: tool.risk_level(),
                            permission_level: Some(level),
                            justification: format!(
                                "Full-permission tool '{}' requires approval",
                                tool_name
                            ),
                        };
                        return self
                            .graph_tool_approval(
                                request,
                                tool.clone(),
                                args.clone(),
                                tool_name,
                                state,
                            )
                            .await;
                    }
                    // Standard or Read permission — execute directly
                    let output = tool.execute(args.clone()).await?;
                    state.conversation.add_message(Message::tool_result(
                        format!("graph_tool_{}", tool_name),
                        output.content.clone(),
                    ));
                    return Ok(oneai_workflow::ActionResult {
                        output: output.content,
                        error: output.error,
                    });
                }
            }
        }

        // No domain pack — check tool's risk level for approval
        let perm_level = oneai_core::PermissionLevel::from_risk_level(tool.risk_level());
        if perm_level == oneai_core::PermissionLevel::Full {
            let request = oneai_core::ApprovalRequest {
                tool_name: tool_name.to_string(),
                args: args.clone(),
                risk_level: tool.risk_level(),
                permission_level: Some(perm_level),
                justification: format!("Full-permission tool '{}' requires approval", tool_name),
            };
            return self
                .graph_tool_approval(request, tool.clone(), args.clone(), tool_name, state)
                .await;
        }

        // Standard or Read permission — execute directly
        let output = tool.execute(args.clone()).await?;
        state.conversation.add_message(Message::tool_result(
            format!("graph_tool_{}", tool_name),
            output.content.clone(),
        ));

        Ok(oneai_workflow::ActionResult {
            output: output.content,
            error: output.error,
        })
    }

    /// Execute a paradigm switch node — updates state.active_paradigm.
    async fn execute_paradigm_switch(
        &self,
        paradigm: &str,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_workflow::ActionResult> {
        // Update active paradigm
        state.active_paradigm = Some(paradigm.to_string());
        state.parsed_decision = None; // Clear — new inference needed

        // Update conversation with paradigm-specific system prompt
        let paradigm_config = ParadigmConfig::for_paradigm(match paradigm {
            "plan" => ParadigmKind::Plan,
            "reflect" => ParadigmKind::Reflect,
            "explore" => ParadigmKind::Explore,
            _ => ParadigmKind::ReAct,
        });

        // Replace system prompt in conversation
        state
            .conversation
            .messages
            .retain(|m| m.role != Role::System);
        state
            .conversation
            .add_message(Message::system(&paradigm_config.system_prompt));

        if !paradigm_config.decision_hint.is_empty() {
            state.conversation.add_message(Message::system(format!(
                "[Paradigm switch]: {}",
                paradigm_config.decision_hint
            )));
        }

        Ok(oneai_workflow::ActionResult {
            output: format!(
                "{} paradigm activated — system prompt changed, tools filtered to: [{}]",
                paradigm,
                paradigm_config.tool_filter.join(", ")
            ),
            error: None,
        })
    }

    /// Parse an LLM response into a GraphDecision using the AgentLoop's OutputParser.
    ///
    /// This mirrors the `parse_decision()` logic from the AgentLoop, but produces
    /// a `GraphDecision` instead of an `AgentDecision`. The conversion ensures
    /// that edge conditions in the StateGraph use the same decision parsing
    /// as the AgentLoop, making routing consistent and reliable.
    async fn parse_decision(
        &self,
        response: &InferenceResponse,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_core::GraphDecision> {
        // Use the same parsing logic as AgentLoop.parse_decision()
        let mut tool_calls = Vec::new();
        let mut text_parts = Vec::new();

        for block in &response.message.content {
            match block {
                ContentBlock::ToolCall { id: _, name, args } => {
                    // Check for special internal tools
                    if name == "delegate" {
                        let args_value: serde_json::Value =
                            serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({}));
                        let agent_kind = args_value
                            .get("agent_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Explore")
                            .to_string();
                        let task = args_value
                            .get("task")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let decision = oneai_core::GraphDecision::Delegate { agent_kind, task };
                        state.parsed_decision = Some(decision.clone());
                        return Ok(decision);
                    }
                    if name == "switch_paradigm" {
                        let args_value: serde_json::Value =
                            serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({}));
                        let paradigm = args_value
                            .get("paradigm")
                            .and_then(|v| v.as_str())
                            .unwrap_or("react")
                            .to_string();
                        let decision = oneai_core::GraphDecision::SwitchParadigm { paradigm };
                        state.parsed_decision = Some(decision.clone());
                        return Ok(decision);
                    }
                    tool_calls.push(name.clone());
                }
                ContentBlock::Text { text } => {
                    text_parts.push(text.clone());
                }
                ContentBlock::Thinking { .. } => {
                    // Thinking blocks are not part of the decision — skip
                }
                _ => {}
            }
        }

        let decision = if !tool_calls.is_empty() {
            oneai_core::GraphDecision::ToolCalls {
                count: tool_calls.len(),
            }
        } else {
            oneai_core::GraphDecision::DirectAnswer {
                text: text_parts.join("\n"),
            }
        };

        state.parsed_decision = Some(decision.clone());
        Ok(decision)
    }
}

impl AgentLoopGraphActionExecutor {
    /// Route a graph-tool approval through the unified interaction gate and
    /// (on proceed) execute the tool, recording the result into `state`.
    /// Shared by the three full-permission approval sites in `execute_tool_call`.
    async fn graph_tool_approval(
        &self,
        request: oneai_core::ApprovalRequest,
        tool: Arc<dyn Tool>,
        args: serde_json::Value,
        tool_name: &str,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_workflow::ActionResult> {
        let resp = self
            .interaction_gate
            .request(InteractionRequest::ToolApproval { approval: request })
            .await?;
        match resp {
            InteractionResponse::Proceed => {
                let output = tool.execute(args).await?;
                state.conversation.add_message(Message::tool_result(
                    format!("graph_tool_{}", tool_name),
                    output.content.clone(),
                ));
                Ok(oneai_workflow::ActionResult {
                    output: output.content,
                    error: output.error,
                })
            }
            InteractionResponse::ProceedWith { modification } => match modification {
                InteractionModification::ReplaceToolArgs(new_args) => {
                    let output = tool.execute(new_args).await?;
                    state.conversation.add_message(Message::tool_result(
                        format!("graph_tool_{}", tool_name),
                        output.content.clone(),
                    ));
                    Ok(oneai_workflow::ActionResult {
                        output: output.content,
                        error: output.error,
                    })
                }
                _ => {
                    let output = tool.execute(args).await?;
                    state.conversation.add_message(Message::tool_result(
                        format!("graph_tool_{}", tool_name),
                        output.content.clone(),
                    ));
                    Ok(oneai_workflow::ActionResult {
                        output: output.content,
                        error: output.error,
                    })
                }
            },
            InteractionResponse::Revise { feedback } => Ok(oneai_workflow::ActionResult {
                output: String::new(),
                error: Some(format!("User rejected: {}", feedback)),
            }),
            InteractionResponse::Abort { reason } => Ok(oneai_workflow::ActionResult {
                output: String::new(),
                error: Some(format!("Denied: {}", reason)),
            }),
            _ => Ok(oneai_workflow::ActionResult {
                output: String::new(),
                error: Some("Unsupported interaction response for tool approval".to_string()),
            }),
        }
    }

    /// Build tool definitions filtered by paradigm config and domain pack.
    ///
    /// This is the same logic as `AgentLoop.build_tool_definitions_for_paradigm()`,
    /// adapted for GraphState's `active_paradigm` and `tool_filter_override`.
    async fn build_tool_definitions_for_state(
        &self,
        tool_filter_override: &Option<Vec<String>>,
        active_paradigm: &Option<String>,
    ) -> Vec<ToolDefinition> {
        let tools_map = self.tools.read().await;

        // Determine tool filter: override > paradigm config > all tools.
        // As with `build_tool_definitions_for_paradigm`, a non-empty filter that
        // matches nothing in the registry falls back to all tools — otherwise a
        // coding paradigm's filter would yield an empty toolset when no
        // DomainPack registered the named tools.
        let paradigm_config = active_paradigm_to_config(active_paradigm);
        let filtered_tools: Vec<&Arc<dyn Tool>> = if let Some(filter) = tool_filter_override {
            // Override: only include specified tools
            let matched: Vec<&Arc<dyn Tool>> = tools_map
                .values()
                .filter(|tool| filter.contains(&tool.name().to_string()))
                .collect();
            if matched.is_empty() {
                tools_map.values().collect()
            } else {
                matched
            }
        } else if let Some(config) = &paradigm_config {
            if config.tool_filter.is_empty() {
                tools_map.values().collect()
            } else {
                let matched: Vec<&Arc<dyn Tool>> = tools_map
                    .values()
                    .filter(|tool| config.tool_filter.contains(&tool.name().to_string()))
                    .collect();
                if matched.is_empty() {
                    tools_map.values().collect()
                } else {
                    matched
                }
            }
        } else {
            tools_map.values().collect()
        };

        // Footprint gate — see `build_tool_definitions_for_paradigm`.
        let filtered_tools: Vec<&Arc<dyn Tool>> = filtered_tools
            .into_iter()
            .filter(|tool| tool.service_available())
            // #27 exposure gate — keep only model-visible-initial exposures.
            // See `build_tool_definitions_for_paradigm` for the full rationale.
            .filter(|tool| {
                let resolver: Option<&dyn oneai_core::traits::ExposureResolver> =
                    self.domain_pack.as_deref().map(|dp| {
                        let r: &dyn oneai_core::traits::ExposureResolver = dp;
                        r
                    });
                oneai_core::traits::effective_exposure(resolver, tool.as_ref())
                    .is_model_visible_initial()
            })
            .collect();

        // Apply domain pack tool decorators
        let mut defs: Vec<ToolDefinition> = if let Some(domain) = &self.domain_pack {
            filtered_tools
                .iter()
                .map(|tool| {
                    let decorator = domain.find_decorator(tool.name());
                    match decorator {
                        Some(dec) => {
                            let description = dec
                                .description_override
                                .as_deref()
                                .unwrap_or_else(|| tool.description());
                            let schema = if dec.extra_params.is_null()
                                || dec.extra_params == serde_json::json!({})
                            {
                                tool.parameters_schema()
                            } else {
                                oneai_domain::merge_tool_schemas(
                                    tool.parameters_schema(),
                                    dec.extra_params.clone(),
                                )
                            };
                            ToolDefinition {
                                name: tool.name().to_string(),
                                description: description.to_string(),
                                parameters_schema: schema,
                            }
                        }
                        None => ToolDefinition {
                            name: tool.name().to_string(),
                            description: tool.description().to_string(),
                            parameters_schema: tool.parameters_schema(),
                        },
                    }
                })
                .collect()
        } else {
            filtered_tools
                .iter()
                .map(|tool| ToolDefinition {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters_schema: tool.parameters_schema(),
                })
                .collect()
        };

        // Inject the model-driven meta-tools (delegate / switch_paradigm) so
        // LlmInfer nodes inside a StateGraph can also delegate / switch
        // paradigm. Intercepted by `AgentLoopGraphActionExecutor::parse_decision`,
        // never dispatched to the ToolExecutor.
        // NOTE: the plan control tools (task_create/exit_plan_mode/...) are not
        // injected here yet — that is pre-existing tech debt, out of scope for
        // the meta-tool打通 work.
        // `switch_project` is loop-only: it relies on `AgentDecision::
        // SwitchProject`, which the graph executor (oneai-core `GraphDecision`)
        // doesn't model. Filter it so a StateGraph LlmInfer node doesn't
        // advertise a meta-tool it can't honor.
        // `switch_project` and the three Phase 2A background-delegation tools
        // are loop-only: they rely on `AgentDecision::SwitchProject` /
        // `DelegateBackground` / `TaskStatus` / `CollectResults`, which the
        // graph executor (oneai-core `GraphDecision`) doesn't model. Filter
        // them so a StateGraph LlmInfer node doesn't advertise meta-tools it
        // can't honor.
        // The graph executor has no AsyncTaskRunner, so it advertises `delegate`
        // foreground-only (background mode omitted via meta_tool_definitions(false)).
        let mut meta = crate::meta_tool::meta_tool_definitions(false);
        meta.retain(|d| !crate::meta_tool::is_loop_only_meta_tool(&d.name));
        defs.append(&mut meta);
        defs
    }
}

/// Convert a string paradigm name to ParadigmConfig.
fn active_paradigm_to_config(paradigm: &Option<String>) -> Option<ParadigmConfig> {
    paradigm.as_ref().map(|p| {
        ParadigmConfig::for_paradigm(match p.as_str() {
            "plan" => ParadigmKind::Plan,
            "reflect" => ParadigmKind::Reflect,
            "explore" => ParadigmKind::Explore,
            _ => ParadigmKind::ReAct,
        })
    })
}

/// Build the tool-preference rules block from a tool registry, emitting a rule
/// only for tools that are actually registered. Used both by the main
/// `AgentLoop` (via `tool_preference_block`) and by the StateGraph
/// `AgentLoopGraphActionExecutor` when resolving the `{{TOOL_PREFERENCE_RULES}}`
/// marker — so neither path promises the model tools it cannot call.
fn build_tool_preference_block(tools: &HashMap<String, Arc<dyn Tool>>) -> String {
    let has = |n: &str| tools.contains_key(n);

    let mut rules: Vec<&str> = Vec::new();
    if has("read_file") {
        rules.push("- For reading files: use read_file (NOT shell cat/head/tail)");
    }
    if has("edit_file") {
        rules.push("- For editing files: use edit_file (NOT shell sed/awk)");
    }
    if has("write_file") {
        rules.push("- For creating/writing files: use write_file (NOT shell echo/tee/cat>)");
    }
    if has("list_directory") {
        rules.push("- For listing directories: use list_directory (NOT shell ls)");
    }
    if has("grep") {
        rules.push("- For searching content: use grep (NOT shell grep/find)");
    }
    if has("glob") {
        rules.push("- For finding files: use glob (NOT shell find)");
    }
    if has("shell") {
        rules.push(
            "- Use shell ONLY for: compilation, testing, git operations, package management, \
             running scripts, or commands that have no dedicated tool equivalent",
        );
    }
    if rules.is_empty() {
        // No known coding tools registered — don't promise specifics. Nudge the
        // model toward the tools that ARE available (listed in its tool
        // definitions) rather than naming tools that may not exist.
        return "\n\n**Tool Use**: Use the tools available to you when they help; \
                if none apply, answer directly. When you have the final answer, \
                respond with just text without any tool calls."
            .to_string();
    }
    format!(
        "\n\n**Tool Preference Rules** (IMPORTANT — always follow these):\n{}\n\
         - This ensures safer, more precise, and more readable operations",
        rules.join("\n")
    )
}

/// Replace the `{{TOOL_PREFERENCE_RULES}}` marker in `prompt` with a block
/// derived from `tools`, returning the prompt unchanged when the marker is
/// absent. Shared by the main loop and the graph action executor so the marker
/// can never leak into a system message unexpanded.
fn resolve_tool_preference_marker(prompt: &str, tools: &HashMap<String, Arc<dyn Tool>>) -> String {
    if prompt.contains("{{TOOL_PREFERENCE_RULES}}") {
        let block = build_tool_preference_block(tools);
        prompt.replace("{{TOOL_PREFERENCE_RULES}}", &block)
    } else {
        prompt.to_string()
    }
}

#[cfg(test)]
mod smart_router_tests {
    use super::*;

    #[test]
    fn test_route_cat_to_read_file() {
        let call = ToolCallRequest {
            id: "test-1".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "cat src/main.rs"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "read_file");
        assert_eq!(routed.args["path"], "src/main.rs");
    }

    #[test]
    fn test_route_ls_to_list_directory() {
        let call = ToolCallRequest {
            id: "test-2".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "ls -la src/"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "list_directory");
        assert_eq!(routed.args["path"], "src/");
    }

    #[test]
    fn test_route_grep_to_grep_tool() {
        let call = ToolCallRequest {
            id: "test-3".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "grep -rn fn main src/"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "grep");
        assert_eq!(routed.args["pattern"], "fn");
        assert_eq!(routed.args["path"], "main"); // "main" becomes path since it's second non-option arg
    }

    #[test]
    fn test_route_find_to_glob() {
        let call = ToolCallRequest {
            id: "test-4".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "find . -name *.rs"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "glob");
        assert_eq!(routed.args["pattern"], "*.rs"); // Quotes removed
        assert_eq!(routed.args["path"], ".");
    }

    #[test]
    fn test_no_redirect_for_git() {
        let call = ToolCallRequest {
            id: "test-5".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "git status"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // No redirect
    }

    #[test]
    fn test_no_redirect_for_cargo() {
        let call = ToolCallRequest {
            id: "test-6".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "cargo test"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // No redirect
    }

    #[test]
    fn test_route_pwd_to_environment() {
        let call = ToolCallRequest {
            id: "test-7".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "pwd"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "environment");
    }

    #[test]
    fn test_route_tree_to_list_directory() {
        let call = ToolCallRequest {
            id: "test-8".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "tree src/"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "list_directory");
        assert_eq!(routed.args["path"], "src/");
    }

    #[test]
    fn test_no_redirect_for_echo_write() {
        let call = ToolCallRequest {
            id: "test-9".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "echo 'hello' > /tmp/test"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // echo with > should stay as shell
    }

    #[test]
    fn test_no_redirect_for_non_shell() {
        let call = ToolCallRequest {
            id: "test-10".to_string(),
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "src/main.rs"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "read_file"); // Non-shell calls pass through
    }

    #[test]
    fn test_route_curl_simple_to_web_fetch() {
        let call = ToolCallRequest {
            id: "test-11".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "curl https://example.com/api"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "web_fetch");
        assert_eq!(routed.args["url"], "https://example.com/api");
    }

    #[test]
    fn test_no_redirect_for_curl_post() {
        let call = ToolCallRequest {
            id: "test-12".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "curl -X POST -d 'data' https://api.com"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // POST request stays as shell
    }

    #[test]
    fn test_route_file_to_read_file() {
        let call = ToolCallRequest {
            id: "test-13".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "file src/main.rs"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "read_file");
        assert_eq!(routed.args["path"], "src/main.rs");
    }

    #[test]
    fn test_no_redirect_for_empty_command() {
        let call = ToolCallRequest {
            id: "test-14".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": ""}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // Empty command stays as shell
    }
}
#[cfg(test)]
mod dynamic_tool_prompt_tests {
    //! Tests for the registry-derived system prompt and tool filtering —
    //! verifies the prompt never promises tools the model cannot call, and
    //! that paradigm/sub-agent tool filters fall back to all tools when their
    //! hardcoded preferred names are absent from the registry (the no-DomainPack
    //! case).
    use super::*;
    use crate::context_assembler::ContextAssembler;
    use crate::mock_provider::{MockProvider, ScriptedResponse};
    use crate::mock_tool::MockTool;
    use crate::streaming::IncrementalStreamParser;
    use crate::sub_agent::SubAgentFactoryNone;
    use oneai_core::budget::{BudgetAllocation, ContextBudgetManager, TokenBudget};
    use oneai_core::TokenUsage;
    use oneai_parser::ThreeLayerParser;
    use oneai_skill::SkillSelector;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn build_loop_with(tool_names: &[&str], config: AgentLoopConfig) -> AgentLoop {
        let mut map: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        for n in tool_names {
            map.insert(
                n.to_string(),
                Arc::new(MockTool::success_tool(*n, "ok")) as Arc<dyn Tool>,
            );
        }
        let tools_map = Arc::new(tokio::sync::RwLock::new(map));
        AgentLoop::new(
            Arc::new(MockProvider::from_script(vec![])),
            tools_map,
            Arc::new(ThreeLayerParser::new()),
            Arc::new(oneai_tool::NoopInteractionGate),
            Arc::new(SkillSelector::new()),
            Arc::new(ContextBudgetManager::new(
                TokenBudget::new(100000),
                BudgetAllocation::default(),
                Arc::new(oneai_core::budget::NoopCompressor),
            )),
            Arc::new(SubAgentFactoryNone),
            ContextAssembler::new(),
            IncrementalStreamParser::new(),
            config,
        )
    }

    fn build_loop(tool_names: &[&str]) -> AgentLoop {
        build_loop_with(tool_names, AgentLoopConfig::default())
    }

    /// Like `build_loop_with` but with an explicit provider — used to test
    /// provider-dependent behavior such as constrained-output tier gating.
    fn build_loop_with_provider(
        provider: Arc<dyn LlmProvider>,
        config: AgentLoopConfig,
    ) -> AgentLoop {
        let tools_map = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        AgentLoop::new(
            provider,
            tools_map,
            Arc::new(ThreeLayerParser::new()),
            Arc::new(oneai_tool::NoopInteractionGate),
            Arc::new(SkillSelector::new()),
            Arc::new(ContextBudgetManager::new(
                TokenBudget::new(100000),
                BudgetAllocation::default(),
                Arc::new(oneai_core::budget::NoopCompressor),
            )),
            Arc::new(SubAgentFactoryNone),
            ContextAssembler::new(),
            IncrementalStreamParser::new(),
            config,
        )
    }

    /// Recording observer that captures every `on_token_usage_full` call so
    /// tests can assert the cache-token data path end-to-end (provider usage →
    /// agent loop → observer).
    type UsageCalls = Arc<Mutex<Vec<(u32, u32, u32, u32)>>>;
    struct UsageRecorder {
        calls: UsageCalls,
    }
    impl UsageRecorder {
        fn new() -> (Self, UsageCalls) {
            let calls: UsageCalls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }
    impl AgentLoopObserver for UsageRecorder {
        fn on_iteration_start(&self, _: usize, _: ParadigmKind) {}
        fn on_direct_answer(&self, _: &str) {}
        fn on_tool_calls(&self, _: &[ToolCallRequest]) {}
        fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
        fn on_delegate(&self, _: &str, _: &str, _: &SubAgentKind) {}
        fn on_paradigm_switch(&self, _: ParadigmKind) {}
        fn on_checkpoint(&self, _: usize) {}
        fn on_complete(&self, _: &AgentLoopResult) {}
        fn on_thinking(&self, _: &str) {}
        fn on_token_usage_full(
            &self,
            prompt_tokens: u32,
            completion_tokens: u32,
            cache_read_tokens: u32,
            cache_creation_tokens: u32,
        ) {
            self.calls.lock().unwrap().push((
                prompt_tokens,
                completion_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            ));
        }
    }

    #[tokio::test]
    async fn on_token_usage_full_propagates_provider_cache_tokens() {
        // Script a DirectAnswer whose usage carries real cache stats (as the
        // Anthropic/OpenAI providers now report them). The agent loop must
        // surface them via `on_token_usage_full` — not drop them on the floor.
        let scripted = ScriptedResponse::custom(
            vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
            TokenUsage {
                prompt_tokens: 1000,
                completion_tokens: 50,
                total_tokens: 1050,
                cache_read_tokens: 800,
                cache_creation_tokens: 200,
            },
        );
        let provider = MockProvider::from_script(vec![scripted]);
        let loop_ = build_loop_with_provider(Arc::new(provider), AgentLoopConfig::default());
        let (recorder, calls) = UsageRecorder::new();
        let _ = loop_.run_with_observer("hi", &recorder).await;

        let recorded = calls.lock().unwrap().clone();
        assert!(
            recorded
                .iter()
                .any(|(_, _, cr, cc)| *cr == 800 && *cc == 200),
            "cache tokens not propagated to observer; got {:?}",
            recorded
        );
    }

    #[tokio::test]
    async fn tool_preference_block_empty_registry_emits_generic_nudge() {
        let loop_ = build_loop(&[]);
        let block = loop_.tool_preference_block().await;
        // No coding tools registered → must not promise any, and must not
        // emit the "Tool Preference Rules" header.
        assert!(!block.contains("read_file"));
        assert!(!block.contains("Tool Preference Rules"));
        assert!(block.contains("Tool Use"));
    }

    #[tokio::test]
    async fn tool_preference_block_with_coding_tools_emits_rules() {
        let loop_ = build_loop(&["read_file", "edit_file", "grep", "glob", "shell"]);
        let block = loop_.tool_preference_block().await;
        assert!(block.contains("Tool Preference Rules"));
        assert!(block.contains("read_file"));
        assert!(block.contains("edit_file"));
        assert!(block.contains("Use shell ONLY for"));
    }

    #[tokio::test]
    async fn build_system_prompt_replaces_marker() {
        let loop_ = build_loop(&["read_file"]);
        let prompt = loop_.build_system_prompt().await;
        // The marker must be substituted, and the substituted rules must reflect
        // the actual registry.
        assert!(!prompt.contains("{{TOOL_PREFERENCE_RULES}}"));
        assert!(prompt.contains("read_file"));
    }

    #[tokio::test]
    async fn build_system_prompt_no_marker_left_unchanged() {
        // A domain-style prompt without the marker is returned verbatim — the
        // dynamic block is not appended, preserving domain prompt behavior.
        let cfg = AgentLoopConfig {
            system_prompt: "You are a research agent. Use the available tools.".to_string(),
            ..AgentLoopConfig::default()
        };
        let loop_ = build_loop_with(&["read_file"], cfg);
        let prompt = loop_.build_system_prompt().await;
        assert_eq!(prompt, "You are a research agent. Use the available tools.");
    }

    #[test]
    fn build_constrained_output_bridges_policy_and_provider_tier() {
        use oneai_core::{ConstrainedMode, ConstrainedOutputPolicy, StructuredOutputConfig};
        use oneai_provider::OllamaProvider;

        let schema = serde_json::json!({ "type": "object", "required": ["answer"] });
        let so = StructuredOutputConfig {
            schema: schema.clone(),
            max_retries: 2,
            re_prompt_on_failure: true,
            error_prompt_template: None,
        };

        let cfg_with_so = |policy: ConstrainedOutputPolicy| AgentLoopConfig {
            structured_output: Some(so.clone()),
            constrained_output_policy: policy,
            ..AgentLoopConfig::default()
        };

        // Auto + local backend (Ollama prefers true) → Some(JsonSchema, schema).
        let ollama_loop = build_loop_with_provider(
            Arc::new(OllamaProvider::new(oneai_core::ModelConfig::ollama(
                "llama3".to_string(),
            ))),
            cfg_with_so(ConstrainedOutputPolicy::Auto),
        );
        let co = ollama_loop
            .build_constrained_output()
            .expect("Auto+local → Some");
        assert_eq!(co.mode, ConstrainedMode::JsonSchema);
        assert_eq!(co.schema, schema);

        // Auto + cloud backend (MockProvider prefers false) → None.
        let mock_loop = build_loop_with_provider(
            Arc::new(MockProvider::always_answers("ok")),
            cfg_with_so(ConstrainedOutputPolicy::Auto),
        );
        assert!(mock_loop.build_constrained_output().is_none());

        // Always forces it on even for the cloud mock.
        let always_loop = build_loop_with_provider(
            Arc::new(MockProvider::always_answers("ok")),
            cfg_with_so(ConstrainedOutputPolicy::Always),
        );
        assert!(always_loop.build_constrained_output().is_some());

        // Never forces it off even for Ollama.
        let never_loop = build_loop_with_provider(
            Arc::new(OllamaProvider::new(oneai_core::ModelConfig::ollama(
                "llama3".to_string(),
            ))),
            cfg_with_so(ConstrainedOutputPolicy::Never),
        );
        assert!(never_loop.build_constrained_output().is_none());

        // No structured_output → None regardless of policy/provider.
        let no_so_loop = build_loop_with_provider(
            Arc::new(OllamaProvider::new(oneai_core::ModelConfig::ollama(
                "llama3".to_string(),
            ))),
            AgentLoopConfig {
                structured_output: None,
                constrained_output_policy: ConstrainedOutputPolicy::Always,
                ..AgentLoopConfig::default()
            },
        );
        assert!(no_so_loop.build_constrained_output().is_none());
    }

    #[tokio::test]
    async fn paradigm_tool_filter_no_match_falls_back_to_all() {
        // Plan paradigm filters to [read_file, grep, glob]; registry has only
        // "shell". Without the fallback this would yield zero real tools.
        let loop_ = build_loop(&["shell"]);
        let cfg = ParadigmConfig::for_paradigm(ParadigmKind::Plan);
        let defs = loop_
            .build_tool_definitions_for_paradigm(Some(&cfg), false)
            .await;
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(
            names.contains(&"shell"),
            "shell should be exposed via fallback, got {:?}",
            names
        );
        assert!(!names.contains(&"read_file"));
    }

    #[tokio::test]
    async fn paradigm_tool_filter_with_match_scopes_normally() {
        // When the filter matches, least-privilege scoping is preserved:
        // edit_file and shell are excluded for the Plan paradigm.
        let loop_ = build_loop(&["read_file", "edit_file", "shell"]);
        let cfg = ParadigmConfig::for_paradigm(ParadigmKind::Plan);
        let defs = loop_
            .build_tool_definitions_for_paradigm(Some(&cfg), false)
            .await;
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(!names.contains(&"edit_file"));
        assert!(!names.contains(&"shell"));
    }

    #[tokio::test]
    async fn resolve_marker_never_leaks_into_prompt() {
        // Regression guard for the StateGraph (AgentLoopGraphActionExecutor)
        // path: a default config prompt containing the marker must be fully
        // resolved against the registry, and a non-marker prompt must pass
        // through untouched.
        let loop_with_coding = build_loop(&["read_file", "shell"]);
        let tools = loop_with_coding.tools.read().await;
        let resolved =
            resolve_tool_preference_marker(&loop_with_coding.config.system_prompt, &tools);
        assert!(!resolved.contains("{{TOOL_PREFERENCE_RULES}}"));
        assert!(resolved.contains("read_file"));

        let custom = "You are a research agent.".to_string();
        assert_eq!(resolve_tool_preference_marker(&custom, &tools), custom,);
    }

    #[test]
    fn activate_forced_paradigm_materializes_metadata_into_state() {
        // Directive::SwitchParadigm writes conversation.metadata["active_paradigm"].
        // run_with_conversation calls activate_forced_paradigm before the loop;
        // here we call it directly to assert the state mutation without running
        // inference: the chosen paradigm's config + tagged tail land in state.
        let loop_ = build_loop(&["read_file", "grep", "shell"]);
        let mut conv = oneai_core::Conversation::new();
        conv.metadata
            .insert("active_paradigm".to_string(), "plan".to_string());
        let mut state = LoopState::from_conversation(conv, "decompose the task");

        // Before activation: default paradigm, no config, no paradigm tail.
        assert_eq!(state.active_paradigm, ParadigmKind::ReAct);
        assert!(state.active_paradigm_config.is_none());
        assert!(!state
            .conversation
            .messages
            .iter()
            .any(|m| m.metadata.contains_key("paradigm_tail")));

        let activated = loop_.activate_forced_paradigm(&mut state);
        assert_eq!(activated, Some(ParadigmKind::Plan));
        assert_eq!(state.active_paradigm, ParadigmKind::Plan);
        assert!(state.active_paradigm_config.is_some());
        assert!(state.conversation.messages.iter().any(
            |m| m.role == oneai_core::Role::System && m.metadata.contains_key("paradigm_tail")
        ));
    }

    #[test]
    fn activate_forced_paradigm_no_metadata_is_noop() {
        // Default turn path: no Directive::SwitchParadigm ever received ⇒
        // metadata has no "active_paradigm" ⇒ activation is a no-op (default
        // ReAct, no config, no tail). Guards against spurious activation.
        let loop_ = build_loop(&["read_file"]);
        let mut state = LoopState::from_conversation(oneai_core::Conversation::new(), "x");
        assert_eq!(loop_.activate_forced_paradigm(&mut state), None);
        assert_eq!(state.active_paradigm, ParadigmKind::ReAct);
        assert!(state.active_paradigm_config.is_none());
    }

    #[test]
    fn activate_forced_paradigm_unknown_metadata_is_noop() {
        // A stale/corrupt metadata value (e.g. a future paradigm name this
        // binary doesn't know) must degrade to a no-op, not panic.
        let loop_ = build_loop(&["read_file"]);
        let mut conv = oneai_core::Conversation::new();
        conv.metadata
            .insert("active_paradigm".to_string(), "quantum".to_string());
        let mut state = LoopState::from_conversation(conv, "x");
        assert_eq!(loop_.activate_forced_paradigm(&mut state), None);
        assert_eq!(state.active_paradigm, ParadigmKind::ReAct);
    }
}
