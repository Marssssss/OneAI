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
use oneai_memory::MemoryManager;
use oneai_rag::DocumentIndex;
use oneai_skill::SkillSelector;
use oneai_parser::ThreeLayerParser;
use oneai_workflow::WorkflowExecutor;
use oneai_persistence::FilePersistence;
use oneai_trace::{TraceContext, TraceEmitter, InMemoryCollector};

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

    /// Build the application.
    pub fn build(self) -> Result<App> {
        let approval_gate = self.approval_gate.unwrap_or_else(|| {
            Arc::new(BlockingApprovalGate)
        });

        let parser = self.parser.unwrap_or_else(|| {
            Arc::new(ThreeLayerParser::new())
        });

        let memory_manager = self.memory_manager.unwrap_or_else(|| {
            Arc::new(MemoryManager::new())
        });

        let tool_executor = Arc::new(ToolExecutor::with_approval_gate(
            self.tool_registry.clone(),
            approval_gate.clone(),
        ));

        // Build workflow executor with the tool registry
        // We need to create a HashMap<String, Arc<dyn Tool>> from the registry
        // Since we can't easily extract all tools from the async registry in sync context,
        // we'll create an empty map and let the workflow executor handle tool lookup separately
        let workflow_executor = Arc::new(WorkflowExecutor::new(
            Arc::new(std::collections::HashMap::new()),
            approval_gate.clone(),
        ));

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
            .expect("Build should succeed");

        assert!(!app.has_provider()); // No provider set
        assert!(app.tool_executor().list_tools().await.is_empty());
    }

    #[tokio::test]
    async fn test_app_register_and_use_tool() {
        let app = AppBuilder::new()
            .auto_approval_gate()
            .build()
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
            .expect("Build should succeed");

        // Platform should be Android (set by the adapter)
        assert_eq!(*app.platform(), Platform::Android);
    }
}