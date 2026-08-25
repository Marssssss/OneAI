//! Core trait definitions for the OneAI framework.
//!
//! These traits define the primary abstractions that all components implement:
//! - `LlmProvider`: LLM inference (streaming + non-streaming)
//! - `Tool`: Tool registration and execution
//! - `MemoryStore`: Short-term and long-term memory
//! - `SkillProvider`: Skill selection and management
//! - `PlatformTool`: Platform-specific tool extension
//! - `InteractionGate`: Human-machine collaboration at every loop decision point
//! - `OutputParser`: 3-layer output parsing defense
//! - `StateReducer`: ScopeState reduction for parallel agents
//! - `TaskScheduler`: Platform-independent task scheduling
//! - `CronScheduler`: Durable cron / NL-schedule orchestration (Phase 3.2)
//! - `StatePersistence`: Checkpoint save/load for agent state recovery
//! - `VectorStore` / `VectorBackend` / `KeywordBackend` / `RetrievalBackend`:
//!   pluggable retrieval backends (legacy minimal vs. hybrid-with-filters)
//! - `RerankerProvider`: second-stage cross-encoder rerank

use crate::error::Result;
use crate::platform::Platform;
use crate::types::*;
use crate::types::{HookContext, HookPoint, HookResult};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

// ─── LlmProvider ──────────────────────────────────────────────────────────────

/// The primary abstraction for all LLM interactions.
///
/// Implementations handle provider-specific protocol translation (OpenAI, Anthropic, Ollama, etc.)
/// and expose a uniform interface for inference and streaming.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Perform a complete (non-streaming) inference request.
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse>;

    /// Perform a streaming inference request, returning an SSE stream.
    ///
    /// The stream yields `InferenceStreamChunk` items as they arrive from the provider.
    /// The final chunk will have `is_final = true` and include token usage.
    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>>;

    /// Query the capabilities of the connected model.
    fn capabilities(&self) -> ModelCapability;

    /// Get the model configuration.
    fn config(&self) -> &ModelConfig;

    /// Probe the provider's own model-metadata endpoint for the context window.
    ///
    /// This is the L2 dynamic-detection layer of OneAI's 3-layer context-window
    /// resolution (user config > provider probe > built-in library). Implementations
    /// query endpoints like Ollama `/api/show`, Anthropic `/v1/models/{id}`, or
    /// Gemini `models.get` and return the discovered window size in tokens.
    ///
    /// The default returns `None` so the resolver falls through to the built-in
    /// static library. Probing must be best-effort — network/auth failures
    /// return `None` rather than erroring, so inference is never blocked by a
    /// metadata-endpoint outage.
    async fn probe_context_window(&self) -> Option<u32> {
        None
    }

    /// List the model ids available at the provider's endpoint.
    ///
    /// Implementations query the provider's model-listing endpoint (OpenAI-
    /// compatible `GET /models`, Anthropic `GET /v1/models`, Gemini
    /// `GET /models`, Ollama `GET /api/tags`) and return the discovered model
    /// ids. Powers the settings UI's model dropdown (`provider/models` RPC),
    /// so the user picks from the endpoint's real catalog instead of typing
    /// a model name blind.
    ///
    /// Best-effort like `probe_context_window` — network/auth/parse failures
    /// return an empty list rather than erroring, so a metadata-endpoint
    /// outage never blocks the UI. The default returns empty (this provider
    /// has no model-listing endpoint).
    async fn list_models(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether this backend benefits from constrained/structured-output decoding.
    ///
    /// Encodes a **cost/benefit judgment, not raw capability**: cloud SOTA backends
    /// return `false` (constrained decoding hurts reasoning quality on strong models
    /// for little gain — they already emit valid JSON reliably); local/self-hosted
    /// backends (Ollama small models, vLLM/llama.cpp) return `true` (their invalid-JSON
    /// rate is high enough that grammar-constrained decoding is net positive).
    ///
    /// Honored only when the agent's `ConstrainedOutputPolicy` is `Auto`. Post-hoc
    /// `StructuredOutputConfig` validation + `ModelRetry` runs regardless of this flag.
    /// Default `false` so cloud providers opt out unless they explicitly override.
    fn prefers_constrained_output(&self) -> bool {
        false
    }
}

// ─── Tool ─────────────────────────────────────────────────────────────────────

/// Unified interface for all tools — local, MCP, and platform-specific.
///
/// Each tool has a name, description, parameter schema, and risk level.
/// High-risk tools must pass through the `InteractionGate` (ToolApproval) before execution.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's unique name.
    fn name(&self) -> &str;

    /// Human-readable description of what the tool does.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// The risk level of this tool's operations.
    fn risk_level(&self) -> RiskLevel;

    /// Whether this tool's backing service is available right now.
    ///
    /// The **`check_fn` Footprint gate**: a tool whose prerequisites are unmet
    /// (no API key configured, backing binary missing, MCP server disconnected,
    /// feature flag off) returns `false` here. The `AgentLoop` excludes such
    /// tools **entirely** from the schema sent to the model — zero footprint,
    /// not merely "disabled" — so the model never sees a broken option it
    /// would otherwise try to call. This keeps the per-domain / per-config
    /// tool table small and focused, improving routing quality and lowering
    /// prompt tokens.
    ///
    /// Default `true`: existing tools remain visible unless they opt into
    /// gating by overriding this (or are wrapped via a `GatedTool`
    /// registration-level `check_fn`). This is a non-`async` check by design
    /// — it runs on the tool-definition hot path every iteration.
    fn service_available(&self) -> bool {
        true
    }

    /// How this tool is exposed to the model and to code mode (#27).
    ///
    /// See [`crate::ToolExposure`]. Default [`crate::ToolExposure::Direct`]:
    /// existing tools remain visible everywhere (zero behavior change). The
    /// effective value at a given site is resolved through an
    /// [`ExposureResolver`] (a DomainPack `PermissionProfile.tool_exposure`
    /// map overrides this) — see [`effective_exposure`].
    ///
    /// Like [`service_available`](Self::service_available), this is a
    /// non-`async` check by design — it runs on the tool-definition hot path
    /// every iteration.
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    /// Execute the tool with the given arguments.
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput>;
}

// ─── PermissionResolver ──────────────────────────────────────────────────────

/// Resolve the domain-level permission action for a tool call.
///
/// This is the seam by which a `PermissionProfile` (defined in `oneai-domain`,
/// which depends on `oneai-tool`) is injected into the **tool-execution
/// paths** that live below it — `ToolExecutor` (`oneai-tool`) and the workflow
/// `execute_step` path (`oneai-workflow`) — without inverting the dependency
/// direction. The holder (executor) calls `resolve()` before dispatch; a
/// `Deny` short-circuits, `AutoApprove` skips the interaction gate,
/// `RequireConfirmation` forces it, and `UseDefaultPermission` falls back to
/// the tool's own permission level.
///
/// Implementations: `PermissionProfile` (and `MergedDomainPack` via its
/// `resolve_permission`). When `None`, executors fall back to per-tool
/// `risk_level()` — the pre-existing behaviour.
pub trait PermissionResolver: Send + Sync {
    /// Resolve the permission action for a `(tool_name, args)` call.
    fn resolve(&self, tool_name: &str, args: &serde_json::Value) -> PermissionAction;
}

// ─── ExposureResolver ────────────────────────────────────────────────────────

/// Resolve the effective [`ToolExposure`] of a tool — the seam by which a
/// DomainPack's `PermissionProfile.tool_exposure` map overrides a tool's own
/// [`Tool::exposure`] without inverting the dependency direction (`oneai-tool`
/// and `oneai-agent` hold this trait object, not `oneai-domain`).
///
/// Mirrors [`PermissionResolver`]: the impl lives in `oneai-domain`
/// (`PermissionProfile`, `MergedDomainPack`); the holders are the four
/// enforcement sites — the model-schema builder, the agent dispatch path,
/// the code-mode bridge tool list, and the `tool_search` discovery tool.
/// When `None`, the caller falls back to `tool.exposure()` directly — see
/// [`effective_exposure`].
pub trait ExposureResolver: Send + Sync {
    /// Resolve the effective exposure for `(tool_name, tool)`. The `tool`
    /// reference is supplied so the resolver can fall back to the tool's own
    /// [`Tool::exposure`] when no override is configured.
    fn resolve_exposure(&self, tool_name: &str, tool: &dyn Tool) -> ToolExposure;
}

/// The effective exposure of `tool`: the resolver's override (if any) wins,
/// otherwise the tool's own [`Tool::exposure`].
///
/// This is the single function the four enforcement sites call — it is the
/// one source of truth for "what exposure applies here", so a DomainPack
/// override and a tool-impl default never diverge.
pub fn effective_exposure(
    resolver: Option<&dyn ExposureResolver>,
    tool: &dyn Tool,
) -> ToolExposure {
    resolver
        .map(|r| r.resolve_exposure(tool.name(), tool))
        .unwrap_or_else(|| tool.exposure())
}

