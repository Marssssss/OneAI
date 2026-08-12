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

// ─── Engine-config + group-chat scenario DTOs ───────────────────────────────
//
// Carry the foreign-side config/scenario JSON the c_facade's `Directive::Init`
// / `Directive::StartGroupChat` need, so the 3-symbol pump can build the
// engine + a multi-agent `GroupChatSession` without any foreign-specific view
// type. Field shapes mirror the legacy `c_facade::parse_config` /
// `parse_scenario` JSON exactly, so a frontend that already built that JSON
// (the macOS scenario Record / the Windows C# config object) submits the
// same bytes as an `Init` / `StartGroupChat` directive.

/// Embedding-config DTO — mirrors `oneai_uniffi::EmbeddingConfigView`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BusEmbeddingConfig {
    /// `auto` (default) ⇒ zero-config probe; `openai`/`ollama`/`voyage` ⇒ explicit.
    #[serde(default = "default_embedding_provider")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

fn default_embedding_provider() -> String {
    "auto".to_string()
}

/// Engine-config DTO — the payload of [`Directive::Init`]. Mirrors the legacy
/// `c_facade::parse_config` JSON shape (`kind`/`api_key`/`base_url`/`model`/
/// `host`/`port`/`db_path`/`default_tools`/`embedding`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusEngineConfig {
    /// Provider kind: `openai` / `anthropic` / `ollama`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_path: Option<String>,
    #[serde(default = "default_true")]
    pub default_tools: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<BusEmbeddingConfig>,
}

fn default_true() -> bool {
    true
}

/// A single group-chat member spec — mirrors `oneai_uniffi::AgentSpecView`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusAgentSpec {
    pub id: String,
    pub name: String,
    pub system_prompt: String,
    /// Provider kind: `openai`/`anthropic`/`ollama` (default `openai`).
    #[serde(default = "default_openai")]
    pub kind: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

fn default_openai() -> String {
    "openai".to_string()
}

/// Review-loop config — mirrors `oneai_uniffi::ReviewLoopSpecView`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusReviewLoop {
    pub reviewer_id: String,
    pub approve_marker: String,
    #[serde(default = "default_one")]
    pub max_rounds: u64,
}

fn default_one() -> u64 {
    1
}

/// Locale tag — mirrors `oneai_uniffi::ChatLocaleView` (`en`/`zh`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum BusLocale {
    En,
    #[default]
    Zh,
}

/// Group-chat scenario DTO — the payload of [`Directive::StartGroupChat`].
/// Mirrors the legacy `c_facade::parse_scenario` JSON shape exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusGroupScenario {
    pub members: Vec<BusAgentSpec>,
    #[serde(default = "default_scripted")]
    pub turn_policy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_order: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moderator_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_loop: Option<BusReviewLoop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<BusLocale>,
}

fn default_scripted() -> String {
    "scripted".to_string()
}

// ─── Directive (inbound) ─────────────────────────────────────────────────────

