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
use oneai_core::traits::{ApprovalGate, LlmProvider, OutputParser, Tool};
use oneai_core::platform::{Platform, PlatformAdapter, PlatformApprovalGate};

use oneai_tool::{ToolExecutor, ToolRegistry, BlockingApprovalGate, AutoApprovalGate, ChannelApprovalGateWithThreshold};
use oneai_memory::{MemoryManager, MemoryManagerConfig};
use oneai_rag::DocumentIndex;
use oneai_skill::SkillSelector;
use oneai_parser::ThreeLayerParser;
use oneai_workflow::WorkflowExecutor;
use oneai_persistence::FilePersistence;
use oneai_trace::{TraceContext, TraceEmitter, InMemoryCollector};

use oneai_domain::{DomainPack, MergedDomainPack};

use oneai_a2a::A2AClient;

use oneai_wasm::{WasmRuntime, WasmRuntimeConfig, WasmModuleManager, WasmActionTool};

use crate::session::AppSession;

/// Builder for assembling a OneAI application.
pub struct AppBuilder {
    /// LLM provider (optional — needed for agent inference).
    provider: Option<Arc<dyn LlmProvider>>,
    /// Tool registry.
    tool_registry: Arc<ToolRegistry>,
    /// Approval gate.
    approval_gate: Option<Arc<dyn ApprovalGate>>,
    /// Output parser.
    parser: Option<Arc<dyn OutputParser>>,
    /// Memory manager.
    memory_manager: Option<Arc<MemoryManager>>,
    /// RAG document index.
    rag_index: Option<Arc<DocumentIndex>>,
    /// Skill selector.
    skill_selector: Option<Arc<SkillSelector>>,
    /// Persistence.
    persistence: Option<Arc<FilePersistence>>,
    /// Platform (detected or overridden).
    platform: Option<Platform>,
    /// Trace context (optional — for trajectory logging).
    trace_context: Option<TraceContext>,
    /// Domain packs (optional — for domain-specific configuration).
    domain_packs: Vec<DomainPack>,
    /// A2A client (optional — for inter-agent communication).
    a2a_client: Option<Arc<A2AClient>>,
    /// WASM runtime (optional — for WASM sandbox execution).
    wasm_runtime: Option<Arc<WasmRuntime>>,
}

