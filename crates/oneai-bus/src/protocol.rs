//! Protocol types — `Directive` (inbound) and `EngineYield` (outbound) plus the
//! serializable DTOs they carry. See the crate root docs for the naming
//! rationale and stability policy.
//!
//! `oneai-core` types are referenced directly where they are already
//! `Serialize`/`Deserialize`: `ContentBlock`, `InteractionRequest`,
//! `InteractionResponse`, `InterruptReason`, `TaskEventPayload`, `ToolOutput`.
//! Types that live in `oneai-agent` (`ToolCallRequest`, `SubAgentKind`,
//! `SubAgentSummary`, `AgentLoopResult`, `ParadigmKind`) cannot be referenced
//! from this crate (it depends only on `oneai-core`) so serializable DTO
//! projections are defined here; `oneai-agent` provides the `From` conversions.

use oneai_core::{
    ContentBlock, InteractionRequest, InteractionResponse, InterruptReason, TaskEventPayload,
    ToolOutput,
};
use serde::{Deserialize, Serialize};

// ─── DTO projections ─────────────────────────────────────────────────────────

/// Serializable mirror of `oneai_agent::ParadigmKind` — the active paradigm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum BusParadigmKind {
    Plan,
    ReAct,
    Reflect,
    Explore,
}

/// Serializable mirror of `oneai_agent::SubAgentKind` — which sub-agent kind a
/// delegate targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum BusSubAgentKind {
    Plan,
    Explore,
    Code,
    Review,
    Reflect,
    /// A user-defined sub-agent kind (the registered factory name).
    Custom(String),
}

/// Serializable mirror of `oneai_agent::ToolCallRequest` — a tool call the model
/// wants to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Serializable mirror of `oneai_agent::SubAgentSummary` — a distilled result
/// of a delegated sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusSubAgent {
    pub completed: bool,
    pub summary: String,
    pub key_findings: Vec<String>,
    pub budget_exceeded: bool,
    pub agent_kind: BusSubAgentKind,
    pub tokens_used: u32,
}

/// Serializable projection of `oneai_agent::AgentLoopResult` (mirrors
/// `oneai_supervisor::TurnSummary`'s field set — the high-level fields a
/// frontend needs; full `Conversation`/`GlobalState` are not `Serialize`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusTurnSummary {
    pub final_answer: String,
    pub iterations: usize,
    pub completed: bool,
    pub active_paradigm: BusParadigmKind,
}

/// Serializable usage record — mirrors the four counters
/// `AgentLoopObserver::on_token_usage_full` receives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BusUsageRecord {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
}

// ─── Directive (inbound) ─────────────────────────────────────────────────────

/// An instruction the frontend submits to the engine. The engine acts on it
/// (start a turn, steer paradigm, interrupt, reply to an approval, shut down).
///
/// Control directives — [`Directive::Approve`] and [`Directive::Interrupt`] —
/// are handled by the bus itself (they resolve a pending approval / fire the
/// registered cancel token). The rest ([`Directive::UserMessage`],
/// [`Directive::SwitchParadigm`], [`Directive::Shutdown`]) are forwarded to
/// the engine driver to read off its directive stream.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// `Approve` carries `InteractionResponse` (~288 B). Boxing would shrink the
// enum but mirrors `oneai_core::InteractionResponse`'s own decision NOT to box
// (boxing breaks ergonomic public construction). Directives are infrequent
// (handful per turn), so the larger enum size is an acceptable trade.
#[allow(clippy::large_enum_variant)]
pub enum Directive {
    /// Start (or continue) a turn with this user content.
    UserMessage { content: Vec<ContentBlock> },
    /// Reply to a previously emitted [`EngineYield::ApprovalRequest`].
    Approve {
        request_id: String,
        response: InteractionResponse,
    },
    /// Cooperatively cancel the in-flight turn — maps to the engine's
    /// `CancellationToken`.
    Interrupt { reason: InterruptReason },
    /// Force a paradigm switch (also doable model-driven via `switch_paradigm`).
    SwitchParadigm { to: BusParadigmKind },
    /// Graceful session end. The engine emits [`EngineYield::SessionEnded`]
    /// then drops its channels.
    Shutdown,
}