// ─── CommandReviewer ─────────────────────────────────────────────────────────

/// Content-level safety review of a tool call — the Guardian layer
/// (#28 Stage 2).
///
/// Where [`PermissionResolver`] decides *which tools* need approval (a
/// domain-level static classification), `CommandReviewer` inspects the call's
/// **content** — the shell command string, the Python script body — and
/// classifies it as [`Verdict::Allow`] / [`Verdict::Deny`] /
/// [`Verdict::Escalate`]. The executor then applies the [`ApprovalPolicy`]
/// matrix to that verdict to decide Run / Deny / Prompt.
///
/// The trait is `async` so an implementation may delegate an `Escalate` to an
/// LLM sub-inference (the `LlmGuardian` in `oneai-agent`); the rule-based
/// `RuleGuardian` in `oneai-tool` is sync-classified and returns immediately.
///
/// Defined in `oneai-core` (alongside `PermissionResolver`) so `oneai-tool`'s
/// executor can hold it without inverting the dependency direction; impls live
/// in `oneai-tool` (rule) and `oneai-agent` (LLM fallback). When `None`, the
/// executor skips the Guardian entirely — the pre-Stage-2 behaviour.
#[async_trait::async_trait]
pub trait CommandReviewer: Send + Sync {
    /// Classify a tool call. `args` is the raw tool args; the reviewer knows
    /// which fields carry the command/script for the tools it reviews.
    async fn review(&self, tool_name: &str, args: &serde_json::Value) -> Verdict;
}

// ─── DataLayerReloader ──────────────────────────────────────────────────────

/// Reload an agent's runtime **data layer** mid-session without restart
/// (evolution-plan §3.4 — the `/reload`-equivalent).
///
/// A DomainPack's *Rust* structure is compile-time and not reloadable, but
/// its **data files** — discovered skill markdown, MCP server tool
/// registrations, (future) `MemoryProfile` JSON / `StateGraph` definitions —
/// can be re-read at runtime. The model triggers this via the `reload` tool
/// (or the CLI `oneai reload`); refresh of the visible tool/skill tables is
/// automatic because the `AgentLoop` reads the live `ToolRegistry` /
/// `SkillRegistry` every turn.
///
/// The trait lives in `oneai-core` (no references to skill/mcp types) so the
/// agent layer can hold it without inverting the dependency direction; the
/// concrete impl (`AppDataLayerReloader`) lives in `oneai-app`, which already
/// depends on `oneai-skill` / `oneai-mcp` / `oneai-tool`.
///
/// Returns the names of items (re-)loaded/registered so the caller can log
/// the reload event and surface it to the model. **OnResume reconcile is a
/// no-op** — reload mutates shared registry maps, not `LoopState` or the
/// working-state event log; the resume path (which rehydrates working state
/// from the event log) is untouched.
#[async_trait]
pub trait DataLayerReloader: Send + Sync {
    /// Re-read the runtime data layer (skills, MCP tools, …) and register /
    /// re-register what changed. Returns the names of items (re-)loaded.
    async fn reload_data_layer(&self) -> Result<Vec<String>>;
}

// ─── MemoryStore ──────────────────────────────────────────────────────────────

/// Abstraction for both short-term and long-term memory.
///
/// Short-term memory uses sliding window with in-memory storage.
/// Long-term memory uses vector storage with hybrid scoring
/// (semantic similarity + temporal proximity).
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Store a new memory entry.
    async fn store(&self, entry: MemoryEntry) -> Result<()>;

    /// Retrieve memory entries matching the query.
    async fn retrieve(&self, query: &MemoryQuery, top_k: usize) -> Result<Vec<MemoryEntry>>;

    /// Compress memory entries when they exceed a threshold.
    /// Returns the entries that were summarized/removed.
    async fn compress(&self, threshold: usize) -> Result<Vec<MemoryEntry>>;

    /// Clear all stored entries.
    async fn clear(&self) -> Result<()>;
}

// ─── SkillProvider ────────────────────────────────────────────────────────────

/// Skill selection and management.
///
/// The SKILL Selector uses lightweight vector/keyword matching to dynamically
/// inject the most relevant skill descriptions into the agent's context.
/// Skills are progressively disclosed and auto-unloaded when the topic changes.
#[async_trait]
pub trait SkillProvider: Send + Sync {
    /// Select the most relevant skills for a given user input.
    async fn select_skills(&self, user_input: &str, top_k: usize) -> Result<Vec<SkillDescriptor>>;

    /// Register a new skill.
    fn register_skill(&self, skill: SkillDescriptor) -> Result<()>;

    /// Remove a skill by name.
    fn remove_skill(&self, name: &str) -> Result<()>;

    /// List all registered skills.
    fn list_skills(&self) -> Result<Vec<SkillDescriptor>>;
}

// ─── PlatformTool ─────────────────────────────────────────────────────────────

/// Platform-specific tool interface.
///
/// Extends the base `Tool` trait with platform identification.
/// Platform tools are implemented per platform in the `platforms/` directory.
pub trait PlatformTool: Tool {
    /// The platform this tool is designed for.
    fn platform(&self) -> Platform;
}

// ─── InteractionGate ──────────────────────────────────────────────────────────

/// Unified interaction gate — the single surface for every "agent loop suspends
/// → asks the application layer → resumes with a reply" decision point.
///
/// Covers tool approval (PreInfer/PostInfer/ToolApproval), planning tradeoffs
/// (PlanDecision), and final plan confirmation (PlanReview). The application
/// layer decides per-point whether to actually call back to the UI via
/// [`enabled`](Self::enabled); points that return `false` are short-circuited by
/// the loop with zero latency (no lock taken, no channel send).
///
/// Implementations:
/// - `NoopInteractionGate` — every point `enabled()==false`; the zero-latency
///   default.
/// - `ChannelInteractionGate` — mpsc+oneshot bridge to an external UI thread,
///   configurable per-point via `InteractionGateConfig`.
/// - `ThresholdInteractionGate` — low-risk tools auto-proceed, the rest go to
///   the channel.
#[async_trait]
pub trait InteractionGate: Send + Sync {
    /// Block at the decision point until the application layer replies.
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse>;

    /// Whether this point should call back to the application layer.
    ///
    /// Returning `false` lets the loop skip the entire interaction block — no
    /// lock acquisition, no channel send, no allocation. This is the lever that
    /// lets a TUI enable `PlanDecision`/`PlanReview`/`ToolApproval` while leaving
    /// `PreInfer`/`PostInfer` off (no per-iteration interruption). The default
    /// returns `true`; `NoopInteractionGate` overrides it to `false` for all
    /// points.
    fn enabled(&self, _point: InteractionPoint) -> bool {
        true
    }
}

// ─── OutputParser ─────────────────────────────────────────────────────────────

/// 3-layer output parsing defense trait.
///
/// Layer 1: Constrained decoding (BNF grammar) — guarantees correct format at generation.
/// Layer 2: Fuzzy JSON repair — repairs malformed output (bracket closing, regex extraction).
/// Layer 3: Fallback self-correction — re-feeds error message to model for re-generation.
#[async_trait]
pub trait OutputParser: Send + Sync {
    /// Parse raw model output into structured content blocks.
    ///
    /// Applies the 3-layer defense automatically:
    /// 1. If constrained decoding is active, the output is already correct (Layer 1).
    /// 2. If not, attempt fuzzy repair (Layer 2).
    /// 3. If repair fails, trigger fallback self-correction (Layer 3).
    async fn parse<'a>(
        &self,
        raw_output: &str,
        schema: Option<&'a serde_json::Value>,
    ) -> Result<ParsedOutput>;

    /// Repair a raw tool-args JSON string (Layer 2 fuzzy repair).
    ///
    /// The agent loop calls this on every tool-call's raw args before dispatch.
    /// The default implementation is a strict `serde_json::from_str`; the
    /// `ThreeLayerParser` overrides it to run Layer 2 fuzzy repair (closing
    /// unclosed brackets, extracting embedded JSON) so mildly-malformed args
    /// are recovered instead of being fed back to the model as errors.
    /// Truly unrepairable args still return `Err` — the agent loop surfaces
    /// that as a Reflexion-style self-correction prompt (Layer 3).
    fn repair_tool_args(&self, raw: &str) -> Result<serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(raw).map_err(|e| {
            crate::error::OneAIError::Parser(crate::error::ParserError::FuzzyRepairFailed(
                e.to_string(),
            ))
        })
    }
}

// ─── ConstrainedDecoder ───────────────────────────────────────────────────────

/// Layer 1: Constrained decoding trait.
///
/// Implementations activate BNF/JSON Schema grammar constraints on providers
/// that support them (LiteRT-LM, Ollama, llama.cpp).
pub trait ConstrainedDecoder: Send + Sync {
    /// Whether constrained decoding is available for the current provider.
    fn is_available(&self) -> bool;

    /// Apply constrained decoding to an inference request.
    fn apply_constraint(&self, req: &mut InferenceRequest, grammar: &str) -> Result<()>;
}

