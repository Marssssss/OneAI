//! Tool registry — registration, lookup, and execution of tools.

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::Tool;
use oneai_core::ToolOutput;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A registration-level `check_fn` — evaluated on the tool-definition hot
/// path to decide whether a tool stays in the schema sent to the model.
///
/// Returning `false` gives the tool **zero footprint** (excluded from the
/// schema, not merely "disabled"). See `Tool::service_available` and the
/// Footprint Ladder in `CLAUDE.md`.
pub type ServiceCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// A `Tool` wrapper that gates an inner tool behind a `check_fn`.
///
/// This is the registration-level seam for the Footprint gate: it lets a
/// `DomainPack` or app config conditionally hide *any* tool — including ones
/// whose impl lives in a crate that can't depend on the gating logic — without
/// the tool itself implementing `service_available`. All `Tool` methods
/// delegate to the inner tool; only `service_available()` consults the
/// `check_fn` (returning `false` when the prerequisite is missing).
pub struct GatedTool {
    inner: Arc<dyn Tool>,
    check: ServiceCheck,
}

impl GatedTool {
    /// Wrap `inner` with a `check_fn`. The tool is visible to the model only
    /// while `check()` returns `true`.
    pub fn new(inner: Arc<dyn Tool>, check: ServiceCheck) -> Self {
        Self { inner, check }
    }
}

#[async_trait::async_trait]
impl Tool for GatedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn risk_level(&self) -> oneai_core::RiskLevel {
        self.inner.risk_level()
    }
    fn service_available(&self) -> bool {
        (self.check)()
    }
    fn exposure(&self) -> oneai_core::ToolExposure {
        self.inner.exposure()
    }
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        self.inner.execute(args).await
    }
}

/// Registry for managing tools.
///
/// Supports registration, lookup, and execution of local tools, MCP tools,
/// and platform-specific tools. High-risk tools are gated through the ApprovalGate.
pub struct ToolRegistry {
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    /// Create a new empty tool registry.
    pub fn new() -> Self {
        Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the internal tools map as `Arc<RwLock<HashMap>>`.
    /// This allows sharing the map with AgentLoop and WorkflowExecutor.
    pub fn tools_map(&self) -> Arc<RwLock<HashMap<String, Arc<dyn Tool>>>> {
        self.tools.clone()
    }

    /// Register a tool.
    pub async fn register(&self, tool: Arc<dyn Tool>) -> Result<()> {
        let mut tools = self.tools.write().await;
        tools.insert(tool.name().to_string(), tool);
        Ok(())
    }

    /// Override an already-registered tool by name (Phase 4.2 — Gondolin
    /// tool-override). Functionally equivalent to [`register`](Self::register)
    /// (both `insert` by `tool.name()`, so the new tool replaces any same-named
    /// entry), but signals *intent* and emits an audit log so a pack author
    /// can't silently clobber a built-in. Use this when a `ContainerizedCodingPack`
    /// swaps a same-named tool (`read_file`/`shell`/…) for a VM-backed impl.
    pub async fn override_tool(&self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.name().to_string();
        let was_present = {
            let tools = self.tools.read().await;
            tools.contains_key(&name)
        };
        if was_present {
            tracing::info!("ToolRegistry: overriding tool '{}'", name);
        } else {
            tracing::warn!(
                "ToolRegistry: override_tool('{}') called but no prior tool with that name was registered — inserting new",
                name
            );
        }
        let mut tools = self.tools.write().await;
        tools.insert(name, tool);
        Ok(())
    }

    /// Register a tool gated behind a `check_fn` (Footprint gate).
    ///
    /// The tool is registered under its own name, wrapped in a `GatedTool`.
    /// While `check()` returns `false` the `AgentLoop` excludes it from the
    /// schema sent to the model — zero footprint — so the model never sees a
    /// tool whose backing service is missing. When `check()` flips back to
    /// `true` the tool reappears on the next tool-definition build.
    pub async fn register_gated(&self, tool: Arc<dyn Tool>, check: ServiceCheck) -> Result<()> {
        let gated = Arc::new(GatedTool::new(tool, check)) as Arc<dyn Tool>;
        self.register(gated).await
    }

    /// Get a tool by name.
    pub async fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let tools = self.tools.read().await;
        tools.get(name).cloned()
    }

    /// List all registered tool names.
    pub async fn list_names(&self) -> Vec<String> {
        let tools = self.tools.read().await;
        tools.keys().cloned().collect()
    }

    /// Execute a tool by name with the given arguments.
    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<ToolOutput> {
        let tools = self.tools.read().await;
        let tool = tools
            .get(name)
            .ok_or_else(|| OneAIError::Tool(format!("Tool '{}' not found", name)))?;
        tool.execute(args).await
    }

    /// Remove a tool by name.
    pub async fn unregister(&self, name: &str) -> Result<()> {
        let mut tools = self.tools.write().await;
        tools.remove(name);
        Ok(())
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
