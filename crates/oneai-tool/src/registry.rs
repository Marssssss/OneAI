//! Tool registry — registration, lookup, and execution of tools.

use std::collections::HashMap;
use std::sync::Arc;
use oneai_core::traits::Tool;
use oneai_core::ToolOutput;
use oneai_core::error::{OneAIError, Result};
use tokio::sync::RwLock;

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
        let tool = tools.get(name).ok_or_else(|| {
            OneAIError::Tool(format!("Tool '{}' not found", name))
        })?;
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