// ─── StateReducer ─────────────────────────────────────────────────────────────

/// Merges sub-agent reductions (ScopeState) back into the global state.
///
/// Implements the MVI/Redux pattern for parallel agent execution.
/// Sub-agents run in isolated Sandbox Scopes with read-only global memory;
/// their results are merged back via this reducer.
pub trait StateReducer: Send + Sync {
    /// Merge a set of reductions into the global state.
    fn reduce(&self, global: &mut GlobalState, reductions: Vec<Reduction>) -> Result<()>;
}

// ─── GlobalState ──────────────────────────────────────────────────────────────

/// The global state shared across all agents in a session.
///
/// Contains the main conversation, memory entries, and shared context variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalState {
    /// The main conversation.
    pub conversation: Conversation,

    /// Global memory entries.
    pub memory: Vec<MemoryEntry>,

    /// Shared context variables (key-value pairs).
    pub context: HashMap<String, String>,

    /// Results from completed sub-agent steps.
    pub step_results: HashMap<String, ContentBlock>,
}

impl GlobalState {
    /// Create a new empty global state.
    pub fn new() -> Self {
        Self {
            conversation: Conversation::new(),
            memory: Vec::new(),
            context: HashMap::new(),
            step_results: HashMap::new(),
        }
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Reduction ────────────────────────────────────────────────────────────────

/// Describes how a sub-agent's result should be merged into the global state.
///
/// Sub-agents produce reductions in their isolated ScopeState;
/// the StateReducer applies these to the global state after parallel execution completes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Reduction {
    /// Append a memory entry to global memory.
    AppendMemory { entry: MemoryEntry },

    /// Update a shared context variable.
    UpdateContext { key: String, value: String },

    /// Set the result for a specific plan step.
    SetResult {
        step_id: String,
        result: ContentBlock,
    },
}

// ─── TaskScheduler ────────────────────────────────────────────────────────────

/// Platform-independent task scheduling.
///
/// Core layer provides a standard async delay trigger.
/// Platform adapters implement native scheduling:
/// - Android: WorkManager
/// - HarmonyOS: WorkScheduler
/// - Desktop: Daemon process
#[async_trait]
pub trait TaskScheduler: Send + Sync {
    /// Schedule a one-shot task with a delay.
    async fn schedule_one_shot(
        &self,
        task: ScheduledTask,
        delay: std::time::Duration,
    ) -> Result<TaskHandle>;

    /// Schedule a periodic task with an interval.
    async fn schedule_periodic(
        &self,
        task: ScheduledTask,
        interval: std::time::Duration,
    ) -> Result<TaskHandle>;

    /// Cancel a scheduled task.
    async fn cancel(&self, handle: &TaskHandle) -> Result<()>;
}

// ─── CronScheduler ────────────────────────────────────────────────────────────

/// Durable cron / NL-schedule orchestration (Phase 3.2).
///
/// The minimal trait surface a host (the CLI `cron serve`, the supervisor) needs
/// to *start* a scheduler; everything else is a safe default. Concrete
/// providers live in `oneai-scheduler` (`CronSchedulerImpl` backed by a
/// [`JobStore`] — in-memory or `FileJobStore` JSONL — plus a delivery `CronRunner`
/// seam). `AppBuilder::cron_provider(...)` holds an `Arc<dyn CronScheduler>` so
/// future agent tools can query schedules; the CLI drives the lifecycle.
///
/// This mirrors the ABC+orchestrator pattern of the gateway's `GatewayRunner`:
/// the trait is the minimal seam, the impl owns the rich API (`add_job` / `list`
/// / `remove`). Capability flags are default trait methods (key portability
/// lesson from `MessagePlatform`) — see [`Self::supports_external_trigger`].
///
/// [`JobStore`]: ../../oneai_scheduler/trait.JobStore.html
#[async_trait]
pub trait CronScheduler: Send + Sync {
    /// Provider name (e.g. `"in-memory"`, `"file"`).
    fn name(&self) -> &str;

    /// Start the scheduler (spawn the ticker loop / arm timers). Idempotent.
    async fn start(&self) -> Result<()>;

    /// Fire all jobs due at or before `now`. Default: no-op (a non-ticking
    /// provider). Returns the number of jobs fired.
    async fn fire_due(&self, _now: chrono::DateTime<chrono::Utc>) -> Result<u32> {
        Ok(0)
    }

    /// Reconcile in-memory state with the persistent store after a restart
    /// (re-arm timers, surface missed one-shots). Default: no-op.
    async fn reconcile(&self) -> Result<()> {
        Ok(())
    }

    /// Notify the scheduler that jobs changed (added / removed / edited) so it
    /// can re-arm. Default: no-op (a provider that polls the store on each
    /// tick ignores this).
    async fn on_jobs_changed(&self) -> Result<()> {
        Ok(())
    }

    /// Whether this provider accepts external one-shot triggers over HTTP
    /// (`POST /cron/fire`). Default: `false` (only the file-backed provider
    /// that owns the axum receiver returns `true`).
    fn supports_external_trigger(&self) -> bool {
        false
    }

    // ─── Job management (the agent-tool seam) ──────────────────────────────
    //
    // These let an agent `schedule` tool create / inspect / remove / manually
    // fire cron jobs — so a user can say "每天9点总结commits" in chat and the
    // agent wires it. Safe defaults: a minimal provider that only ticks
    // returns "unsupported" / empty. The concrete `CronSchedulerImpl` impls
    // these against its [`JobStore`]. Uses the core-level [`CronJobSpec`] so
    // `oneai-core` need not depend on `oneai-scheduler`'s richer `CronJob`.

    /// Add (or replace) a job from a spec. The impl parses `spec.schedule`
    /// and arms `next_fire_at`. Returns the job id (generated if `spec.id`
    /// is empty). Default: unsupported.
    async fn add_job(&self, _spec: CronJobSpec) -> Result<String> {
        Err(crate::error::OneAIError::Other(
            "cron provider does not support add_job".to_string(),
        ))
    }

    /// List all jobs as specs. Default: empty.
    async fn list_jobs(&self) -> Result<Vec<CronJobSpec>> {
        Ok(Vec::new())
    }

    /// Remove a job by id. Returns whether it existed. Default: false.
    async fn remove_job(&self, _id: &str) -> Result<bool> {
        Ok(false)
    }

    /// Manually fire a job now (force — ignores the due window; the impl
    /// routes through its delivery `CronRunner`). Returns whether it fired.
    /// Default: false.
    async fn trigger_job(&self, _id: &str) -> Result<bool> {
        Ok(false)
    }
}

// ─── CronJobSpec ──────────────────────────────────────────────────────────────

/// A core-level cron job spec — primitive fields only, so the
/// [`CronScheduler`] trait can expose job management to agent tools without
/// `oneai-core` depending on `oneai-scheduler`'s richer `CronJob`. The impl
/// parses `schedule` (`"30m"` / `"every 2h"` / ISO / 5-field cron) and adds
/// the store-internal `next_fire_at` / `last_fired_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJobSpec {
    /// Job id. Empty → the provider generates one (uuid).
    #[serde(default)]
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Raw schedule dialect, parsed by the provider.
    pub schedule: String,
    /// The task / prompt to deliver into the agent turn on each fire.
    pub task: String,
    /// Originating platform name (for `deliver=origin`). Default `loopback`.
    #[serde(default)]
    pub platform: String,
    /// Originating channel (raw) to relay the reply to. Default empty.
    #[serde(default)]
    pub channel: String,
    /// Session id to deliver into. Default empty → provider mints one.
    #[serde(default)]
    pub session_id: String,
    /// Bound DomainPack. Default empty → `coding`.
    #[serde(default)]
    pub pack: String,
    /// Originating user id. Default empty.
    #[serde(default)]
    pub user_id: String,
    /// `"origin"` (relay reply to the channel) or `"silent"`. Default `origin`.
    #[serde(default = "cron_default_deliver")]
    pub deliver: String,
    /// Enabled flag. Default true.
    #[serde(default = "cron_default_true")]
    pub enabled: bool,
    /// Arbitrary metadata.
    #[serde(default)]
    pub metadata: std::collections::HashMap<String, String>,
}

fn cron_default_deliver() -> String {
    "origin".to_string()
}

fn cron_default_true() -> bool {
    true
}

// ─── ScheduledTask / TaskHandle ───────────────────────────────────────────────

/// A task to be scheduled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    /// Unique task identifier.
    pub id: String,

    /// Human-readable task name.
    pub name: String,

    /// The task payload (serialized agent state or workflow config).
    pub payload: String,

    /// Task metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// A handle to a scheduled task (for cancellation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHandle {
    /// The task ID.
    pub task_id: String,

    /// Platform-specific scheduling identifier.
    pub platform_handle: String,
}

// ─── HostAllowlistStore ─────────────────────────────────────────────────────