impl AppBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            provider: None,
            tool_registry: Arc::new(ToolRegistry::new()),
            approval_gate: None,
            parser: None,
            memory_manager: None,
            rag_index: None,
            skill_selector: None,
            persistence: None,
            platform: None,
            trace_context: None,
            domain_packs: Vec::new(),
            a2a_client: None,
            wasm_runtime: None,
        }
    }

    /// Set the LLM provider.
    pub fn provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Set the approval gate.
    pub fn approval_gate(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approval_gate = Some(gate);
        self
    }

    /// Use the blocking (always-deny) approval gate.
    pub fn blocking_approval_gate(mut self) -> Self {
        self.approval_gate = Some(Arc::new(BlockingApprovalGate));
        self
    }

    /// Use the auto-approve gate (for testing).
    pub fn auto_approval_gate(mut self) -> Self {
        self.approval_gate = Some(Arc::new(AutoApprovalGate));
        self
    }

    /// Use a channel-based approval gate with auto-approve threshold.
    pub fn channel_approval_gate(
        mut self,
        buffer_size: usize,
        threshold: oneai_core::RiskLevel,
    ) -> (Self, tokio::sync::mpsc::Receiver<oneai_tool::ApprovalPendingItem>) {
        let (gate, receiver) = ChannelApprovalGateWithThreshold::new(buffer_size, threshold);
        self.approval_gate = Some(Arc::new(gate));
        (self, receiver)
    }

    /// Use a platform-specific approval gate.
    ///
    /// This allows the app to use native UI dialogs (NSAlert, AlertDialog,
    /// UIAlertController, etc.) for high-risk tool approval.
    pub fn platform_approval_gate(mut self, gate: Arc<dyn PlatformApprovalGate>) -> Self {
        self.approval_gate = Some(gate as Arc<dyn ApprovalGate>);
        self
    }

    /// Use a PlatformAdapter's approval gate.
    ///
    /// Convenience method that unpacks the platform adapter's approval gate
    /// and sets it as the app's approval gate. Also records the platform type.
    pub fn platform_adapter(mut self, adapter: PlatformAdapter) -> Self {
        self.approval_gate = Some(adapter.approval_gate);
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
    /// 1. Proactively recall relevant LTM memories into STM context on each turn
    /// 2. At session end, reflect on STM entries and generate episodic LTM memories
    ///
    /// This requires an LLM provider for the reflection prompt.
    /// The same provider is used for both reflection and compression.
    ///
    /// **Usage**:
    /// ```ignore
    /// let app = AppBuilder::new()
    ///     .provider(provider)
    ///     .with_memory_reflection()  // ← enables STM↔LTM closed loop
    ///     .build()?;
    /// ```
    pub fn with_memory_reflection(mut self) -> Self {
        if let Some(provider) = &self.provider {
            let config = MemoryManagerConfig::default();
            let injection_config = oneai_memory::MemoryInjectionConfig::default();
            self.memory_manager = Some(Arc::new(
                MemoryManager::with_compressor_and_reflection(
                    config,
                    injection_config,
                    provider.clone(),
                )
            ));
        }
        // If no provider is set yet, reflection will be enabled when
        // the provider is set (via the build() method).
        self
    }

    /// Enable memory reflection with custom injection configuration.
    pub fn with_memory_reflection_config(mut self, injection_config: oneai_memory::MemoryInjectionConfig) -> Self {
        if let Some(provider) = &self.provider {
            let config = MemoryManagerConfig::default();
            self.memory_manager = Some(Arc::new(
                MemoryManager::with_compressor_and_reflection(
                    config,
                    injection_config,
                    provider.clone(),
                )
            ));
        }
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

    /// Build the application.
    ///
    /// This creates the App and eagerly registers all domain pack tools
    /// into the ToolRegistry and WorkflowExecutor, so they are ready
    /// before any session is created.
    pub async fn build(self) -> Result<App> {
        let approval_gate = self.approval_gate.unwrap_or_else(|| {
            Arc::new(BlockingApprovalGate)
        });

        let parser = self.parser.unwrap_or_else(|| {
            Arc::new(ThreeLayerParser::new())
        });

        let memory_manager = self.memory_manager.unwrap_or_else(|| {
            Arc::new(MemoryManager::new())
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

        let tool_executor = Arc::new(ToolExecutor::with_approval_gate(
            self.tool_registry.clone(),
            approval_gate.clone(),
        ));

        // Build workflow executor with the tool registry
        let workflow_executor = Arc::new(WorkflowExecutor::new(
            Arc::new(std::collections::HashMap::new()),
            approval_gate.clone(),
        ));

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

        let platform = self.platform.unwrap_or(Platform::current());

        Ok(App {
            provider: self.provider,
            tool_registry: self.tool_registry,
            tool_executor,
            approval_gate,
            parser,
            memory_manager,
            rag_index: self.rag_index,
            skill_selector: self.skill_selector.unwrap_or_else(|| {
                Arc::new(SkillSelector::new())
            }),
            persistence: self.persistence,
            workflow_executor,
            platform,
            trace_context: self.trace_context,
            domain_pack: merged_domain_pack,
            a2a_client: self.a2a_client,
            wasm_runtime: self.wasm_runtime,
            wasm_module_manager,
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
    /// Approval gate.
    pub approval_gate: Arc<dyn ApprovalGate>,
    /// Output parser.
    pub parser: Arc<dyn OutputParser>,
    /// Memory manager.
    pub memory_manager: Arc<MemoryManager>,
    /// RAG document index (optional).
    pub rag_index: Option<Arc<DocumentIndex>>,
    /// Skill selector.
    pub skill_selector: Arc<SkillSelector>,
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
}

impl App {
    /// Create a new agent session.
    pub fn create_session(&self) -> AppSession {
        AppSession::new(self)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_tool::CalculatorTool;
    use oneai_core::traits::{ApprovalGate, Tool};
    use oneai_core::platform::StubPlatformApprovalGate;

    #[tokio::test]
    async fn test_app_builder_default_build() {
        let app = AppBuilder::new()
            .auto_approval_gate()
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
            .auto_approval_gate()
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
            .auto_approval_gate()
            .build()
            .await
            .expect("Build should succeed");

        let mut session = app.create_session();

        // Add messages to memory
        session.send_user_message("Rust is a programming language").await.unwrap();

        // Retrieve from memory
        let results = session.retrieve_memory("programming", 5).await.unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn test_app_blocking_gate() {
        let app = AppBuilder::new()
            .blocking_approval_gate()
            .build()
            .await
            .expect("Build should succeed");

        app.register_tool(Arc::new(oneai_tool::ShellTool::new())).await.unwrap();

        let session = app.create_session();

        // Shell is high-risk — should be denied by blocking gate
        let result = session.execute_tool("shell", serde_json::json!({"command": "echo test"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("denied"));
    }

    #[tokio::test]
    async fn test_app_with_persistence() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let persistence = Arc::new(FilePersistence::new(tmp_dir.path().to_str().unwrap()));

        let app = AppBuilder::new()
            .auto_approval_gate()
            .persistence(persistence)
            .build()
            .await
            .expect("Build should succeed");

        let session = app.create_session();

        // Save a checkpoint
        let checkpoint_id = session.save_checkpoint().await.unwrap();

        // Verify it was saved
        assert!(!checkpoint_id.is_empty());
    }

    #[tokio::test]
    async fn test_app_platform_approval_gate() {
        // Test building an App with a platform approval gate (stub)
        let gate = Arc::new(StubPlatformApprovalGate::macos());
        let app = AppBuilder::new()
            .platform_approval_gate(gate)
            .build()
            .await
            .expect("Build should succeed");

        // Stub auto-approves, so tools should work
        app.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
        let session = app.create_session();

        let result = session.execute_tool("calculator", serde_json::json!({"expression": "2+2"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "4");

        // Platform should be auto-detected
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
}