/// An instruction the frontend submits to the engine. The engine acts on it
/// (start a turn, steer paradigm, interrupt, reply to an approval, shut down).
///
/// Control directives — [`Directive::Approve`] and [`Directive::Interrupt`] —
/// are handled by the bus itself (they resolve a pending approval / fire the
/// registered cancel token). The rest ([`Directive::UserMessage`],
/// [`Directive::SwitchParadigm`], [`Directive::Shutdown`], plus the
/// session/config/init/compact directives) are forwarded to the engine driver
/// to read off its directive stream.
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
    /// Hot-update session config. Currently only `plan_mode` (Plan blocks tool
    /// execution); provider/model/memory overrides are future fields —
    /// `#[non_exhaustive]` allows adding them without breaking older frontends.
    /// `None` ⇒ leave that field unchanged.
    UpdateConfig { plan_mode: Option<bool> },
    /// Compact the conversation via LLM summarization (`/compact`). The result
    /// lands as [`EngineYield::CompactResult`].
    Compact { keep_recent_turns: usize },
    /// Generate a project-instruction file (`/init`). `format` = `oneai` /
    /// `agents` / `claude` (None ⇒ default). `force` overwrites an existing
    /// file; `no_llm` skips LLM synthesis and uses the heuristic composer.
    /// The result lands as [`EngineYield::InitResult`].
    InitProject {
        format: Option<String>,
        force: bool,
        no_llm: bool,
    },
    /// Start a fresh session. `id` = None ⇒ the engine assigns a new id; Some
    /// ⇒ bind to that id (must not already exist). Result: [`EngineYield::SessionCreated`].
    CreateSession { id: Option<String> },
    /// Load a previously-saved session by id (full id or a unique short prefix,
    /// resolved by the engine). Result: [`EngineYield::SessionLoaded`] (empty
    /// `messages` ⇒ not found / empty).
    LoadSession { id: String },
    /// Clear the live session's conversation history (the engine starts a fresh
    /// backend conversation; the frontend keeps its sidebar entry). Result:
    /// [`EngineYield::SessionCleared`].
    ClearSession,
    /// Delete a saved session from the durable store. Result:
    /// [`EngineYield::SessionDeleted`] (or [`EngineYield::Error`] if the id is
    /// unknown / the store is not configured).
    DeleteSession { id: String },
    /// Bootstrap the engine + bus + directive pump from a config blob. Only the
    /// in-process 3-symbol c_facade pump consumes this (it intercepts `Init`
    /// *before* the bus forwards anything — the pump, bus, and `AppSession`
    /// must exist before any other directive can be submitted). Sidecar
    /// frontends never send it (they `oneai serve` an already-built engine).
    Init { config: BusEngineConfig },
    /// Build a multi-agent `GroupChatSession` from a scenario (replaces the
    /// single-agent session for subsequent `GroupStart`/`GroupUserMessage`
    /// directives). The engine emits [`EngineYield::SpeakerTurn`] + speaker-
    /// tagged fragment yields for each member's turn.
    StartGroupChat { scenario: BusGroupScenario },
    /// Run the scenario's configured opener turn (call before the first
    /// `GroupUserMessage`). Only valid after `StartGroupChat`.
    GroupStart,
    /// Append the user's message and run the round's speakers per the turn
    /// policy until it's the user's turn again.
    GroupUserMessage { user_input: String },
    /// Hot-swap the group's turn policy to a fixed scripted order at runtime
    /// (used by interview-style scenarios that drop a speaker mid-conversation).
    GroupSetScriptedOrder { order: Vec<String> },
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
    /// `speaker` = `Some(member_id)` in a group-chat turn; `None` in the
    /// single-agent path (serialized as `"speaker":null`). Both ends of the
    /// bus are the same crate version, so the field is always present on the
    /// wire; an older frontend simply ignores the extra key.
    StreamChunk {
        turn_id: String,
        text: String,
        speaker: Option<String>,
    },
    /// Model thinking/reasoning fragment. ← `on_thinking`. `speaker` as above.
    Thinking {
        turn_id: String,
        text: String,
        speaker: Option<String>,
    },
    /// Model produced a direct answer (loop will end). ← `on_direct_answer`.
    DirectAnswer {
        turn_id: String,
        text: String,
        speaker: Option<String>,
    },
    /// Model wants to call tools. ← `on_tool_calls`.
    ToolCalls {
        turn_id: String,
        calls: Vec<BusToolCall>,
        speaker: Option<String>,
    },
    /// A tool call completed. ← `on_tool_result`.
    ToolResult {
        turn_id: String,
        call_id: String,
        tool_name: String,
        output: ToolOutput,
        speaker: Option<String>,
    },
    /// Model delegated to a sub-agent. ← `on_delegate`.
    Delegate {
        turn_id: String,
        task: String,
        agent_kind: BusSubAgentKind,
        speaker: Option<String>,
    },
    /// A delegated sub-agent finished. ← `on_delegate_complete`.
    DelegateComplete {
        turn_id: String,
        summary: BusSubAgent,
        speaker: Option<String>,
    },
    /// A group-chat member's turn is starting. Emitted by the group-chat bus
    /// observer's `on_speaker_turn` before that member's fragment yields, so a
    /// frontend can bracket a member's bubble. Single-agent turns never emit it.
    SpeakerTurn { turn_id: String, speaker: String },
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
    /// A new session was created (`Directive::CreateSession` / `ClearSession`).
    /// Carries the engine-assigned id so the frontend binds its sidebar entry.
    SessionCreated { id: String },
    /// A session was loaded (`Directive::LoadSession`). Carries the rebuilt
    /// message history so the frontend re-renders. Empty `messages` ⇒ not
    /// found / empty session — the frontend shows an error and keeps the live
    /// session rather than going amnesiac.
    SessionLoaded {
        id: String,
        messages: Vec<oneai_core::Message>,
    },
    /// Session cleared (`Directive::ClearSession`) — fresh backend
    /// conversation, new engine-assigned id. Distinct from `SessionCreated` so
    /// the frontend can phrase the announcement differently.
    SessionCleared { id: String },
    /// Session deleted (`Directive::DeleteSession`) — confirms the id removed
    /// from the durable store.
    SessionDeleted { id: String },
    /// Session ended — no further yields will be emitted.
    SessionEnded,
}