/// A store of hosts the user has approved (or denied) for sandboxed egress.
///
/// The local `NetworkProxy` consults this store before tunnelling a sandboxed
/// process's outbound connection. An approved host is tunneled straight
/// through; a denied host is blocked without re-prompting. Hosts the user
/// admitted via the `InteractionRequest::NetworkApproval` prompt are recorded
/// here so subsequent connections to the same host don't re-prompt.
///
/// The v1 contract is session-scoped (in-memory); a durable, SQLite-backed
/// implementation lives in `oneai-persistence`. The deny side (`is_denied` /
/// `add_denied`) defaults to "no record" so existing implementations stay
/// compatible — a store that doesn't track denials simply never denies on
/// that basis (the proxy's gate prompt still applies).
///
/// `host` is the bare hostname (no port, lowercased by the caller).
#[async_trait]
pub trait HostAllowlistStore: Send + Sync {
    /// Whether `host` is on the approved list.
    async fn is_allowed(&self, host: &str) -> bool;

    /// Add `host` to the approved list (idempotent).
    async fn add(&self, host: String);

    /// Whether `host` has been explicitly denied. Default: no record — a
    /// store that doesn't track denials leaves this false so the proxy falls
    /// through to its gate-prompt path.
    async fn is_denied(&self, _host: &str) -> bool {
        false
    }

    /// Record `host` as denied so future tunnel attempts are blocked without
    /// re-prompting. Default: no-op — stores that don't persist denials simply
    /// drop the record (the next attempt re-prompts).
    async fn add_denied(&self, _host: String) {}
}

// ─── StatePersistence ─────────────────────────────────────────────────────────

/// State persistence for checkpointing and recovery.
///
/// Used to save agent/workflow state when interrupted,
/// and recover it when the session resumes.
#[async_trait]
pub trait StatePersistence: Send + Sync {
    /// Save a checkpoint of the current agent state.
    async fn save_checkpoint(&self, state: &AgentState) -> Result<String>;

    /// Load a checkpoint by ID.
    async fn load_checkpoint(&self, id: &str) -> Result<AgentState>;

    /// List all available checkpoints.
    async fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>>;

    /// Delete a checkpoint by ID.
    async fn delete_checkpoint(&self, id: &str) -> Result<()>;
}

// ─── WorkingStateStore ──────────────────────────────────────────────────────

/// Durable store for agent **working state** — the cross-session "what am I
/// working on" object (goal, steps/progress, decisions, blockers, notes).
///
/// Unlike `StatePersistence` (which snapshots full `AgentState` per session),
/// `WorkingStateStore` persists working state **independently of any session
/// transcript**, as an append-only per-task event log. This is what lets a
/// brand-new session discover and continue an unfinished task from a previous
/// session (reference doc §6.2): the new session reads this store, not the old
/// session's conversation.
///
/// The in-memory `WorkingState` held in `LoopState` is a *projection* derived
/// from the event log; this store is the source of truth. `append_event` is
/// the only write path; `derive_state` rebuilds a projection from events
/// (used on startup / crash recovery). The hot read path (per-turn pinned
/// re-injection) uses the in-memory projection, not this store — so there is
/// zero IO per turn.
#[async_trait]
pub trait WorkingStateStore: Send + Sync {
    /// Create a new task, appending a `TaskCreated` event. Returns the new
    /// task id.
    async fn create_task(
        &self,
        user_id: &str,
        project: &str,
        goal: &str,
        intent: &str,
        session_id: &str,
    ) -> Result<String>;

    /// Get the full derived working state for a task (rebuild from events).
    async fn get_task(&self, task_id: &str) -> Result<Option<WorkingState>>;

    /// List open (Active / Paused) tasks for a (user, project) — reads the
    /// lightweight index, does not derive each task. Cross-session discovery.
    async fn list_open_tasks(&self, user_id: &str, project: &str) -> Result<Vec<TaskBrief>>;

    /// Append one event to the task's log. The only write path. Also updates
    /// the index. Returns the event id.
    async fn append_event(
        &self,
        task_id: &str,
        session_id: &str,
        parent_event_id: Option<&str>,
        event_type: TaskEventType,
        payload: TaskEventPayload,
    ) -> Result<String>;

    /// Rebuild the working-state projection from the event log (latest
    /// `Snapshot` + tail). Used on startup / crash recovery.
    async fn derive_state(&self, task_id: &str) -> Result<WorkingState>;

    /// Fold old events into a `Snapshot` event when the log exceeds the
    /// threshold, keeping `keep_recent` recent events. Idempotent / no-op
    /// when under threshold.
    async fn compact_if_needed(&self, task_id: &str) -> Result<()>;

    /// Archive a task: gzip the event log, mark it `Archived` in the index.
    async fn archive_task(&self, task_id: &str) -> Result<()>;
}

// ─── AgentState / CheckpointInfo ──────────────────────────────────────────────

/// The full state of an agent session, for checkpointing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    /// Unique session identifier.
    pub session_id: String,

    /// The global state at the time of checkpoint.
    pub global_state: GlobalState,

    /// The agent paradigm that was active.
    pub active_paradigm: String,

    /// The step in the workflow/plan that was being executed.
    #[serde(default)]
    pub active_step: Option<String>,

    /// Timestamp of the checkpoint.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Metadata about a saved checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointInfo {
    /// The checkpoint ID.
    pub id: String,

    /// The session ID this checkpoint belongs to.
    pub session_id: String,

    /// When the checkpoint was created.
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Brief description of what was checkpointed.
    pub description: String,
}

// ─── LifecycleHook ────────────────────────────────────────────────────────────

/// A lifecycle hook that runs at specific points in the agent loop.
///
/// Lifecycle hooks are the evolution from InteractionGate's "围栏式安全"
/// (gate-based: approve/deny before execution) to "生命周期安全"
/// (event-driven: allow/deny/modify at every lifecycle stage).
///
/// Inspired by Claude Code's hooks system (PreToolUse/PostToolUse/Notification/Stop),
/// OneAI extends this to include inference lifecycle hooks (PreInfer/PostInfer).
///
/// Hooks can:
/// - **Allow**: Proceed without changes (audit/logging hooks)
/// - **Deny**: Block the action (safety/policy hooks)
/// - **Modify**: Transform the parameters (constraint enforcement hooks)
///
/// Multiple hooks can be registered at the same point. They execute in
/// registration order. For PreToolUse: if any hook returns Deny, the overall
/// result is Deny; if any hook returns Modify, the last Modify's args win.
#[async_trait]
pub trait LifecycleHook: Send + Sync {
    /// The hook points where this hook should be triggered.
    /// A hook can register at multiple points (e.g., a logging hook
    /// at both PreToolUse and PostToolUse).
    fn points(&self) -> Vec<HookPoint>;

    /// Run the hook at the given context.
    /// Returns a HookResult indicating whether to allow, deny, or modify.
    async fn run(&self, context: HookContext) -> HookResult;

    /// Unique name for this hook (for logging/debugging/identification).
    fn name(&self) -> &str;
}

// ─── VectorStore ──────────────────────────────────────────────────────────────

/// Abstraction for vector storage, allowing swap between embedded and remote implementations.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Store a vector with associated metadata.
    async fn upsert(
        &self,
        id: &str,
        embedding: Vec<f32>,
        metadata: HashMap<String, String>,
    ) -> Result<()>;

    /// Search for vectors similar to the query embedding.
    async fn search(
        &self,
        query_embedding: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<VectorSearchResult>>;

    /// Delete a vector by ID.
    async fn delete(&self, id: &str) -> Result<()>;
}

/// A result from vector similarity search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchResult {
    /// The ID of the matching vector.
    pub id: String,

    /// Similarity score (0.0 to 1.0).
    pub score: f32,

    /// Associated metadata.
    pub metadata: HashMap<String, String>,
}

// ─── Retrieval backend abstractions ─────────────────────────────────────────
//
// The legacy `VectorStore` trait above is intentionally minimal (upsert/search/
// delete, no filters, no keyword path) and is preserved for backward
// compatibility. The traits below supersede it for real retrieval work:
//
// - `VectorBackend`  — low-level dense vector ANN/KNN with metadata filtering
// - `KeywordBackend`  — low-level lexical (BM25 / learned-sparse) search
// - `RetrievalBackend`— composite hybrid retrieval (what app-supplied backends
//                       like Qdrant implement directly, bypassing the
//                       framework's RRF fusion when the backend does hybrid
//                       natively)
// - `RerankerProvider` — second-stage cross-encoder rerank
//
// Design principle: these traits MUST NOT leak storage-internal types (no
// sqlite connection, no tantivy index handle, no ort session). An app that
// brings its own Qdrant/Milvus/pgvector/Elasticsearch implements against the
// public trait surface, not against framework internals.
//
// Non-negotiable default: the framework's reference pipeline
// (`oneai_vector::StandardRetrievalPipeline`) runs BM25 + dense → RRF(k=60)
// → rerank (top-150 → top-K), which Anthropic's "Contextual Retrieval"
// evaluation showed cuts top-20 retrieval failure rate by 67%. An app that
// implements its own `RetrievalBackend` and skips the keyword/sparse leg will
// see measurable degradation on Chinese short queries and unique-identifier
// lookups (error codes, proper nouns) — the trait docs say so explicitly so
// that dropping BM25 is an informed decision, not a silent downgrade.

