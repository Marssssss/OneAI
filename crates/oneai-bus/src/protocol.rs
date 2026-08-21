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

/// Serializable mirror of `oneai_agent::DelegateProgressEvent` — coarse mid-run
/// progress from a delegated (incl. background) sub-agent. Only the high-signal
/// events a parent/UI cares about; full per-token streams stay inside the
/// sub-agent. ← `on_delegate_progress`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BusDelegateProgress {
    /// The sub-agent started a new iteration under `paradigm`.
    IterationStart {
        iteration: usize,
        paradigm: BusParadigmKind,
    },
    /// A tool finished inside the sub-agent. `snapshot` is a short result
    /// preview (truncated) — not the full output.
    ToolResult { tool_name: String, snapshot: String },
    /// Token usage after a sub-agent inference.
    TokenUsage { prompt: u32, completion: u32 },
    /// The sub-agent was cancelled (parent interrupt propagated).
    Cancelled,
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

/// A single scenario-validation problem, surfaced by
/// [`BusGroupScenario::validate`] and the app-server `scenario/validate`
/// method. Centralizing the check in the wire-DTO crate means every frontend
/// (macOS / VS Code / browser) calls one authoritative validator instead of
/// each re-implementing a client-side mirror that drifts from the engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScenarioError {
    /// Dot-path to the offending field, e.g. `members.0.name`, `script_order`.
    pub field: String,
    /// Stable machine code (e.g. `empty`, `unknown_id`, `missing`) — frontends
    /// localize the display text off this rather than baking in the English
    /// `message`.
    pub code: String,
    /// English human message; a fallback for frontends without a translation.
    pub message: String,
}

impl BusGroupScenario {
    /// Validate this scenario against the same checks the engine enforces at
    /// `GroupChatSession::build` (`oneai-agent/src/group_chat.rs`: members
    /// non-empty; scripted order / moderator / opener must reference existing
    /// members). This is the *wire-level pre-check* run before a
    /// `Directive::StartGroupChat` submit (and by `scenario/validate` for live
    /// editor feedback); the engine build re-checks on the parsed config as
    /// launch-time defense-in-depth. Returns all problems found (not just the
    /// first) so an editor can flag every field at once.
    pub fn validate(&self) -> Vec<ScenarioError> {
        let mut errs = Vec::new();
        let ids: std::collections::HashSet<&str> =
            self.members.iter().map(|m| m.id.as_str()).collect();

        if self.members.is_empty() {
            errs.push(ScenarioError {
                field: "members".into(),
                code: "empty".into(),
                message: "group chat needs at least one member".into(),
            });
            return errs; // member-id checks below are meaningless with no members
        }
        for (i, m) in self.members.iter().enumerate() {
            if m.name.trim().is_empty() {
                errs.push(ScenarioError {
                    field: format!("members.{i}.name"),
                    code: "empty".into(),
                    message: format!("member {} ({}) is missing a name", i, m.id),
                });
            }
            if m.system_prompt.trim().is_empty() {
                errs.push(ScenarioError {
                    field: format!("members.{i}.system_prompt"),
                    code: "empty".into(),
                    message: format!("member '{}' is missing a system prompt", m.id),
                });
            }
        }
        // turn_policy: "scripted" | "moderator" | anything-else ⇒ round-robin
        // (mirrors the fallthrough parse in ScenarioSpecView::from).
        match self.turn_policy.as_str() {
            "scripted" => {
                if let Some(order) = &self.script_order {
                    for id in order {
                        if !ids.contains(id.as_str()) {
                            errs.push(ScenarioError {
                                field: "script_order".into(),
                                code: "unknown_id".into(),
                                message: format!("scripted order references unknown member '{id}'"),
                            });
                        }
                    }
                }
            }
            "moderator" => {
                let mid = self.moderator_id.as_deref().unwrap_or("");
                if mid.trim().is_empty() {
                    errs.push(ScenarioError {
                        field: "moderator_id".into(),
                        code: "missing".into(),
                        message: "moderator policy requires a moderator_id".into(),
                    });
                } else if !ids.contains(mid) {
                    errs.push(ScenarioError {
                        field: "moderator_id".into(),
                        code: "unknown_id".into(),
                        message: format!("moderator '{mid}' is not a member"),
                    });
                }
            }
            _ => {} // round-robin — no id references to check
        }
        if let Some(op) = self
            .opener_agent_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            if !ids.contains(op) {
                errs.push(ScenarioError {
                    field: "opener_agent_id".into(),
                    code: "unknown_id".into(),
                    message: format!("opener '{op}' is not a member"),
                });
            }
        }
        if let Some(rl) = &self.review_loop {
            if !ids.contains(rl.reviewer_id.as_str()) {
                errs.push(ScenarioError {
                    field: "review_loop.reviewer_id".into(),
                    code: "unknown_id".into(),
                    message: format!("reviewer '{}' is not a member", rl.reviewer_id),
                });
            }
            if rl.max_rounds == 0 {
                errs.push(ScenarioError {
                    field: "review_loop.max_rounds".into(),
                    code: "invalid".into(),
                    message: "review_loop.max_rounds must be at least 1".into(),
                });
            }
        }
        errs
    }

    /// True when [`validate`](Self::validate) finds no problems.
    pub fn is_valid(&self) -> bool {
        self.validate().is_empty()
    }
}

