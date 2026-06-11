//! Tool executor — orchestrates tool execution with approval gating.
//!
//! The ToolExecutor is the primary interface for executing tools in the OneAI framework.
//! It combines the ToolRegistry and ApprovalGate to provide a unified execution flow:
//!
//! 1. Look up the tool in the registry
//! 2. Check the tool's risk level
//! 3. If the tool is high-risk, request approval from the ApprovalGate
//! 4. If approved (or low-risk), execute the tool
//! 5. Return the result
//!
//! The ToolExecutor also supports:
//! - Argument modification via the ApprovalGate (user can modify args before execution)
//! - Execution logging/tracing
//! - Timeout enforcement for tool execution

use std::sync::Arc;
use std::time::Duration;

use oneai_core::{ApprovalRequest, ApprovalResponse, RiskLevel, PermissionLevel, ToolOutput};
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{ApprovalGate, Tool};

use crate::registry::ToolRegistry;
use crate::approval::BlockingApprovalGate;

/// Configuration for the ToolExecutor.
#[derive(Debug, Clone)]
pub struct ToolExecutorConfig {
    /// Default timeout for tool execution (in seconds).
    pub default_timeout_secs: u64,
    /// Whether to require approval for Medium-risk tools.
    /// By default, only High-risk tools require approval.
    pub require_approval_for_medium: bool,
}

impl Default for ToolExecutorConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 60,
            require_approval_for_medium: false,
        }
    }
}

/// Tool executor that orchestrates tool execution with approval gating.
///
/// The ToolExecutor is the primary interface for executing tools in the agent loop.
/// It combines the ToolRegistry and ApprovalGate to provide:
/// - Automatic approval gating for high-risk tools
/// - Argument modification via the approval flow
/// - Timeout enforcement
/// - Execution logging
pub struct ToolExecutor {
    /// Tool registry for looking up and executing tools.
    registry: Arc<ToolRegistry>,
    /// Approval gate for high-risk tool approval.
    approval_gate: Arc<dyn ApprovalGate>,
    /// Configuration.
    config: ToolExecutorConfig,
}