/// Metadata attached to a stored vector or document — a flat string map.
pub type Metadata = HashMap<String, String>;

/// Metadata filter applied to a retrieval request.
///
/// All predicates are AND-combined. Empty filter = no filtering.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Filter {
    /// Require `metadata[key] == value` for every entry.
    pub metadata_eq: Metadata,
    /// Require `metadata[key]` to be one of the listed values.
    pub metadata_in: HashMap<String, Vec<String>>,
}

impl Filter {
    /// Create an empty (match-all) filter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Require `metadata[key] == value`.
    pub fn with_eq(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata_eq.insert(key.into(), value.into());
        self
    }

    /// Require `metadata[key] ∈ values`.
    pub fn with_in(mut self, key: impl Into<String>, values: Vec<String>) -> Self {
        self.metadata_in.insert(key.into(), values);
        self
    }

    /// Whether the filter matches a given metadata map.
    pub fn matches(&self, meta: &Metadata) -> bool {
        for (k, v) in &self.metadata_eq {
            if meta.get(k) != Some(v) {
                return false;
            }
        }
        for (k, vs) in &self.metadata_in {
            match meta.get(k) {
                Some(mv) if vs.iter().any(|v| v == mv) => {}
                _ => return false,
            }
        }
        true
    }
}

/// Which retrieval legs to run for a [`RetrievalRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SearchMode {
    /// Dense vector ANN/KNN only.
    Vector,
    /// Lexical (BM25 / sparse) only.
    Keyword,
    /// Run both legs and fuse (RRF by default).
    #[default]
    Hybrid,
}

/// How to fuse multi-leg retrieval results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FusionMode {
    /// Reciprocal Rank Fusion (Cormack et al. 2009): `Σ 1/(k + rank)`.
    ///
    /// `k` defaults to 60 (the empirical constant from the original paper,
    /// used by Weaviate/Milvus and Anthropic's reference pipeline). Per-leg
    /// weights default to 1.0; supply weights to bias dense vs. lexical.
    Rrf {
        /// RRF constant. Default 60.
        k: u32,
        /// Optional per-leg weights, aligned with the legs' result order.
        weights: Option<Vec<f32>>,
    },
    /// Distribution-Based Score Fusion (Qdrant v1.11): 3-sigma normalize each
    /// leg's raw scores then sum. Only use when retrievers are well-calibrated
    /// and an eval set confirms it beats weighted RRF.
    Dbsf,
}

impl Default for FusionMode {
    fn default() -> Self {
        FusionMode::Rrf {
            k: 60,
            weights: None,
        }
    }
}

/// A single retrieval query — describes what to search for and how.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RetrievalRequest {
    /// The text query (used for the lexical leg and, if no embedding is
    /// supplied, as the basis for the dense leg by an external embedder).
    pub text: String,
    /// Optional pre-computed query embedding for the dense leg. `None` means
    /// the caller expects the backend/pipeline to compute it via an
    /// `EmbeddingService`; a backend that cannot will run keyword-only.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    /// Maximum results to return after fusion + rerank.
    #[serde(default = "default_retrieval_top_k")]
    pub top_k: usize,
    /// Optional metadata filter (AND-combined).
    #[serde(default)]
    pub filter: Option<Filter>,
    /// Which legs to run.
    #[serde(default)]
    pub mode: SearchMode,
    /// How to fuse the legs.
    #[serde(default)]
    pub fusion: FusionMode,
}

fn default_retrieval_top_k() -> usize {
    5
}

impl RetrievalRequest {
    /// Keyword-only retrieval.
    pub fn keyword(text: impl Into<String>, top_k: usize) -> Self {
        Self {
            text: text.into(),
            embedding: None,
            top_k,
            filter: None,
            mode: SearchMode::Keyword,
            fusion: FusionMode::default(),
        }
    }

    /// Vector-only retrieval with a pre-computed query embedding.
    pub fn vector(text: impl Into<String>, embedding: Vec<f32>, top_k: usize) -> Self {
        Self {
            text: text.into(),
            embedding: Some(embedding),
            top_k,
            filter: None,
            mode: SearchMode::Vector,
            fusion: FusionMode::default(),
        }
    }

    /// Hybrid retrieval (dense + lexical, fused via RRF).
    pub fn hybrid(text: impl Into<String>, embedding: Vec<f32>, top_k: usize) -> Self {
        Self {
            text: text.into(),
            embedding: Some(embedding),
            top_k,
            filter: None,
            mode: SearchMode::Hybrid,
            fusion: FusionMode::default(),
        }
    }

    /// Attach a metadata filter.
    pub fn with_filter(mut self, filter: Filter) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Override the fusion strategy.
    pub fn with_fusion(mut self, fusion: FusionMode) -> Self {
        self.fusion = fusion;
        self
    }
}

/// A raw hit from a single retrieval leg (vector or keyword).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorHit {
    /// The stored vector/document ID.
    pub id: String,
    /// Leg-specific score (cosine similarity, BM25, etc. — not normalized).
    pub score: f32,
    /// Associated metadata.
    pub metadata: Metadata,
}

/// A fused retrieval hit — content + score + optional embedding, ready for
/// context injection or reranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalHit {
    /// The stored document/chunk ID.
    pub id: String,
    /// The retrieved text content.
    pub content: String,
    /// Fused relevance score (higher = more relevant).
    pub score: f32,
    /// The chunk's embedding, if the backend retains it (for downstream
    /// reranking or visualization). `None` for keyword-only backends.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    /// Associated metadata.
    pub metadata: Metadata,
}

/// A document to be reranked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankDoc {
    /// The document/chunk ID.
    pub id: String,
    /// The document text.
    pub content: String,
}

impl RerankDoc {
    /// Create a rerank candidate.
    pub fn new(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
        }
    }
}

/// A reranked document with its new score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedDoc {
    /// The document/chunk ID.
    pub id: String,
    /// The document text.
    pub content: String,
    /// Cross-encoder relevance score (higher = more relevant).
    pub score: f32,
}

/// Low-level dense vector backend (ANN/KNN) with metadata filtering.
///
/// Implementations: `oneai_vector::InMemoryVectorBackend` (brute cosine),
/// `SqliteVecBackend` (exact KNN, mobile/small), `UsearchBackend` (HNSW),
/// and app-supplied backends (Qdrant, LanceDB, pgvector, …).
///
/// `dimension()` must be fixed for the lifetime of an instance — backends
/// use it to size storage at construction.
#[async_trait]
pub trait VectorBackend: Send + Sync {
    /// Upsert a vector with metadata. If `id` exists it is replaced.
    async fn upsert(&self, id: &str, embedding: &[f32], metadata: Metadata) -> Result<()>;

    /// Search for vectors similar to `query`, returning at most `top_k` hits
    /// that satisfy `filter` (pre-filter where supported).
    async fn search(
        &self,
        query: &[f32],
        top_k: usize,
        filter: Option<&Filter>,
    ) -> Result<Vec<VectorHit>>;

    /// Delete a vector by ID. No-op if absent.
    async fn delete(&self, id: &str) -> Result<()>;

    /// The fixed dimension of vectors this backend accepts.
    fn dimension(&self) -> usize;
}

/// Low-level lexical retrieval backend (BM25 / learned-sparse).
///
/// Implementations: `oneai_vector::TantivyBm25Backend` (Tantivy + jieba CJK),
/// and app-supplied backends (Qdrant sparse, Elasticsearch, …).
#[async_trait]
pub trait KeywordBackend: Send + Sync {
    /// Upsert a document's text + metadata for lexical indexing.
    async fn upsert_doc(&self, id: &str, text: &str, metadata: Metadata) -> Result<()>;

    /// Lexical search for `query`, returning at most `top_k` hits that satisfy
    /// `filter`.
    async fn search(
        &self,
        query: &str,
        top_k: usize,
        filter: Option<&Filter>,
    ) -> Result<Vec<VectorHit>>;

    /// Delete a document by ID. No-op if absent.
    async fn delete(&self, id: &str) -> Result<()>;
}

/// Composite hybrid retrieval backend.
///
/// This is the trait an app-supplied backend (e.g. Qdrant, which does dense +
/// BM25 + RRF natively) implements directly — in that case `search_hybrid`
/// fuses internally and the framework's `StandardRetrievalPipeline` is not
/// used. For embedded deployments, `oneai_vector::StandardRetrievalPipeline`
/// composes a `VectorBackend` + `KeywordBackend` + optional `RerankerProvider`
/// via RRF and implements this trait.
///
/// `upsert_chunk` does NOT take a `Chunk` type — it takes raw `content` +
/// `metadata` + optional `embedding` so the trait stays free of `oneai-rag`
/// types and both RAG documents and memory entries can share one backend.
#[async_trait]
pub trait RetrievalBackend: Send + Sync {
    /// Run a hybrid/keyword/vector retrieval per [`RetrievalRequest`] and
    /// return fused, ranked hits.
    async fn search_hybrid(&self, req: &RetrievalRequest) -> Result<Vec<RetrievalHit>>;