// ─── EngineYield (outbound) ──────────────────────────────────────────────────

/// Something the engine yields to every subscribed frontend. Each variant maps
/// 1:1 to an `AgentLoopObserver` callback or an engine lifecycle event; the bus
/// converts the callback into a stream value so all frontends consume the same
/// shape whether in-process or over the sidecar wire.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineYield {
    /// A new turn started (carries the engine-assigned `turn_id`).
    TurnStart { turn_id: String, task: String },
    /// Iteration N of the loop began, under this paradigm.
    IterationStart {
        turn_id: String,
        iteration: usize,
        paradigm: BusParadigmKind,
    },
    /// Incremental model text output (typewriter). ← `on_stream_chunk`.
    StreamChunk { turn_id: String, text: String },
    /// Model thinking/reasoning fragment. ← `on_thinking`.
    Thinking { turn_id: String, text: String },
    /// Model produced a direct answer (loop will end). ← `on_direct_answer`.
    DirectAnswer { turn_id: String, text: String },
    /// Model wants to call tools. ← `on_tool_calls`.
    ToolCalls {
        turn_id: String,
        calls: Vec<BusToolCall>,
    },
    /// A tool call completed. ← `on_tool_result`.
    ToolResult {
        turn_id: String,
        call_id: String,
        tool_name: String,
        output: ToolOutput,
    },
    /// Model delegated to a sub-agent. ← `on_delegate`.
    Delegate {
        turn_id: String,
        task: String,
        agent_kind: BusSubAgentKind,
    },
    /// A delegated sub-agent finished. ← `on_delegate_complete`.
    DelegateComplete {
        turn_id: String,
        summary: BusSubAgent,
    },
    /// Paradigm switched. ← `on_paradigm_switch`.
    ParadigmSwitch {
        turn_id: String,
        from: BusParadigmKind,
        to: BusParadigmKind,
    },
    /// Engine needs a decision from the user — blocks until the matching
    /// [`Directive::Approve`] arrives. Carries the engine-assigned `request_id`.
    /// ← `ChannelInteractionGate` pending item.
    ApprovalRequest {
        request_id: String,
        request: InteractionRequest,
    },
    /// Working-state append (task progress, decisions, blockers).
    WorkingState { event: TaskEventPayload },
    /// Per-category context token breakdown. ← `on_context_accounting`.
    /// Carried as the core `ContextAccounting` (serde-derivable); a frontend
    /// renders its context/utilization panel straight off it.
    ContextAccounting {
        turn_id: String,
        accounting: oneai_core::ContextAccounting,
    },
    /// Plan-state snapshot changed (task created/updated/cleared).
    /// ← `on_plan_update`. `oneai_agent::PlanState` lives in a crate the bus
    /// doesn't depend on, so it's carried as its `serde_json::Value` form
    /// (BusObserver serializes; the frontend deserializes).
    PlanUpdate {
        turn_id: String,
        plan: Option<serde_json::Value>,
    },
    /// Tools were added to the registry mid-run (self-extension). ←
    /// `on_tools_added`.
    ToolsAdded { turn_id: String, names: Vec<String> },
    /// `/init` (project-info generation) finished — payload is the
    /// pre-formatted result/error message. Producer-agnostic: in the
    /// in-process CLI the frontend's /init task emits this directly; in a
    /// sidecar the engine emits it after processing `Directive::InitProject`.
    InitResult { message: String },
    /// `/compact` (LLM summarization) finished. `summary` empty ⇒
    /// conversation was too short. `retained` are the recent `(role, text)`
    /// turns kept for the frontend to re-render after clearing its display.
    CompactResult {
        summary: String,
        removed_count: usize,
        retained: Vec<(String, String)>,
    },
    /// Cache-aware token usage for the turn so far. ← `on_token_usage_full`.
    TokenUsage { usage: BusUsageRecord },
    /// A recoverable (loop continues) or fatal (turn ends) engine error.
    Error { recoverable: bool, message: String },
    /// Turn completed. ← `on_complete`.
    TurnComplete {
        turn_id: String,
        summary: BusTurnSummary,
    },
    /// Session ended — no further yields will be emitted.
    SessionEnded,
}