impl ToolExecutor {
    /// Create a new tool executor with a blocking (always-deny) approval gate.
    ///
    /// Useful for testing environments where no UI is available.
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            approval_gate: Arc::new(BlockingApprovalGate),
            config: ToolExecutorConfig::default(),
        }
    }

    /// Create a tool executor with a custom approval gate.
    pub fn with_approval_gate(
        registry: Arc<ToolRegistry>,
        approval_gate: Arc<dyn ApprovalGate>,
    ) -> Self {
        Self {
            registry,
            approval_gate,
            config: ToolExecutorConfig::default(),
        }
    }

    /// Create a tool executor with custom configuration and approval gate.
    pub fn with_config(
        registry: Arc<ToolRegistry>,
        approval_gate: Arc<dyn ApprovalGate>,
        config: ToolExecutorConfig,
    ) -> Self {
        Self {
            registry,
            approval_gate,
            config,
        }
    }

    /// Execute a tool by name with the given arguments.
    ///
    /// The execution flow:
    /// 1. Look up the tool in the registry
    /// 2. Check if the tool requires approval (based on risk level)
    /// 3. If approval is needed, request it from the approval gate
    /// 4. If approved, use the (possibly modified) args to execute the tool
    /// 5. Return the result
    ///
    /// Returns an error if:
    /// - The tool is not found in the registry
    /// - The approval gate denies the request
    /// - The tool execution fails
    /// - The tool execution times out
    pub async fn execute(&self, tool_name: &str, args: serde_json::Value) -> Result<ToolOutput> {
        // Look up the tool
        let tool = self.registry.get(tool_name).await.ok_or_else(|| {
            OneAIError::Tool(format!("Tool '{}' not found", tool_name))
        })?;

        // Check if the tool requires approval
        let needs_approval = self.needs_approval(&tool);

        if needs_approval {
            // Request approval
            let approval_request = ApprovalRequest {
                tool_name: tool_name.to_string(),
                args: args.clone(),
                risk_level: tool.risk_level(),
                permission_level: Some(PermissionLevel::from_risk_level(tool.risk_level())),
                justification: format!(
                    "Tool '{}' with risk level {:?} requires human approval",
                    tool_name, tool.risk_level()
                ),
            };

            let approval_response = self.approval_gate.request_approval(approval_request).await?;

            match approval_response {
                ApprovalResponse::Approved { modified_args } => {
                    // Use modified args if provided, otherwise use original args
                    let final_args = modified_args.unwrap_or(args);
                    tracing::info!(
                        "Tool '{}' approved for execution with args: {}",
                        tool_name, final_args
                    );
                    self.execute_with_timeout(tool, final_args).await
                }
                ApprovalResponse::Denied { reason } => {
                    tracing::warn!(
                        "Tool '{}' denied: {}", tool_name, reason
                    );
                    Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Execution denied: {}", reason)),
                    })
                }
                ApprovalResponse::Modified { args: modified_args } => {
                    tracing::info!(
                        "Tool '{}' approved with modified args: {}",
                        tool_name, modified_args
                    );
                    self.execute_with_timeout(tool, modified_args).await
                }
                ApprovalResponse::Observe { observation } => {
                    tracing::info!(
                        "Tool '{}' execution paused for observation: {}",
                        tool_name, observation
                    );
                    Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Execution paused for observation: {}", observation)),
                    })
                }
            }
        } else {
            // No approval needed — execute directly
            tracing::info!(
                "Tool '{}' executing directly (risk level: {:?})",
                tool_name, tool.risk_level()
            );
            self.execute_with_timeout(tool, args).await
        }
    }

    /// Check if a tool requires approval based on its risk level and executor config.
    fn needs_approval(&self, tool: &Arc<dyn Tool>) -> bool {
        match tool.risk_level() {
            RiskLevel::High => true,
            RiskLevel::Medium => self.config.require_approval_for_medium,
            RiskLevel::Low => false,
        }
    }

    /// Execute a tool with timeout enforcement.
    async fn execute_with_timeout(
        &self,
        tool: Arc<dyn Tool>,
        args: serde_json::Value,
    ) -> Result<ToolOutput> {
        let timeout = Duration::from_secs(self.config.default_timeout_secs);

        let result = tokio::time::timeout(timeout, tool.execute(args)).await;

        match result {
            Ok(output) => output, // output is already Result<ToolOutput, OneAIError>
            Err(_) => {
                Ok(ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!(
                        "Tool '{}' timed out after {} seconds",
                        tool.name(), self.config.default_timeout_secs
                    )),
                })
            }
        }
    }

    /// Register a tool in the registry.
    pub async fn register_tool(&self, tool: Arc<dyn Tool>) -> Result<()> {
        self.registry.register(tool).await
    }

    /// List all registered tool names.
    pub async fn list_tools(&self) -> Vec<String> {
        self.registry.list_names().await
    }

    /// Get the tool registry.
    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// Get the approval gate.
    pub fn approval_gate(&self) -> &Arc<dyn ApprovalGate> {
        &self.approval_gate
    }

    /// Get the configuration.
    pub fn config(&self) -> &ToolExecutorConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_tools::CalculatorTool;
    use crate::tool_interfaces::{ShellTool, FileReadTool, FileEditTool};
    use crate::approval::{AutoApprovalGate, ChannelApprovalGateWithThreshold, ApprovalDecision};
    use oneai_core::RiskLevel;

    #[tokio::test]
    async fn test_tool_executor_auto_approve_low_risk() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();

        let executor = ToolExecutor::new(registry);

        // Calculator is low-risk — should execute without approval
        let result = executor.execute("calculator", serde_json::json!({"expression": "2+3"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5");
    }

    #[tokio::test]
    async fn test_tool_executor_auto_approve_gate() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(ShellTool::new())).await.unwrap();

        // Use AutoApprovalGate — all requests are approved
        let executor = ToolExecutor::with_approval_gate(
            registry,
            Arc::new(AutoApprovalGate),
        );

        // Shell is high-risk — should be auto-approved
        let result = executor.execute("shell", serde_json::json!({"command": "echo hello"})).await;
        // ShellTool requires a real system, so the result depends on the environment
        // But it should NOT be denied
        assert!(result.is_ok());
        let output = result.unwrap();
        // It should either succeed (real shell) or be denied with a different reason
        if !output.success && output.error.as_ref().map(|e| e.contains("denied")).unwrap_or(false) {
            panic!("Should not be denied by approval gate");
        }
    }

    #[tokio::test]
    async fn test_tool_executor_blocking_gate() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(ShellTool::new())).await.unwrap();

        // Use blocking (always-deny) gate
        let executor = ToolExecutor::new(registry);

        // Shell is high-risk — should be denied by the blocking gate
        let result = executor.execute("shell", serde_json::json!({"command": "echo hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("denied"));
    }

    #[tokio::test]
    async fn test_tool_executor_channel_approve() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(ShellTool::new())).await.unwrap();

        let (gate, mut receiver) = ChannelApprovalGateWithThreshold::new_manual_only(16);

        // Spawn a task that approves all requests
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                item.response_tx.send(ApprovalDecision::approve()).unwrap();
            }
        });

        let executor = ToolExecutor::with_approval_gate(
            registry,
            Arc::new(gate),
        );

        let result = executor.execute("shell", serde_json::json!({"command": "echo hello"})).await;
        assert!(result.is_ok());
        // Should not be denied
        let output = result.unwrap();
        assert!(!output.error.as_ref().map(|e| e.contains("denied")).unwrap_or(false));
    }

    #[tokio::test]
    async fn test_tool_executor_channel_deny() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(ShellTool::new())).await.unwrap();

        let (gate, mut receiver) = ChannelApprovalGateWithThreshold::new_manual_only(16);

        // Spawn a task that denies all requests
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                item.response_tx.send(ApprovalDecision::deny("Forbidden")).unwrap();
            }
        });

        let executor = ToolExecutor::with_approval_gate(
            registry,
            Arc::new(gate),
        );

        let result = executor.execute("shell", serde_json::json!({"command": "echo hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("Forbidden"));
    }

    #[tokio::test]
    async fn test_tool_executor_channel_modify() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();

        let (gate, mut receiver) = ChannelApprovalGateWithThreshold::new(16, RiskLevel::Medium);

        // Spawn a task that modifies the args
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                // Modify the expression
                item.response_tx.send(ApprovalDecision::modify(
                    serde_json::json!({"expression": "10 * 5"})
                )).unwrap();
            }
        });

        let executor = ToolExecutor::with_approval_gate(
            registry,
            Arc::new(gate),
        );

        // Calculator is low-risk — should be auto-approved, not modified
        let result = executor.execute("calculator", serde_json::json!({"expression": "2+3"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5"); // Original expression, not modified
    }

    #[tokio::test]
    async fn test_tool_executor_not_found() {
        let registry = Arc::new(ToolRegistry::new());
        let executor = ToolExecutor::new(registry);

        let result = executor.execute("nonexistent", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_tool_executor_require_medium_approval() {
        let registry = Arc::new(ToolRegistry::new());
        // Use FileEditTool which has Standard/Medium permission level
        registry.register(Arc::new(FileEditTool::new())).await.unwrap();

        let config = ToolExecutorConfig {
            require_approval_for_medium: true,
            default_timeout_secs: 60,
        };

        let executor = ToolExecutor::with_config(
            registry,
            Arc::new(BlockingApprovalGate),
            config,
        );

        // FileEditTool is Standard-permission (Medium risk) — should be denied with blocking gate
        let result = executor.execute("edit_file", serde_json::json!({"file_path": "/tmp/test", "old_string": "a", "new_string": "b"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("denied"));
    }

    #[tokio::test]
    async fn test_tool_executor_register_and_list() {
        let registry = Arc::new(ToolRegistry::new());
        let executor = ToolExecutor::new(registry.clone());

        executor.register_tool(Arc::new(CalculatorTool::new())).await.unwrap();
        executor.register_tool(Arc::new(FileReadTool::new())).await.unwrap();

        let tools = executor.list_tools().await;
        assert_eq!(tools.len(), 2);
    }
}