    /// Upsert a chunk: index its text for the lexical leg and, when an
    /// embedding is supplied, store it for the dense leg.
    async fn upsert_chunk(
        &self,
        id: &str,
        content: &str,
        metadata: Metadata,
        embedding: Option<&[f32]>,
    ) -> Result<()>;

    /// Delete a chunk (both legs) by ID. No-op if absent.
    async fn delete(&self, id: &str) -> Result<()>;
}

/// Second-stage cross-encoder reranker.
///
/// Implementations: `oneai_vector::BgeRerankerOnnx` (local ONNX, Apache-2.0
/// `bge-reranker-v2-m3`), and app-supplied cloud rerankers (Cohere rerank-v4,
/// Voyage rerank-2.5). Reranking is the last pipeline step and, per Anthropic's
/// Contextual Retrieval evaluation, cuts retrieval failure rate by 67% on top
/// of hybrid + BM25.
#[async_trait]
pub trait RerankerProvider: Send + Sync {
    /// Rerank `docs` against `query`, returning the top `top_n` by
    /// cross-encoder score.
    async fn rerank(&self, query: &str, docs: &[RerankDoc], top_n: usize)
        -> Result<Vec<RankedDoc>>;

    /// The reranker model name (for logging/identification).
    fn model(&self) -> &str;
}

// ─── MemoryPersistence ─────────────────────────────────────────────────────

/// Separator marking a conversation row as an internal **discarded-prefix
/// archive snapshot** written by context compression (see
/// `MemoryManager::archive_discarded_snapshot`). The id is formatted as
/// `"{session_id}{DISCARDED_SNAPSHOT_MARKER}{uuid}"`.
///
/// These rows are NOT user-facing conversations — they hold the raw transcript
/// that was summarized away, kept only for audit / on-demand `memory_search`
/// fallback. Persistence backends MUST exclude them from `list_conversations`
/// (they must never surface as selectable sessions in a foreign UI) and MUST
/// cascade-delete them when the parent `session_id` is deleted.
/// `load_conversation` still resolves them by exact id (for the audit/search
/// path); only the listing and delete-cascade treat them specially.
pub const DISCARDED_SNAPSHOT_MARKER: &str = "::discarded::";

/// Trait for persisting and restoring memory and conversation state.
///
/// Enables SQLite (or other) backends to store STM entries, LTM entries,
/// and conversation history, allowing session resume and knowledge accumulation
/// across application restarts.
///
/// This addresses the critical gap where all memory is purely in-memory
/// (HashMap, VecDeque) and lost on restart. With a MemoryPersistence backend,
/// the agent framework becomes truly usable for production scenarios.
#[async_trait]
pub trait MemoryPersistence: Send + Sync {
    /// Save STM entries for a session (bulk operation).
    async fn save_stm(&self, session_id: &str, entries: &[MemoryEntry]) -> Result<()>;

    /// Load STM entries for a session (ordered by position in the sliding window).
    async fn load_stm(&self, session_id: &str) -> Result<Vec<MemoryEntry>>;

    /// Clear STM entries for a session.
    async fn clear_stm(&self, session_id: &str) -> Result<()>;

    /// Save a single LTM entry.
    async fn save_ltm(&self, entry: &MemoryEntry) -> Result<()>;

    /// Load a LTM entry by ID.
    async fn load_ltm(&self, id: &str) -> Result<Option<MemoryEntry>>;

    /// Search LTM by keyword (case-insensitive substring match).
    async fn search_ltm_keyword(&self, keyword: &str, top_k: usize) -> Result<Vec<MemoryEntry>>;

    /// Search LTM by embedding (cosine similarity against stored embeddings).
    ///
    /// Loads entries with embeddings from storage, computes brute-force cosine
    /// similarity in Rust (acceptable for <10K entries), and returns the top_k
    /// most similar entries with their scores.
    async fn search_ltm_embedding(
        &self,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<(MemoryEntry, f32)>>;

    /// Delete a LTM entry by ID.
    async fn delete_ltm(&self, id: &str) -> Result<()>;

    /// Clear all LTM entries.
    async fn clear_ltm(&self) -> Result<()>;

    /// Save a conversation (message history for multi-turn sessions).
    async fn save_conversation(&self, id: &str, conversation: &Conversation) -> Result<()>;

    /// Load a conversation by ID.
    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>>;

    /// Load this session's discarded-prefix archive snapshots (the archival
    /// tier), ordered oldest-first by creation time.
    ///
    /// These are the `{session_id}{DISCARDED_SNAPSHOT_MARKER}{uuid}` rows
    /// written by context compression (see
    /// `MemoryManager::archive_discarded_snapshot`). They hold the raw
    /// transcript that was summarized away, and are merged back with the
    /// live (compressed) conversation by
    /// `MemoryManager::full_transcript_messages` to reconstruct the full
    /// history for display — the model keeps seeing the compressed live
    /// context, while the UI shows the complete, queryable transcript.
    /// Default: empty (backends without an archive return nothing).
    async fn load_discarded_snapshots(&self, _session_id: &str) -> Result<Vec<Conversation>> {
        Ok(Vec::new())
    }

    /// Cheap per-snapshot DISPLAY message count (non-`system` messages) for
    /// this session's discarded-prefix archive, oldest-first. Returns only
    /// `(id, count)` — no message content is loaded. Used by
    /// `MemoryManager::transcript_page` to compute segment boundaries for
    /// pagination without deserializing any snapshot. Default: empty.
    async fn snapshot_display_counts(&self, _session_id: &str) -> Result<Vec<(String, u32)>> {
        Ok(Vec::new())
    }

    /// List all saved conversations (metadata only, not full message history).
    async fn list_conversations(&self) -> Result<Vec<SessionInfo>>;

    /// Delete a conversation and its associated STM entries by ID.
    async fn delete_conversation(&self, id: &str) -> Result<()>;

    // ─── MemoryFact persistence (core/archival tiers) ──────────────────────
    //
    // These back the DomainPack MemoryProfile layer's durable facts. Default
    // impls are no-ops so existing backends keep compiling; the SQLite backend
    // overrides them to persist facts across restarts ("越用越好用").

    /// Upsert a fact (conflict-resolved by user_id+subject+predicate).
    async fn store_fact(&self, _fact: &MemoryFact) -> Result<()> {
        Ok(()) // no-op default
    }

    /// Load all facts for a user (cross-session habits) and/or session.
    async fn load_facts(&self, _user_id: &str, _session_id: &str) -> Result<Vec<MemoryFact>> {
        Ok(Vec::new()) // no-op default
    }
}

// ─── DiscardedSink ──────────────────────────────────────────────────────────

/// Sink for messages discarded during context compression.
///
/// The "压缩即不丢" closure: when `ContextBudgetManager::compress` summarizes
/// away older turns, the discarded `Message`s are handed to this sink before
/// being dropped from the live conversation. A typical implementation persists
/// them as a turn-scoped conversation snapshot (via `MemoryPersistence::
/// save_conversation`) so they remain available for resume, audit, and on-demand
/// `memory_search` fallback — raw transcript is not lost even though it leaves
/// the working context.
///
/// Compression-coupled fact extraction (turning discarded turns into durable
/// `MemoryFact`s) runs *inside* the compressor; this sink is the complementary
/// raw-transcript archive. Failures must not propagate — a bad sink must not
/// break the compression path.
#[async_trait]
pub trait DiscardedSink: Send + Sync {
    /// Archive a batch of discarded messages, scoped to `session_id`.
    async fn archive_discarded(&self, session_id: &str, discarded: Vec<Message>) -> Result<()>;
}

// ─── SessionInfo ────────────────────────────────────────────────────────────

/// Metadata about a saved conversation session.
///
/// Used by `MemoryPersistence::list_conversations()` to return summary
/// information without loading the full message history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionInfo {
    /// The session/conversation ID.
    pub id: String,

    /// When the session was first created.
    pub created_at: chrono::DateTime<chrono::Utc>,

    /// When the session was last updated (last message timestamp).
    pub updated_at: chrono::DateTime<chrono::Utc>,

    /// Number of messages in the conversation.
    pub message_count: usize,

    /// A short title derived from the first user message (truncated, whitespace
    /// collapsed). `None` when the conversation has no user message yet. Used by
    /// foreign UIs (e.g. the Android drawer) to label session rows without
    /// loading full histories.
    #[serde(default)]
    pub title: Option<String>,

    /// The workspace (working-directory path) the user bound this session to at
    /// creation — a frontend "select workspace" affordance (deepseek-harness
    /// parity). `None` for sessions created without one (the legacy default:
    /// the agent's app-global cwd). Persisted in `conversation.metadata
    /// ["workspace"]`; surfaced here so `session/list` groups sessions by
    /// workspace without re-reading every conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,