// ─── Rich scenario (shared store / editor unit) ──────────────────────────────
//
// `BusGroupScenario` above is the *engine launch payload* — minimal, the form
// `Directive::StartGroupChat` consumes. The non-Rust frontends' scenario
// *editor* edits a richer model (the macOS `Scenario` Swift struct: cast +
// turn policy + topic-intake fields + a debrief phase + icon/name). To let
// every frontend (macOS / VS Code / browser) share ONE scenario library and
// ONE editor over `scenario/*`, that richer model is promoted here to a
// shared wire DTO: [`BusScenario`]. It mirrors the macOS `Scenario: Codable`
// JSON shape exactly so the Swift struct round-trips it unchanged.
//
// At launch, the frontend compiles a `BusScenario` (+ collected topic values)
// into a `BusGroupScenario` via [`BusScenario::to_group_scenario`], baking the
// topic background into member system prompts — and submits `group/start`.
// The two DTOs stay separate: the engine never sees `topic_fields`/`debrief`.

/// One topic-intake field the user fills before starting a scenario (e.g.
/// "应聘岗位"). `visible_to` = None ⇒ the value is shared background for ALL
/// members; Some(ids) ⇒ only those members see it in their system prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BusTopicField {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Member ids allowed to see this field's value in their background.
    /// None = all members; Some = only those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_to: Option<Vec<String>>,
}

/// Optional "debrief" phase config. After the user triggers the debrief, the
/// turn policy is switched to a scripted order containing only
/// `debrief_member_id`, and `summary_prompt` is sent to that member for a
/// full-session summary. The other members no longer participate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BusDebriefConfig {
    pub button_label: String,
    pub summary_prompt: String,
    pub debrief_member_id: String,
}

/// A scenario member — mirrors [`BusAgentSpec`] plus a UI-only `role` (short
/// label) the engine does not consume. [`BusScenario::to_group_scenario`]
/// drops `role` when compiling the launch payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BusScenarioMember {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub system_prompt: String,
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

/// A multi-agent scenario — the shared store / editor unit over `scenario/*`.
/// Mirrors the macOS `Scenario` Swift struct field-for-field so the two
/// `Codable`s produce/consume the same JSON. The engine consumes the compiled
/// [`BusGroupScenario`] (see [`Self::to_group_scenario`]), never this directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusScenario {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub members: Vec<BusScenarioMember>,
    /// `scripted` | `moderator` | `roundrobin` (mirrors macOS `TurnPolicy`).
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
    pub topic_fields: Option<Vec<BusTopicField>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debrief: Option<BusDebriefConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_loop: Option<BusReviewLoop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<BusLocale>,
}

