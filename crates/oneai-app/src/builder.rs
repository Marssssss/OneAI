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

use oneai_core::error::Result;
use oneai_core::traits::{InteractionGate, LlmProvider, OutputParser, Tool, EmbeddingService, MemoryPersistence};
use oneai_core::{Conversation, SessionInfo};
use oneai_core::EmbeddingConfig;
use oneai_core::usage::{UsageTracker, InMemoryUsageTracker};
use oneai_core::rate_limiter::{RateLimiter, TokenWindowRateLimiter, RateLimitConfig};
use oneai_core::circuit_breaker::{CircuitBreaker, ThresholdCircuitBreaker, CircuitBreakerConfig};
use oneai_core::platform::{Platform, PlatformAdapter};
use oneai_core::{ModelConfig, CloudProviderKind};
use oneai_core::ProviderPoolConfig;
use oneai_core::SmartRouteConfig;
use oneai_core::TokenCounter;
use oneai_core::ContextManager;
use oneai_core::ContextManagerConfig;

use oneai_provider::{ProviderPool, SmartRouter};

use oneai_tool::{
    ToolExecutor, ToolRegistry, InteractionGateConfig, NoopInteractionGate,
    ChannelInteractionGate, ThresholdInteractionGate,
};
use oneai_memory::{MemoryManager, MemoryManagerConfig};
use oneai_rag::DocumentIndex;
use oneai_rag::EmbeddingConfigExt;
use oneai_skill::SkillSelector;
use oneai_parser::ThreeLayerParser;
use oneai_workflow::WorkflowExecutor;
use oneai_persistence::FilePersistence;
use oneai_trace::{TraceContext, TraceEmitter, InMemoryCollector};

use oneai_domain::{DomainPack, MergedDomainPack};

use oneai_a2a::A2AClient;

use oneai_wasm::{WasmRuntime, WasmRuntimeConfig, WasmModuleManager, WasmActionTool, WasmModuleRegistry, WasmResourceMonitor};

use oneai_mcp::{McpPluginRegistry, McpServerHost};

use oneai_a2a::{A2AServerHost, TaskStore, AgentCard};

use oneai_persistence::SqliteSessionStore;

use crate::session::AppSession;

/// Builder for assembling a OneAI application.
pub struct AppBuilder {
    /// LLM provider (optional — needed for agent inference).
    provider: Option<Arc<dyn LlmProvider>>,
    /// Tool registry.
    tool_registry: Arc<ToolRegistry>,
    /// Unified interaction gate — every loop-suspend decision point.
    /// When `None` at `build()` time, defaults to `NoopInteractionGate` (zero latency).
    interaction_gate: Option<Arc<dyn InteractionGate>>,
    /// Output parser.
    parser: Option<Arc<dyn OutputParser>>,
    /// Memory manager.
    memory_manager: Option<Arc<MemoryManager>>,
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
    /// Policy for Layer-1 constrained decoding (tier-gated). Propagated into
    /// the `AgentLoopConfig`. Only takes effect when `structured_output` is
    /// also configured on the loop. Default `Auto`.
    constrained_output_policy: oneai_core::ConstrainedOutputPolicy,
    /// Durable working-state store root (optional). When set, the app builds a
    /// `FileWorkingStateStore` rooted here so the agent persists goal/steps/
    /// decisions/blockers to per-task append-only event logs — enabling crash
    /// recovery and cross-session task continuation.
    working_state_root: Option<std::path::PathBuf>,
}