    /// Whether the user archived this session (sidebar UX — archived sessions
    /// fold into a collapsed "已归档" group and stay out of the active list).
    /// Persisted as `conversation.metadata["archived"] = "1"` (absent ⇔ false);
    /// surfaced in `session/list` so every frontend renders archive state
    /// consistently (the prior web-only localStorage flag did not sync to
    /// native). Defaults false for legacy/unknown rows.
    #[serde(default)]
    pub archived: bool,
}

impl SessionInfo {
    /// Create a new SessionInfo with the given fields (title = None).
    pub fn new(
        id: String,
        created_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
        message_count: usize,
    ) -> Self {
        Self {
            id,
            created_at,
            updated_at,
            message_count,
            title: None,
            workspace: None,
            archived: false,
        }
    }

    /// Create a new SessionInfo with an explicit title (first-user-message
    /// preview). The caller is responsible for truncating/collapsing the title.
    pub fn with_title(
        id: String,
        created_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
        message_count: usize,
        title: Option<String>,
    ) -> Self {
        Self {
            id,
            created_at,
            updated_at,
            message_count,
            title,
            workspace: None,
            archived: false,
        }
    }

    /// Builder-style workspace setter — `list_conversations` derives this from
    /// `conversation.metadata["workspace"]`; callers that already hold the
    /// conversation set it directly.
    pub fn with_workspace(mut self, workspace: Option<String>) -> Self {
        self.workspace = workspace;
        self
    }

    /// Builder-style archived setter — `list_conversations` derives this from
    /// `conversation.metadata["archived"]`.
    pub fn with_archived(mut self, archived: bool) -> Self {
        self.archived = archived;
        self
    }
}

// ─── Re-export serde_json for trait definitions ──────────────────────────────

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── FeedbackEntry ──────────────────────────────────────────────────────────

/// One stored per-message feedback entry — a user's 👍/👎/note reaction to a
/// specific assistant message, identified by `(session_id, turn_id)`. Lives in
/// core (like [`SessionInfo`]) so both `oneai-persistence` (the durable store)
/// and `oneai-app-server` (the `FeedbackStore` trait + wire shape) share one
/// type without a dep-direction violation.
///
/// `kind` is a free-form wire string (`"up"` / `"down"` / `"note"` — constants
/// live in `oneai-app-server::feedback`); `created_at_ms` is epoch-millis so a
/// frontend orders entries without a separate round-trip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FeedbackEntry {
    /// Stable id (store-assigned on record).
    pub id: String,
    /// The conversation the feedback belongs to.
    pub session_id: String,
    /// The turn whose assistant message this feedback targets.
    pub turn_id: String,
    /// The role of the message being reacted to (`"assistant"` today).
    pub message_role: String,
    /// `"up"` / `"down"` / `"note"`.
    pub kind: String,
    /// Free-text commentary (present for `note`; `None` otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Epoch-millis creation timestamp.
    pub created_at_ms: u64,
}

// ─── HostAllowEntry ─────────────────────────────────────────────────────────

/// One row of the durable host allow/deny list — a host the user admitted (or
/// blocked) for sandboxed egress, persisted across sessions. Lives in core
/// (like [`FeedbackEntry`]) so both `oneai-persistence` (the durable
/// [`HostAllowlistStore`] impl, which builds these from its `host_allowlist` /
/// `host_denylist` tables) and `oneai-app-server` (the `HostAllowlistRpc` trait
/// + wire shape) share one type without a dep-direction violation.
///
/// `recorded_at_ms` is epoch-millis (the durable table stores unix-seconds;
/// the persistence impl ×1000s on read) so a frontend orders rows without a
/// second round-trip. A plain data DTO — no `#[non_exhaustive]`, matching
/// [`FeedbackEntry`], since it is constructed by value across crates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HostAllowEntry {
    /// Bare lowercased hostname (no port).
    pub host: String,
    /// Epoch-millis when the host was admitted/denied.
    pub recorded_at_ms: u64,
}

// ─── EmbeddingService ──────────────────────────────────────────────────────

/// Embedding service — generates vector embeddings from text.
///
/// The primary interface for embedding generation. Implementations
/// use different backends (local ONNX, Ollama API, OpenAI API, Anthropic API).
///
/// When integrated into DocumentIndex, the service is called automatically
/// during document insertion — each chunk's embedding is computed
/// and stored in the vector store without manual intervention.
///
/// When integrated into MemoryManager, the service is called automatically
/// during `add()` and `inject_ltm_context()` — embeddings are computed
/// for each memory entry, enabling true semantic search in LTM.
///
/// Concrete implementations live in `oneai-rag`:
/// - `OpenAIEmbeddingService` — OpenAI text-embedding API (cloud, high quality)
/// - `VoyageEmbeddingService` — Voyage embedding API (`api.voyageai.com`, `VOYAGE_API_KEY`)
/// - `OllamaEmbeddingService` — Ollama local embedding API (local, no API key needed)
/// - `FastEmbedService` — local ONNX model via fastembed crate
#[async_trait]
pub trait EmbeddingService: Send + Sync {
    /// Generate an embedding for a single text string.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Generate embeddings for multiple text strings in a batch.
    ///
    /// Batch embedding is more efficient than individual calls
    /// because it amortizes the model inference overhead.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Get the embedding model being used.
    fn model(&self) -> EmbeddingModel;

    /// The provider's effective max input size, in UTF-8 bytes used as a
    /// conservative proxy for tokens (token count ≤ UTF-8 byte length).
    ///
    /// `None` means "no enforced limit"; the chunk splitter skips splitting
    /// in that case. Providers with a known per-model cap override this.
    fn max_input_tokens(&self) -> Option<usize> {
        None
    }

    /// Get the embedding dimension.
    fn dimension(&self) -> usize {
        let dim = self.model().dimension();
        if dim == 0 {
            0 // Runtime-determined models (Ollama) — use actual_dimension()
        } else {
            dim
        }
    }

    /// Get the actual embedding dimension by generating a test embedding.
    ///
    /// This is needed for models like Ollama where the dimension isn't
    /// known until runtime. For models with a fixed dimension, this
    /// returns the known value without making an API call.
    async fn actual_dimension(&self) -> Result<usize> {
        let dim = self.model().dimension();
        if dim > 0 {
            Ok(dim)
        } else {
            let test = self.embed("test").await?;
            Ok(test.len())
        }
    }

    /// Health check — verify the embedding service is reachable and functional.
    ///
    /// Generates a tiny test embedding to verify connectivity and correctness.
    /// Returns Ok(()) if the service is healthy, Err with details otherwise.
    async fn health_check(&self) -> Result<()> {
        let embedding = self.embed("health check").await?;
        if embedding.is_empty() {
            return Err(crate::error::OneAIError::Embedding(
                "Embedding service returned empty vector".to_string(),
            ));
        }
        for val in &embedding {
            if !val.is_finite() {
                return Err(crate::error::OneAIError::Embedding(
                    "Embedding service returned non-finite values".to_string(),
                ));
            }
        }
        Ok(())
    }
}

// ─── EmbeddingModel ─────────────────────────────────────────────────────────

/// An embedding model, identified by its canonical model-name string.
///
/// Newtype over `String`: adding a new model never requires changing an enum
/// — just pass its name string, e.g. `EmbeddingModel::new("voyage-3")`. Known
/// dimensions are looked up from [`KNOWN_EMBEDDING_DIMENSIONS`]; names absent
/// from the table return `0` and are resolved at runtime via
/// [`EmbeddingService::actual_dimension`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmbeddingModel(pub String);

impl EmbeddingModel {
    /// Create a model identifier from any string-like input.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The canonical model-name string sent to provider APIs.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Look up the known vector dimension for this model.
    ///
    /// Returns `0` for models whose dimension is determined at runtime (e.g.
    /// Ollama models, or names absent from [`KNOWN_EMBEDDING_DIMENSIONS`]);
    /// callers should then use [`EmbeddingService::actual_dimension`].
    pub fn dimension(&self) -> usize {
        KNOWN_EMBEDDING_DIMENSIONS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&self.0))
            .map(|(_, d)| *d)
            .unwrap_or(0)
    }

    // ── Built-in convenience constructors ──────────────────────────────────

    /// `text-embedding-3-small` (OpenAI, 1536-dim).
    pub fn openai_small() -> Self {
        Self::new("text-embedding-3-small")
    }
    /// `text-embedding-3-large` (OpenAI, 3072-dim).
    pub fn openai_large() -> Self {
        Self::new("text-embedding-3-large")
    }
    /// `voyage-3` (Voyage, 1024-dim).
    pub fn voyage3() -> Self {
        Self::new("voyage-3")
    }
    /// `voyage-3-lite` (Voyage, 512-dim).
    pub fn voyage3_lite() -> Self {
        Self::new("voyage-3-lite")
    }
    /// `nomic-embed-text` (Ollama, runtime-dim).
    pub fn nomic_embed_text() -> Self {
        Self::new("nomic-embed-text")
    }
    /// `all-MiniLM-L6-v2` (FastEmbed, 384-dim).
    pub fn allminilm_l6_v2() -> Self {
        Self::new("all-MiniLM-L6-v2")
    }
    /// `bge-base-en-v1.5` (FastEmbed, 768-dim).
    pub fn bge_base_en_v15() -> Self {
        Self::new("bge-base-en-v1.5")
    }
    /// `mixedbread-embed-large-v1` (FastEmbed, 1024-dim).
    pub fn mxbai_embed_large_v1() -> Self {
        Self::new("mixedbread-embed-large-v1")
    }
}