impl BusScenario {
    /// Validate this rich scenario. Extends [`BusGroupScenario::validate`]
    /// with the editor-only references: each `topic_fields[].visible_to` id
    /// and `debrief.debrief_member_id` must reference an existing member.
    /// Returns all problems found.
    pub fn validate(&self) -> Vec<ScenarioError> {
        let mut errs = Vec::new();
        let ids: std::collections::HashSet<&str> =
            self.members.iter().map(|m| m.id.as_str()).collect();

        if self.name.trim().is_empty() {
            errs.push(ScenarioError {
                field: "name".into(),
                code: "empty".into(),
                message: "scenario is missing a name".into(),
            });
        }
        if self.members.is_empty() {
            errs.push(ScenarioError {
                field: "members".into(),
                code: "empty".into(),
                message: "group chat needs at least one member".into(),
            });
            return errs;
        }
        for (i, m) in self.members.iter().enumerate() {
            if m.name.trim().is_empty() {
                errs.push(ScenarioError {
                    field: format!("members.{i}.name"),
                    code: "empty".into(),
                    message: format!("member {} ({}) is missing a name", i, m.id),
                });
            }
            if m.system_prompt.trim().is_empty() {
                errs.push(ScenarioError {
                    field: format!("members.{i}.system_prompt"),
                    code: "empty".into(),
                    message: format!("member '{}' is missing a system prompt", m.id),
                });
            }
        }
        match self.turn_policy.as_str() {
            "scripted" => {
                if let Some(order) = &self.script_order {
                    for id in order {
                        if !ids.contains(id.as_str()) {
                            errs.push(ScenarioError {
                                field: "script_order".into(),
                                code: "unknown_id".into(),
                                message: format!("scripted order references unknown member '{id}'"),
                            });
                        }
                    }
                }
            }
            "moderator" => {
                let mid = self.moderator_id.as_deref().unwrap_or("");
                if mid.trim().is_empty() {
                    errs.push(ScenarioError {
                        field: "moderator_id".into(),
                        code: "missing".into(),
                        message: "moderator policy requires a moderator_id".into(),
                    });
                } else if !ids.contains(mid) {
                    errs.push(ScenarioError {
                        field: "moderator_id".into(),
                        code: "unknown_id".into(),
                        message: format!("moderator '{mid}' is not a member"),
                    });
                }
            }
            // "roundrobin" or anything else ⇒ round-robin fallthrough (no refs).
            _ => {}
        }
        if let Some(op) = self
            .opener_agent_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            if !ids.contains(op) {
                errs.push(ScenarioError {
                    field: "opener_agent_id".into(),
                    code: "unknown_id".into(),
                    message: format!("opener '{op}' is not a member"),
                });
            }
        }
        if let Some(fields) = &self.topic_fields {
            for (fi, f) in fields.iter().enumerate() {
                if let Some(vis) = &f.visible_to {
                    for vid in vis {
                        if !ids.contains(vid.as_str()) {
                            errs.push(ScenarioError {
                                field: format!("topic_fields.{fi}.visible_to"),
                                code: "unknown_id".into(),
                                message: format!(
                                    "topic field '{}' is visible to unknown member '{vid}'",
                                    f.id
                                ),
                            });
                        }
                    }
                }
            }
        }
        if let Some(db) = &self.debrief {
            if !ids.contains(db.debrief_member_id.as_str()) {
                errs.push(ScenarioError {
                    field: "debrief.debrief_member_id".into(),
                    code: "unknown_id".into(),
                    message: format!("debrief member '{}' is not a member", db.debrief_member_id),
                });
            }
        }
        if let Some(rl) = &self.review_loop {
            if !ids.contains(rl.reviewer_id.as_str()) {
                errs.push(ScenarioError {
                    field: "review_loop.reviewer_id".into(),
                    code: "unknown_id".into(),
                    message: format!("reviewer '{}' is not a member", rl.reviewer_id),
                });
            }
            if rl.max_rounds == 0 {
                errs.push(ScenarioError {
                    field: "review_loop.max_rounds".into(),
                    code: "invalid".into(),
                    message: "review_loop.max_rounds must be at least 1".into(),
                });
            }
        }
        errs
    }

    /// True when [`validate`](Self::validate) finds no problems.
    pub fn is_valid(&self) -> bool {
        self.validate().is_empty()
    }

    /// Compile this rich scenario into the engine launch payload, dropping the
    /// UI-only fields (`icon`, `name`, `role`, `topic_fields`, `debrief`) the
    /// engine does not consume. NOTE: topic-value baking (appending the
    /// collected intake answers to each member's `system_prompt` per
    /// `visible_to`) is the frontend's pre-submit step — it mutates
    /// `system_prompt` on these members *before* calling this. This fn only
    /// maps member→[`BusAgentSpec`] (dropping `role`) and passes the rest
    /// through. Sharing it here keeps every frontend's compile identical.
    pub fn to_group_scenario(&self) -> BusGroupScenario {
        BusGroupScenario {
            members: self
                .members
                .iter()
                .map(|m| BusAgentSpec {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    system_prompt: m.system_prompt.clone(),
                    kind: m.kind.clone(),
                    model: m.model.clone(),
                    api_key: m.api_key.clone(),
                    base_url: m.base_url.clone(),
                    color: m.color.clone(),
                    avatar: m.avatar.clone(),
                })
                .collect(),
            turn_policy: self.turn_policy.clone(),
            script_order: self.script_order.clone(),
            moderator_id: self.moderator_id.clone(),
            opener_agent_id: self.opener_agent_id.clone(),
            opener_line: self.opener_line.clone(),
            title: Some(self.name.clone()),
            review_loop: self.review_loop.clone(),
            locale: self.locale,
        }
    }
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
    /// ⇒ bind to that id (must not already exist). `workspace` = a working-
    /// directory path the user chose (deepseek-harness parity); the engine
    /// persists it in `conversation.metadata["workspace"]` and threads it as
    /// the session's active cwd (see AppSession). `None` ⇒ the app-global cwd.
    /// Result: [`EngineYield::SessionCreated`].
    CreateSession {
        id: Option<String>,
        workspace: Option<String>,
    },
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
        task_id: String,
        task: String,
        agent_kind: BusSubAgentKind,
        speaker: Option<String>,
    },
    /// A delegated sub-agent finished. ← `on_delegate_complete`. Also emitted
    /// by the background-completion sink so a background sub-agent's card is
    /// marked done (the result still arrives separately as a `UserMessage`).
    DelegateComplete {
        turn_id: String,
        task_id: String,
        summary: BusSubAgent,
        speaker: Option<String>,
    },
    /// Mid-run progress from a delegated (incl. background) sub-agent —
    /// iteration / tool-result snapshot / token usage. ← `on_delegate_progress`.
    /// `task_id` matches the `Delegate` event's so a frontend can fan it onto
    /// the right sub-agent card across turns.
    DelegateProgress {
        turn_id: String,
        task_id: String,
        agent_kind: BusSubAgentKind,
        event: BusDelegateProgress,
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
                task_id: "d1".into(),
                task: "".into(),
                agent_kind: BusSubAgentKind::Plan,
                speaker: None,
            },
            EngineYield::DelegateComplete {
                turn_id: "t".into(),
                task_id: "d1".into(),
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
            EngineYield::DelegateProgress {
                turn_id: "t".into(),
                task_id: "d1".into(),
                agent_kind: BusSubAgentKind::Plan,
                event: BusDelegateProgress::IterationStart {
                    iteration: 1,
                    paradigm: BusParadigmKind::ReAct,
                },
            },
        ];
        for f in &fragments {
            // Round-trips. Most variants also carry a `speaker` JSON key;
            // `DelegateProgress` is a mid-run progress event, not a speech
            // act, so it deliberately has no speaker — skip that sub-check.
            let line = serde_json::to_string(f).unwrap();
            if !matches!(f, EngineYield::DelegateProgress { .. }) {
                assert!(
                    line.contains(r#""speaker""#),
                    "variant lost speaker field: {line}"
                );
            }
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

    // ── BusGroupScenario::validate ───────────────────────────────────────

    fn member(id: &str, name: &str, prompt: &str) -> BusAgentSpec {
        BusAgentSpec {
            id: id.into(),
            name: name.into(),
            system_prompt: prompt.into(),
            kind: "openai".into(),
            model: String::new(),
            api_key: None,
            base_url: None,
            color: None,
            avatar: None,
        }
    }

    fn valid_scenario() -> BusGroupScenario {
        BusGroupScenario {
            members: vec![member("a", "A", "prompt-a")],
            turn_policy: "scripted".into(),
            script_order: Some(vec!["a".into()]),
            moderator_id: None,
            opener_agent_id: Some("a".into()),
            opener_line: None,
            title: None,
            review_loop: Some(BusReviewLoop {
                reviewer_id: "a".into(),
                approve_marker: "ok".into(),
                max_rounds: 2,
            }),
            locale: None,
        }
    }

    #[test]
    fn validate_accepts_a_sound_scenario() {
        assert!(valid_scenario().is_valid(), "baseline should be clean");
    }

    #[test]
    fn validate_rejects_empty_members() {
        let mut s = valid_scenario();
        s.members.clear();
        let errs = s.validate();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "members");
        assert_eq!(errs[0].code, "empty");
        // Returns early — no spurious downstream member-id errors.
    }

    #[test]
    fn validate_flags_blank_member_name_and_prompt() {
        let mut s = valid_scenario();
        s.members.push(member("b", "  ", ""));
        let errs = s.validate();
        let fields: Vec<&str> = errs.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"members.1.name"));
        assert!(fields.contains(&"members.1.system_prompt"));
    }

    #[test]
    fn validate_flags_scripted_order_unknown_id() {
        let mut s = valid_scenario();
        s.script_order = Some(vec!["a".into(), "ghost".into()]);
        let errs = s.validate();
        assert_eq!(errs[0].field, "script_order");
        assert_eq!(errs[0].code, "unknown_id");
        assert!(errs[0].message.contains("ghost"));
    }

    #[test]
    fn validate_flags_moderator_missing_and_unknown() {
        // moderator policy with no moderator_id → missing.
        let mut s = valid_scenario();
        s.turn_policy = "moderator".into();
        s.moderator_id = None;
        let errs = s.validate();
        assert_eq!(errs[0].field, "moderator_id");
        assert_eq!(errs[0].code, "missing");

        // moderator_id pointing at a non-member → unknown_id.
        s.moderator_id = Some("nobody".into());
        let errs = s.validate();
        assert_eq!(errs[0].field, "moderator_id");
        assert_eq!(errs[0].code, "unknown_id");
    }

    #[test]
    fn validate_flags_opener_and_reviewer_unknown_and_zero_rounds() {
        let mut s = valid_scenario();
        s.opener_agent_id = Some("outsider".into());
        s.review_loop = Some(BusReviewLoop {
            reviewer_id: "outsider".into(),
            approve_marker: "ok".into(),
            max_rounds: 0,
        });
        let errs = s.validate();
        let codes: Vec<&str> = errs.iter().map(|e| e.code.as_str()).collect();
        // Three independent problems flagged together, not short-circuited.
        assert_eq!(errs.len(), 3);
        assert!(codes.iter().filter(|c| **c == "unknown_id").count() == 2);
        assert!(codes.contains(&"invalid"));
    }

    #[test]
    fn validate_round_robin_needs_no_refs() {
        // Any turn_policy other than scripted/moderator falls through to
        // round-robin — no id refs required, so a lone member is valid.
        let mut s = valid_scenario();
        s.turn_policy = "roundrobin".into();
        s.script_order = None;
        assert!(s.is_valid());
    }

    // ── BusScenario (rich) ──────────────────────────────────────────────

    fn rich_member(id: &str, name: &str) -> BusScenarioMember {
        BusScenarioMember {
            id: id.into(),
            name: name.into(),
            role: None,
            system_prompt: "prompt".into(),
            kind: "openai".into(),
            model: String::new(),
            api_key: None,
            base_url: None,
            color: None,
            avatar: None,
        }
    }

    fn valid_rich() -> BusScenario {
        BusScenario {
            id: "sc1".into(),
            name: "Interview".into(),
            icon: Some("person.2".into()),
            members: vec![rich_member("coach", "Coach")],
            turn_policy: "moderator".into(),
            script_order: None,
            moderator_id: Some("coach".into()),
            opener_agent_id: Some("coach".into()),
            opener_line: None,
            topic_fields: Some(vec![BusTopicField {
                id: "role".into(),
                label: "应聘岗位".into(),
                placeholder: None,
                visible_to: None,
            }]),
            debrief: Some(BusDebriefConfig {
                button_label: "结束".into(),
                summary_prompt: "summarize".into(),
                debrief_member_id: "coach".into(),
            }),
            review_loop: None,
            locale: Some(BusLocale::Zh),
        }
    }

    #[test]
    fn rich_validate_accepts_sound_scenario() {
        assert!(valid_rich().is_valid());
    }

    #[test]
    fn rich_validate_flags_topic_visible_to_unknown_member() {
        let mut s = valid_rich();
        s.topic_fields = Some(vec![BusTopicField {
            id: "secret".into(),
            label: "项目经历".into(),
            placeholder: None,
            visible_to: Some(vec!["ghost".into()]),
        }]);
        let errs = s.validate();
        assert_eq!(errs[0].field, "topic_fields.0.visible_to");
        assert_eq!(errs[0].code, "unknown_id");
    }

    #[test]
    fn rich_validate_flags_debrief_member_unknown() {
        let mut s = valid_rich();
        s.debrief = Some(BusDebriefConfig {
            button_label: "结束".into(),
            summary_prompt: "summarize".into(),
            debrief_member_id: "outsider".into(),
        });
        let errs = s.validate();
        assert_eq!(errs[0].field, "debrief.debrief_member_id");
        assert_eq!(errs[0].code, "unknown_id");
    }

    #[test]
    fn rich_validate_flags_missing_name() {
        let mut s = valid_rich();
        s.name = "  ".into();
        let errs = s.validate();
        assert!(errs.iter().any(|e| e.field == "name" && e.code == "empty"));
    }

    #[test]
    fn to_group_scenario_drops_ui_fields_and_maps_members() {
        let rich = valid_rich();
        let g = rich.to_group_scenario();
        // Engine payload has no topic_fields/debrief/icon/name/role.
        assert_eq!(g.members.len(), 1);
        assert_eq!(g.members[0].id, "coach");
        // No UI-only fields leak into the engine payload's JSON.
        let gj = serde_json::to_string(&g).unwrap();
        assert!(!gj.contains("topic_fields"));
        assert!(!gj.contains("debrief"));
        assert!(!gj.contains("role"));
        // title carries the scenario name; turn_policy/moderator_id preserved.
        assert_eq!(g.title.as_deref(), Some("Interview"));
        assert_eq!(g.turn_policy, "moderator");
        assert_eq!(g.moderator_id.as_deref(), Some("coach"));
    }

    #[test]
    fn rich_scenario_round_trips_through_json() {
        // The Swift Scenario Codable must produce/consume this exact JSON.
        let rich = valid_rich();
        let j = serde_json::to_string(&rich).unwrap();
        let back: BusScenario = serde_json::from_str(&j).unwrap();
        assert_eq!(back.id, rich.id);
        assert_eq!(back.members[0].id, "coach");
        assert_eq!(back.topic_fields.as_ref().unwrap()[0].label, "应聘岗位");
        assert_eq!(back.debrief.unwrap().debrief_member_id, "coach");
    }
}