impl AppBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            provider: None,
            tool_registry: Arc::new(ToolRegistry::new()),
            interaction_gate: None,
            parser: None,
            memory_manager: None,
            rag_index: None,
            skill_selector: None,
            skill_registry: Arc::new(oneai_skill::SkillRegistry::new()),
            persistence: None,
            platform: None,
            trace_context: None,
            domain_packs: Vec::new(),
            user_id: None,
            a2a_client: None,
            wasm_runtime: None,
            wasm_module_registry: None,
            wasm_resource_monitor: None,
            mcp_plugin_registry: None,
            mcp_server_host_enabled: false,
            a2a_server_host_enabled: false,
            a2a_server_port: None,
            a2a_server_agent_card: None,
            sqlite_store: None,
            embedding_service: None,
            embedding_config: None,
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
            constrained_output_policy: oneai_core::ConstrainedOutputPolicy::Auto,
            working_state_root: None,
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

    // ─── InteractionGate (unified) ──────────────────────────────────────────

    /// Set the unified interaction gate directly.
    pub fn interaction_gate(mut self, gate: Arc<dyn InteractionGate>) -> Self {
        self.interaction_gate = Some(gate);
        self
    }

    /// Use the no-op interaction gate (every point disabled, zero latency).
    /// This is the default when no gate is configured.
    pub fn noop_interaction_gate(mut self) -> Self {
        self.interaction_gate = Some(Arc::new(NoopInteractionGate));
        self
    }

    /// Use a channel-based interaction gate with all points enabled.
    ///
    /// Returns the builder plus the receiver the UI thread drains for pending
    /// interaction requests.
    pub fn channel_interaction_gate(
        mut self,
        buffer_size: usize,
    ) -> (Self, tokio::sync::mpsc::Receiver<oneai_tool::InteractionPendingItem>) {
        let (gate, receiver) = ChannelInteractionGate::new(buffer_size);
        self.interaction_gate = Some(Arc::new(gate));
        (self, receiver)
    }

    /// Use a channel-based interaction gate with a per-point config.
    pub fn channel_interaction_gate_with_config(
        mut self,
        buffer_size: usize,
        config: InteractionGateConfig,
    ) -> (Self, tokio::sync::mpsc::Receiver<oneai_tool::InteractionPendingItem>) {
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
    ) -> (Self, tokio::sync::mpsc::Receiver<oneai_tool::InteractionPendingItem>) {
        let (gate, receiver) = ThresholdInteractionGate::new(
            buffer_size,
            threshold,
            InteractionGateConfig::default(),
        );
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
    ) -> (Self, tokio::sync::mpsc::Receiver<oneai_tool::InteractionPendingItem>) {
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
        let ctx = TraceEmitter::global().create_context_with_collector(
            Arc::new(InMemoryCollector::new())
        );
        self.trace_context = Some(ctx);
        self
    }

    /// Enable file-based tracing (writes JSON to the specified path).
    pub fn trace_to_file(mut self, path: &str) -> Self {
        let ctx = TraceEmitter::global().create_context_with_collector(
            Arc::new(oneai_trace::FileCollector::new(path))
        );
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

    /// Enable OTEL tracing — exports spans to an OTEL backend via OTLP protocol.
    ///
    /// Creates an `OtlpCollector` that converts OneAI spans to OTEL format
    /// and exports them to the specified endpoint (e.g., Jaeger, Grafana).
    ///
    /// Requires the `otel` feature on `oneai-trace`.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .trace_otel("http://localhost:4317")
    ///     .build()?;
    /// ```
    #[cfg(feature = "otel")]
    pub fn trace_otel(mut self, endpoint: &str) -> Self {
        let config = oneai_trace::OtlpConfig::grpc(endpoint, "oneai-agent");
        let collector = oneai_trace::OtlpCollector::new(config);
        let ctx = TraceEmitter::global().create_context_with_collector(
            Arc::new(collector)
        );
        self.trace_context = Some(ctx);
        self
    }

    /// Enable OTEL tracing with HTTP protocol.
    #[cfg(feature = "otel")]
    pub fn trace_otel_http(mut self, endpoint: &str) -> Self {
        let config = oneai_trace::OtlpConfig::http(endpoint, "oneai-agent");
        let collector = oneai_trace::OtlpCollector::new(config);
        let ctx = TraceEmitter::global().create_context_with_collector(
            Arc::new(collector)
        );
        self.trace_context = Some(ctx);
        self
    }

    /// Enable OTEL tracing with custom configuration.
    #[cfg(feature = "otel")]
    pub fn trace_otel_config(mut self, config: oneai_trace::OtlpConfig) -> Self {
        let collector = oneai_trace::OtlpCollector::new(config);
        let ctx = TraceEmitter::global().create_context_with_collector(
            Arc::new(collector)
        );
        self.trace_context = Some(ctx);
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
            let config = MemoryManagerConfig::default();
            self.memory_manager = Some(Arc::new(
                MemoryManager::with_compressor_and_reflection(
                    config,
                    provider.clone(),
                )
            ));
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
    pub fn domain_pack_from_source(mut self, source: &oneai_domain::PackSource, project_dir: &str) -> Self {
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
        let runtime = WasmRuntime::with_defaults()
            .expect("WASM runtime creation should succeed");
        let app = self.wasm_runtime(Arc::new(runtime));

        // Register WASM action tools
        app.register_wasm_action_tools()
    }

    /// Use a WASM runtime with custom configuration.
    pub fn wasm_runtime_with_config(mut self, config: WasmRuntimeConfig) -> Self {
        let runtime = WasmRuntime::new(config)
            .expect("WASM runtime creation should succeed");
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
    /// Probes, in order: an explicit embedding relay (`ONEAI_EMBEDDING_API_KEY`
    /// + `ONEAI_EMBEDDING_BASE_URL`), Voyage (`VOYAGE_API_KEY`), OpenAI
    /// (`OPENAI_API_KEY`), a reachable local Ollama, then FastEmbed when
    /// implemented. If nothing is available, resolves to `None` and memory
    /// recall falls back to keyword matching — never hard-fails on a missing key.
    pub fn default_embedding_service(self) -> Self {
        self.embedding_config(EmbeddingConfig::auto())
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
        self.provider_pool_config(ProviderPoolConfig::anthropic_primary(anthropic_key, openai_key))
    }

    /// Use the default OpenAI-primary provider pool.
    ///
    /// Creates a fallback chain: OpenAI gpt-4o → Anthropic Sonnet → Ollama qwen2.5.
    pub fn default_provider_pool_openai(self) -> Self {
        let openai_key = std::env::var("OPENAI_API_KEY").ok();
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
        self.provider_pool_config(ProviderPoolConfig::openai_primary(openai_key, anthropic_key))
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
        self.token_counter(Arc::new(oneai_core::HeuristicTokenCounter::new()))
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
    pub fn model_context_resolver(mut self, resolver: Arc<oneai_core::ModelContextResolver>) -> Self {
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
            let config = MemoryManagerConfig::default();
            self.memory_manager = Some(Arc::new(
                MemoryManager::with_persistence(config, store),
            ));
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
            let config = MemoryManagerConfig::default();
            self.memory_manager = Some(Arc::new(
                MemoryManager::with_persistence(config, store),
            ));
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
    pub async fn build(self) -> Result<App> {
        // The unified interaction gate defaults to Noop (every point disabled,
        // zero latency) — production runs without a UI are not blocked. A TUI or
        // platform app wires a Channel/Threshold gate via the interaction_gate* builders.
        let interaction_gate = self.interaction_gate.unwrap_or_else(|| {
            Arc::new(NoopInteractionGate)
        });

        let parser = self.parser.unwrap_or_else(|| {
            Arc::new(ThreeLayerParser::new())
        });

        // Merge domain packs (if any)
        let merged_domain_pack = if self.domain_packs.is_empty() {
            None
        } else {
            Some(Arc::new(MergedDomainPack::merge(self.domain_packs)))
        };

        // Create WASM module manager if runtime is provided
        let wasm_module_manager = self.wasm_runtime.as_ref().map(|rt| {
            WasmModuleManager::new(rt.clone())
        });

        // Auto-create WASM module registry if runtime is set but no registry
        let wasm_module_registry = self.wasm_module_registry.or_else(|| {
            self.wasm_runtime.as_ref().map(|rt| {
                WasmModuleRegistry::new(rt.clone())
            })
        });

        // Auto-create WASM resource monitor if runtime is set but no monitor
        let wasm_resource_monitor = self.wasm_resource_monitor.or_else(|| {
            if self.wasm_runtime.is_some() {
                Some(Arc::new(WasmResourceMonitor::new()))
            } else {
                None
            }
        });

        let tool_executor = Arc::new(ToolExecutor::with_interaction_gate(
            self.tool_registry.clone(),
            interaction_gate.clone(),
        ));

        // Build workflow executor with the tool registry. When a direct LLM
        // provider is set, attach it so prompt-based DAG steps run real
        // inference (otherwise prompt steps only emit interpolated text).
        // The provider_pool-config auto-build path resolves later (below), so
        // pool-only configs still get a provider at the App level — but DAG
        // prompt-steps there fall back to no-inference until a later pass.
        let workflow_executor = if let Some(provider) = &self.provider {
            Arc::new(WorkflowExecutor::with_provider(
                Arc::new(std::collections::HashMap::new()),
                interaction_gate.clone(),
                provider.clone(),
            ))
        } else {
            Arc::new(WorkflowExecutor::new(
                Arc::new(std::collections::HashMap::new()),
                interaction_gate.clone(),
            ))
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

        // Connect MCP plugin servers and register discovered tools
        let mcp_plugin_registry = self.mcp_plugin_registry;
        if let Some(_registry) = &mcp_plugin_registry {
            // Note: connect_all_enabled() is async and mutable, so we need to handle it carefully
            // We'll register tools in the build flow after creating the mutable registry
            tracing::info!("MCP plugin registry configured — tools will be registered at build time");
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
                oneai_a2a::agent_card_from_domain_pack(&domain.as_ref().to_domain_pack(), "http://localhost:8080")
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
            self.embedding_config.as_ref().and_then(|config| {
                match config.build_service() {
                    Ok(Some(service)) => Some(service),
                    Ok(None) => {
                        tracing::info!("No embedding provider resolved; memory recall uses keyword matching");
                        None
                    }
                    Err(err) => {
                        tracing::warn!("Failed to resolve embedding service from config: {}", err);
                        None
                    }
                }
            })
        });

        // Wire embedding service into MemoryManager if configured
        let memory_manager = if embedding_service.is_some() && self.memory_manager.is_none() {
            // Create MemoryManager with embedding service
            let config = MemoryManagerConfig::default();
            Arc::new(MemoryManager::with_embedding(config, embedding_service.clone().unwrap()))
        } else {
            self.memory_manager.unwrap_or_else(|| {
                Arc::new(MemoryManager::new())
            })
        };

        // P5: namespace memory by user id (cross-session habits) and register
        // self-managed memory tools when the active domain opts in.
        if let Some(uid) = &self.user_id {
            memory_manager.set_user_id(uid.clone()).await;
        }
        if let Some(domain) = &merged_domain_pack {
            if domain.memory_profile.enable_memory_tools {
                let mm = memory_manager.clone();
                let recall_cfg = domain.memory_profile.recall.clone();
                self.tool_registry
                    .register(Arc::new(oneai_memory::MemorySearchTool::with_recall_config(mm.clone(), recall_cfg)) as Arc<dyn Tool>)
                    .await?;
                self.tool_registry
                    .register(Arc::new(oneai_memory::CoreMemoryEditTool::new(mm.clone())) as Arc<dyn Tool>)
                    .await?;
                self.tool_registry
                    .register(Arc::new(oneai_memory::ArchivalInsertTool::new(mm)) as Arc<dyn Tool>)
                    .await?;
            }
        }

        // Resolve usage tracker: use explicitly set tracker, or auto-create from persistence
        let usage_tracker = self.usage_tracker.or_else(|| {
            if let Some(store) = &self.sqlite_store {
                // Auto-create persistent tracker if persistence is available
                Some(Arc::new(oneai_persistence::SqliteUsageTracker::from_store(store))
                    as Arc<dyn UsageTracker>)
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

        // Resolve model context resolver: explicit, or auto-create when any
        // token/context component is configured, so the expanded built-in
        // library (L3) + L1 overrides take effect even without explicit setup.
        // Seeded with L1 user-profiles from context_manager_config.profiles.
        // L1 provider-extras (ModelConfig.extra["context_window"]) are added
        // after the provider is resolved below.
        let resolved_resolver: Option<Arc<oneai_core::ModelContextResolver>> =
            self.model_context_resolver.clone().or_else(|| {
                if self.context_manager_config.is_some()
                    || self.context_manager.is_some()
                    || self.token_counter.is_some()
                {
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
                } else {
                    None
                }
            });

        // Resolve token counter: use explicitly set counter, or create default
        let resolved_token_counter = self.token_counter.or_else(|| {
            if self.context_manager_config.is_some() || self.context_manager.is_some() {
                // Auto-create if context manager is configured, attaching the
                // resolver so context_window_size consults the 3-layer path.
                let mut counter = oneai_core::HeuristicTokenCounter::new();
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
                    Arc::new(oneai_core::HeuristicTokenCounter::new()) as Arc<dyn TokenCounter>
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
            provider_pool.clone().map(|pool| pool as Arc<dyn LlmProvider>)
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
        // available every session. Domain builtin skills are registered on top
        // by the caller (CLI/TUI) and add rather than replace these.
        self.skill_registry.load_discovered().await;

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

        Ok(App {
            provider,
            tool_registry: self.tool_registry,
            tool_executor,
            interaction_gate,
            parser,
            memory_manager,
            rag_index: self.rag_index,
            skill_selector: self.skill_selector.unwrap_or_else(|| {
                Arc::new(SkillSelector::new())
            }),
            skill_registry: self.skill_registry,
            active_skill: Arc::new(tokio::sync::RwLock::new(None)),
            persistence: self.persistence,
            workflow_executor,
            platform,
            trace_context: self.trace_context,
            domain_pack: merged_domain_pack,
            a2a_client: self.a2a_client,
            wasm_runtime: self.wasm_runtime,
            wasm_module_manager,
            wasm_module_registry,
            wasm_resource_monitor,
            mcp_plugin_registry,
            mcp_server_host,
            a2a_server_host,
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
            constrained_output_policy: self.constrained_output_policy,
            working_state_store,
        })
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
    /// MCP plugin registry (optional — for MCP server management).
    pub mcp_plugin_registry: Option<McpPluginRegistry>,
    /// MCP server host (optional — for serving tools via MCP protocol).
    pub mcp_server_host: Option<McpServerHost>,
    /// A2A server host (optional — for serving agent capabilities via A2A protocol).
    pub a2a_server_host: Option<A2AServerHost>,
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
    /// Layer-1 constrained-decoding policy — propagated into every `AgentLoopConfig`.
    /// See `AppBuilder::constrained_output_policy`.
    pub constrained_output_policy: oneai_core::ConstrainedOutputPolicy,
    /// Durable working-state store (optional) — the cross-session source of
    /// truth for goal/steps/decisions/blockers, persisted as per-task append-only
    /// event logs. When set, the agent loop persists plan progress incrementally
    /// (so it survives crashes) and a brand-new session can discover and
    /// continue an unfinished task from a previous session. See
    /// `AppBuilder::working_state`.
    pub working_state_store: Option<Arc<dyn oneai_core::traits::WorkingStateStore>>,
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

    /// Register a tool — adds it to both the tool executor and workflow executor.
    pub async fn register_tool(&self, tool: Arc<dyn Tool>) -> Result<()> {
        self.tool_registry.register(tool.clone()).await?;
        self.workflow_executor.register_tool(tool).await;
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
        self.mcp_plugin_registry.as_ref()
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
    use oneai_tool::CalculatorTool;
    use oneai_core::platform::PlatformAdapter;

    #[tokio::test]
    async fn test_app_builder_default_build() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .default_parser()
            .build()
            .await
            .expect("Build should succeed");

        assert!(!app.has_provider()); // No provider set
        assert!(app.tool_executor().list_tools().await.is_empty());
    }

    #[tokio::test]
    async fn test_app_register_and_use_tool() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .build()
            .await
            .expect("Build should succeed");

        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();

        let session = app.create_session();

        // Execute calculator via session
        let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+3"})).await.unwrap();
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

        app.register_tool(Arc::new(oneai_tool::ShellTool::new())).await.unwrap();

        let session = app.create_session();

        // Shell is high-risk — should be denied by the deny-all gate
        let result = session.execute_tool("shell", serde_json::json!({"command": "echo test"})).await.unwrap();
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
        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
        let session = app.create_session();

        let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+2"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "4");

        // Platform should be set by the adapter
        assert!(matches!(app.platform(), Platform::Macos | Platform::Linux | Platform::Windows));
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
            .mcp_server_host()  // ← enable MCP server hosting
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
            .mcp_plugin_registry(registry)  // ← set MCP plugin registry
            .build()
            .await
            .expect("Build should succeed");

        // MCP plugin registry should be set
        assert!(app.mcp_plugin_registry().is_some());

        // No MCP server host (not enabled)
        assert!(app.mcp_server_host().is_none());
    }

    #[tokio::test]
    async fn test_app_with_mcp_servers_from_config() {
        let app = AppBuilder::new()
            .noop_interaction_gate()
            .mcp_servers_from_config()  // ← load MCP servers from config file
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

        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();

        // Verify the MCP server host has the tool
        let host = app.mcp_server_host().unwrap();
        let response = host.process_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        })).await;

        let result = response.get("result").unwrap();
        let tools = result.get("tools").unwrap().as_array().unwrap();
        assert!(tools.iter().any(|t| t.get("name").and_then(|n| n.as_str()) == Some("calculator")));
    }
}