impl From<String> for EmbeddingModel {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl From<&str> for EmbeddingModel {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl std::fmt::Display for EmbeddingModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Static table of known embedding-model dimensions.
///
/// Case-insensitive lookup; names not present resolve to `0` (runtime-probed
/// via [`EmbeddingService::actual_dimension`]).
pub static KNOWN_EMBEDDING_DIMENSIONS: &[(&str, usize)] = &[
    ("all-MiniLM-L6-v2", 384),
    ("bge-base-en-v1.5", 768),
    ("mixedbread-embed-large-v1", 1024),
    ("text-embedding-3-small", 1536),
    ("text-embedding-3-large", 3072),
    ("voyage-3", 1024),
    ("voyage-3-lite", 512),
];

// ─── EmbeddingProvider ──────────────────────────────────────────────────────

/// The embedding provider to use (or [`Auto`][Self::Auto] for zero-config
/// auto-detection).
///
/// `Auto` walks the detection chain (embedding-specific keys, never reusing
/// the LLM provider's key — embedding and chat are separate capabilities) and
/// picks the first available provider; if none is available the resolved
/// service is `None` and memory recall falls back to keyword matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
#[derive(Default)]
pub enum EmbeddingProvider {
    /// Zero-config auto-detection (the default).
    #[default]
    Auto,
    /// OpenAI `text-embedding-3-*` (official API, `OPENAI_API_KEY`).
    OpenAi,
    /// Voyage `voyage-3*` (`api.voyageai.com`, `VOYAGE_API_KEY`).
    Voyage,
    /// Ollama local embedding API (no key; probes `localhost:11434`).
    Ollama,
    /// FastEmbed local ONNX (no key; available when implemented).
    FastEmbed,
    /// Local BGE-M3 ONNX embedder (no key; 1024-dim, CJK-strong; available
    /// under `oneai-vector`'s `ort` feature when model files are present).
    BgeM3,
    /// OpenAI-compatible relay/gateway (explicit `base_url` + key required).
    OpenAiCompat,
}

impl EmbeddingProvider {
    /// Parse a provider id from its serde form (case-insensitive, accepts
    /// `openai-compat` / `openai_compat` / `openai-compatible`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "openai" => Some(Self::OpenAi),
            "voyage" => Some(Self::Voyage),
            "ollama" => Some(Self::Ollama),
            "fastembed" => Some(Self::FastEmbed),
            "bge-m3" | "bge_m3" | "bgem3" => Some(Self::BgeM3),
            "openai-compat" | "openai_compat" | "openai-compatible" => Some(Self::OpenAiCompat),
            _ => None,
        }
    }
}

impl std::fmt::Display for EmbeddingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Auto => "auto",
            Self::OpenAi => "openai",
            Self::Voyage => "voyage",
            Self::Ollama => "ollama",
            Self::FastEmbed => "fastembed",
            Self::BgeM3 => "bge-m3",
            Self::OpenAiCompat => "openai-compat",
        };
        write!(f, "{}", s)
    }
}

// ─── InputType ──────────────────────────────────────────────────────────────

/// Whether an embedding is for a search query or an indexed document.
///
/// Some providers (OpenAI, Voyage) optimize retrieval quality when the input
/// type is declared; providers that ignore it simply do so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum InputType {
    Query,
    Document,
}

// ─── EmbeddingConfig ─────────────────────────────────────────────────────────

/// Configuration for the embedding service.
///
/// The default ([`EmbeddingConfig::default`]) is **zero-config**:
/// `provider = Auto`, so the embedding service is auto-resolved at build time
/// from environment-detectable signals (embedding-specific keys, a reachable
/// local Ollama, etc.). Most users never touch this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EmbeddingConfig {
    /// Which provider to use (`Auto` = detect).
    #[serde(default)]
    pub provider: EmbeddingProvider,
    /// Model-name override (None → the provider's default model).
    #[serde(default)]
    pub model: Option<EmbeddingModel>,
    /// Embedding-specific API key. Sourced, in priority order, from this
    /// field, `ONEAI_EMBEDDING_API_KEY`, or the provider's own env var
    /// (`OPENAI_API_KEY` / `VOYAGE_API_KEY`). **Never** reused from the LLM
    /// provider's key — embedding and chat are separate capabilities.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Base URL override (required for `OpenAiCompat` relays).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Fallback provider used if the primary fails to create or first-call
    /// (build-time + runtime, sharing one `should_continue` classifier).
    #[serde(default)]
    pub fallback: Option<EmbeddingProvider>,
    /// Query/Document input-type hint (OpenAI/Voyage retrieval optimization).
    #[serde(default)]
    pub input_type: Option<InputType>,
    /// Provider output-dimensionality override (OpenAI `dimensions`).
    #[serde(default)]
    pub output_dimensionality: Option<usize>,
    /// Hard cap on input size before byte-bisection chunking kicks in
    /// (None → provider default table).
    #[serde(default)]
    pub max_input_tokens: Option<usize>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::Auto,
            model: None,
            api_key: None,
            base_url: None,
            fallback: None,
            input_type: None,
            output_dimensionality: None,
            max_input_tokens: None,
        }
    }
}

impl EmbeddingConfig {
    /// Zero-config (auto-detect) configuration — the recommended default.
    pub fn auto() -> Self {
        Self::default()
    }

    /// OpenAI embedding (official API, `OPENAI_API_KEY`).
    pub fn openai(api_key: String) -> Self {
        Self {
            provider: EmbeddingProvider::OpenAi,
            api_key: Some(api_key),
            ..Self::default()
        }
    }

    /// Voyage embedding (`api.voyageai.com`, `VOYAGE_API_KEY`).
    pub fn voyage(api_key: String) -> Self {
        Self {
            provider: EmbeddingProvider::Voyage,
            api_key: Some(api_key),
            ..Self::default()
        }
    }

    /// Ollama local embedding (no key; probes `localhost:11434`).
    pub fn ollama() -> Self {
        Self {
            provider: EmbeddingProvider::Ollama,
            ..Self::default()
        }
    }

    /// OpenAI-compatible relay (explicit `base_url` + key required).
    pub fn openai_compat(api_key: String, base_url: String) -> Self {
        Self {
            provider: EmbeddingProvider::OpenAiCompat,
            api_key: Some(api_key),
            base_url: Some(base_url),
            ..Self::default()
        }
    }

    /// FastEmbed local ONNX (no key; available once implemented).
    pub fn fastembed() -> Self {
        Self {
            provider: EmbeddingProvider::FastEmbed,
            ..Self::default()
        }
    }

    /// Builder-style: set the model.
    pub fn with_model(mut self, model: impl Into<EmbeddingModel>) -> Self {
        self.model = Some(model.into());
        self
    }
    /// Builder-style: set the base_url.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }
    /// Builder-style: set the fallback provider.
    pub fn with_fallback(mut self, fallback: EmbeddingProvider) -> Self {
        self.fallback = Some(fallback);
        self
    }
}

// ─── EmbeddingHealthStatus ──────────────────────────────────────────────────

/// Health status report for the embedding service registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EmbeddingHealthStatus {
    /// Primary service model name.
    pub primary_service: String,
    /// Whether the primary service is healthy.
    pub primary_healthy: bool,
    /// Fallback service model name (if configured).
    pub fallback_service: Option<String>,
    /// Whether the fallback service is healthy (if configured).
    pub fallback_healthy: Option<bool>,
    /// Whether caching is enabled.
    pub cache_enabled: bool,
    /// Number of cached embeddings.
    pub cache_size: usize,
}

impl EmbeddingHealthStatus {
    /// Whether the overall embedding system is functional.
    ///
    /// Returns true if either the primary or fallback service is healthy.
    pub fn is_functional(&self) -> bool {
        self.primary_healthy || self.fallback_healthy.unwrap_or(false)
    }

    /// Create a new EmbeddingHealthStatus.
    pub fn new(
        primary_service: String,
        primary_healthy: bool,
        fallback_service: Option<String>,
        fallback_healthy: Option<bool>,
        cache_enabled: bool,
        cache_size: usize,
    ) -> Self {
        Self {
            primary_service,
            primary_healthy,
            fallback_service,
            fallback_healthy,
            cache_enabled,
            cache_size,
        }
    }
}