#[cfg(test)]
mod tests {
    //! P4-A: protocol-level serde round-trips for the `speaker` field + the
    //! new group-chat directive variants + the engine-config/scenario DTOs.
    use super::*;

    fn rt_yield(y: &EngineYield) -> EngineYield {
        let line = serde_json::to_string(y).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn single_agent_fragment_serializes_speaker_null() {
        // Single-agent path emits `speaker: None` → JSON carries
        // `"speaker":null` (not omitted — both ends are the same version).
        let y = EngineYield::StreamChunk {
            turn_id: "t1".into(),
            text: "hi".into(),
            speaker: None,
        };
        let line = serde_json::to_string(&y).unwrap();
        assert!(
            line.contains(r#""speaker":null"#),
            "single-agent fragment must serialize speaker=null: {line}"
        );
        match rt_yield(&y) {
            EngineYield::StreamChunk { text, speaker, .. } => {
                assert_eq!(text, "hi");
                assert_eq!(speaker, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn group_fragment_round_trips_speaker_some() {
        let y = EngineYield::StreamChunk {
            turn_id: "t1".into(),
            text: "hi".into(),
            speaker: Some("member-a".into()),
        };
        let line = serde_json::to_string(&y).unwrap();
        assert!(line.contains(r#""speaker":"member-a""#));
        match rt_yield(&y) {
            EngineYield::StreamChunk { speaker, .. } => {
                assert_eq!(speaker.as_deref(), Some("member-a"))
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn speaker_turn_yield_round_trips() {
        let y = EngineYield::SpeakerTurn {
            turn_id: "t1".into(),
            speaker: "member-a".into(),
        };
        let line = serde_json::to_string(&y).unwrap();
        assert!(line.contains(r#""kind":"speaker_turn""#));
        assert!(line.contains(r#""speaker":"member-a""#));
        match rt_yield(&y) {
            EngineYield::SpeakerTurn { speaker, .. } => assert_eq!(speaker, "member-a"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn all_fragment_variants_carry_speaker() {
        // Every fragment variant must have a `speaker` field after the
        // P4-A extension — guard against silently dropping one on a future edit.
        let fragments = vec![
            EngineYield::StreamChunk {
                turn_id: "t".into(),
                text: "".into(),
                speaker: None,
            },
            EngineYield::Thinking {
                turn_id: "t".into(),
                text: "".into(),
                speaker: None,
            },
            EngineYield::DirectAnswer {
                turn_id: "t".into(),
                text: "".into(),
                speaker: None,
            },
            EngineYield::ToolCalls {
                turn_id: "t".into(),
                calls: vec![],
                speaker: None,
            },
            EngineYield::ToolResult {
                turn_id: "t".into(),
                call_id: "c".into(),
                tool_name: "n".into(),
                output: ToolOutput::default(),
                speaker: None,
            },
            EngineYield::Delegate {
                turn_id: "t".into(),
                task: "".into(),
                agent_kind: BusSubAgentKind::Plan,
                speaker: None,
            },
            EngineYield::DelegateComplete {
                turn_id: "t".into(),
                summary: BusSubAgent {
                    completed: true,
                    summary: String::new(),
                    key_findings: vec![],
                    budget_exceeded: false,
                    agent_kind: BusSubAgentKind::Plan,
                    tokens_used: 0,
                },
                speaker: None,
            },
        ];
        for f in &fragments {
            // Round-trips and still has a `speaker` JSON key.
            let line = serde_json::to_string(f).unwrap();
            assert!(
                line.contains(r#""speaker""#),
                "variant lost speaker field: {line}"
            );
            let _ = serde_json::from_str::<EngineYield>(&line).unwrap();
        }
    }

    #[test]
    fn init_directive_round_trips() {
        let d = Directive::Init {
            config: BusEngineConfig {
                kind: "openai".into(),
                api_key: Some("sk-x".into()),
                base_url: None,
                model: "gpt-4o".into(),
                host: None,
                port: None,
                db_path: Some("/tmp/oneai.db".into()),
                default_tools: true,
                embedding: Some(BusEmbeddingConfig {
                    provider: "auto".into(),
                    model: None,
                    api_key: None,
                    base_url: None,
                    fallback: None,
                }),
            },
        };
        let line = serde_json::to_string(&d).unwrap();
        assert!(line.contains(r#""kind":"init""#));
        match serde_json::from_str::<Directive>(&line).unwrap() {
            Directive::Init { config } => {
                assert_eq!(config.kind, "openai");
                assert_eq!(config.model, "gpt-4o");
                assert!(config.default_tools);
                assert_eq!(config.embedding.unwrap().provider, "auto");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn init_config_defaults_when_fields_absent() {
        // A frontend that omits optional fields (api_key/base_url/port/…)
        // deserializes with the documented defaults.
        let json = r#"{"kind":"init","config":{"kind":"ollama","model":"llama3"}}"#;
        match serde_json::from_str::<Directive>(json).unwrap() {
            Directive::Init { config } => {
                assert_eq!(config.kind, "ollama");
                assert_eq!(config.model, "llama3");
                assert!(config.api_key.is_none());
                assert!(config.port.is_none());
                assert!(config.default_tools, "default_tools defaults true");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn start_group_chat_directive_round_trips() {
        let d = Directive::StartGroupChat {
            scenario: BusGroupScenario {
                members: vec![BusAgentSpec {
                    id: "writer".into(),
                    name: "写手".into(),
                    system_prompt: "起草".into(),
                    kind: "openai".into(),
                    model: "gpt-4o".into(),
                    api_key: Some("sk-test".into()),
                    base_url: None,
                    color: Some("#4D6BFE".into()),
                    avatar: None,
                }],
                turn_policy: "scripted".into(),
                script_order: Some(vec!["writer".into()]),
                moderator_id: None,
                opener_agent_id: Some("writer".into()),
                opener_line: Some("hi".into()),
                title: Some("演示".into()),
                review_loop: Some(BusReviewLoop {
                    reviewer_id: "writer".into(),
                    approve_marker: "定稿".into(),
                    max_rounds: 3,
                }),
                locale: Some(BusLocale::Zh),
            },
        };
        let line = serde_json::to_string(&d).unwrap();
        assert!(line.contains(r#""kind":"start_group_chat""#));
        match serde_json::from_str::<Directive>(&line).unwrap() {
            Directive::StartGroupChat { scenario } => {
                assert_eq!(scenario.members.len(), 1);
                assert_eq!(scenario.members[0].color.as_deref(), Some("#4D6BFE"));
                assert_eq!(scenario.turn_policy, "scripted");
                assert_eq!(scenario.title.as_deref(), Some("演示"));
                assert_eq!(scenario.review_loop.as_ref().unwrap().max_rounds, 3);
                assert_eq!(scenario.locale, Some(BusLocale::Zh));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn group_directive_variants_round_trip() {
        let cases = vec![
            serde_json::to_string(&Directive::GroupStart).unwrap(),
            serde_json::to_string(&Directive::GroupUserMessage {
                user_input: "hello".into(),
            })
            .unwrap(),
            serde_json::to_string(&Directive::GroupSetScriptedOrder {
                order: vec!["a".into(), "b".into()],
            })
            .unwrap(),
        ];
        assert!(cases[0].contains(r#""kind":"group_start""#));
        assert!(cases[1].contains(r#""kind":"group_user_message""#));
        assert!(cases[2].contains(r#""kind":"group_set_scripted_order""#));
        for c in cases {
            let _ = serde_json::from_str::<Directive>(&c).unwrap();
        }
    }
}
