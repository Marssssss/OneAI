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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{InteractionGate, Tool};
use oneai_core::{
    ApprovalRequest, InteractionModification, InteractionPoint, InteractionRequest,
    InteractionResponse, PermissionLevel, RiskLevel, ToolOutput,
};

use crate::interaction_gate::DenyAllInteractionGate;
use crate::registry::ToolRegistry;

/// Configuration for the ToolExecutor.
#[derive(Debug, Clone)]
pub struct ToolExecutorConfig {
    /// Default timeout for tool execution (in seconds).
    pub default_timeout_secs: u64,
    /// Whether to require approval for Medium-risk tools.
    /// By default, only High-risk tools require approval.
    pub require_approval_for_medium: bool,
    /// Maximum size of a tool's textual output in **bytes**. Outputs larger
    /// than this are truncated at a UTF-8 char boundary and tagged with a
    /// `[output truncated]` marker before being returned to the agent loop.
    ///
    /// This is the **single chokepoint** that bounds tool result size
    /// regardless of whether an individual tool self-truncates — it protects
    /// the context window from runaway MCP / custom-tool output that would
    /// otherwise blow past the compressor trigger. Set to `0` to disable.
    pub max_output_bytes: usize,
}

/// Default per-tool output cap: 1 MiB. Generous enough to never bite normal
/// tool output (the largest built-in self-truncating tool, `FileReadTool`,
/// also caps at 1 MiB), but small enough that a runaway multi-MB output from
/// an unbounded MCP/custom tool cannot overflow the context before the
/// compressor runs.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

impl Default for ToolExecutorConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 60,
            require_approval_for_medium: false,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Tool executor that orchestrates tool execution with interaction gating.
///
/// The ToolExecutor is the primary interface for executing tools in the agent loop.
/// It combines the ToolRegistry and InteractionGate to provide:
/// - Automatic approval gating for high-risk tools (via the ToolApproval point)
/// - Argument modification via the interaction flow (ProceedWith → ReplaceToolArgs)
/// - Timeout enforcement
/// - Execution logging
pub struct ToolExecutor {
    /// Tool registry for looking up and executing tools.
    registry: Arc<ToolRegistry>,
    /// Interaction gate — the ToolApproval decision point for high-risk tools.
    interaction_gate: Arc<dyn InteractionGate>,
    /// Configuration.
    config: ToolExecutorConfig,
}

impl ToolExecutor {
    /// Create a new tool executor with a deny-all interaction gate.
    ///
    /// Useful for testing environments where every high-risk tool must be
    /// rejected outright (the replacement for the removed `BlockingApprovalGate`).
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            interaction_gate: Arc::new(DenyAllInteractionGate),
            config: ToolExecutorConfig::default(),
        }
    }

    /// Create a tool executor with a custom interaction gate.
    pub fn with_interaction_gate(
        registry: Arc<ToolRegistry>,
        interaction_gate: Arc<dyn InteractionGate>,
    ) -> Self {
        Self {
            registry,
            interaction_gate,
            config: ToolExecutorConfig::default(),
        }
    }

    /// Create a tool executor with custom configuration and interaction gate.
    pub fn with_config(
        registry: Arc<ToolRegistry>,
        interaction_gate: Arc<dyn InteractionGate>,
        config: ToolExecutorConfig,
    ) -> Self {
        Self {
            registry,
            interaction_gate,
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
        let tool = self
            .registry
            .get(tool_name)
            .await
            .ok_or_else(|| OneAIError::Tool(format!("Tool '{}' not found", tool_name)))?;

        // Check if the tool requires approval
        let needs_approval = self.needs_approval(&tool);

        if needs_approval
            && self
                .interaction_gate
                .enabled(InteractionPoint::ToolApproval)
        {
            // Ask the interaction gate's ToolApproval point whether to proceed.
            let approval_request = ApprovalRequest {
                tool_name: tool_name.to_string(),
                args: args.clone(),
                risk_level: tool.risk_level(),
                permission_level: Some(PermissionLevel::from_risk_level(tool.risk_level())),
                justification: format!(
                    "Tool '{}' with risk level {:?} requires human approval",
                    tool_name,
                    tool.risk_level()
                ),
            };

            let response = self
                .interaction_gate
                .request(InteractionRequest::ToolApproval {
                    approval: approval_request,
                })
                .await?;

            match response {
                InteractionResponse::Proceed => {
                    tracing::info!(
                        "Tool '{}' approved for execution with args: {}",
                        tool_name,
                        args
                    );
                    self.execute_with_timeout(tool, args).await
                }
                InteractionResponse::ProceedWith { modification } => {
                    // ToolApproval only honours an arg rewrite; other modifications
                    // (which don't apply here) fall through to the original args.
                    let final_args = match modification {
                        InteractionModification::ReplaceToolArgs(new_args) => new_args,
                        _ => args,
                    };
                    tracing::info!(
                        "Tool '{}' approved with modified args: {}",
                        tool_name,
                        final_args
                    );
                    self.execute_with_timeout(tool, final_args).await
                }
                InteractionResponse::Abort { reason } => {
                    tracing::warn!("Tool '{}' denied: {}", tool_name, reason);
                    Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Execution denied: {}", reason)),
                    })
                }
                InteractionResponse::Revise { feedback } => {
                    // The direct execute_tool path can't loop on feedback, so a
                    // Revise is surfaced as a denial carrying the feedback.
                    tracing::warn!("Tool '{}' revise-feedback: {}", tool_name, feedback);
                    Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Execution denied: {}", feedback)),
                    })
                }
                InteractionResponse::Choose { .. } => {
                    // PlanDecision-only reply; doesn't apply to ToolApproval. Proceed.
                    self.execute_with_timeout(tool, args).await
                }
                // InteractionResponse is #[non_exhaustive]; unknown variants
                // (e.g. future decision points) default to proceeding.
                _ => self.execute_with_timeout(tool, args).await,
            }
        } else {
            // No approval needed (or the gate disabled the ToolApproval point) —
            // execute directly. A disabled point short-circuits to auto-proceed,
            // which mirrors the agent-loop's behaviour under NoopInteractionGate.
            tracing::info!(
                "Tool '{}' executing directly (risk level: {:?})",
                tool_name,
                tool.risk_level()
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

        let output = match result {
            Ok(output) => output, // output is already Result<ToolOutput, OneAIError>
            Err(_) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!(
                    "Tool '{}' timed out after {} seconds",
                    tool.name(),
                    self.config.default_timeout_secs
                )),
            }),
        };

        Ok(self.enforce_output_limit(tool.name(), output?))
    }

    /// Bound a tool's textual output to `max_output_bytes`. The single
    /// chokepoint that protects the context window from runaway output
    /// (e.g. an unbounded MCP / custom tool) regardless of whether the tool
    /// self-truncates. `max_output_bytes == 0` disables the guard.
    fn enforce_output_limit(&self, tool_name: &str, mut output: ToolOutput) -> ToolOutput {
        let cap = self.config.max_output_bytes;
        if cap == 0 || output.content.len() <= cap {
            return output;
        }
        let original_len = output.content.len();
        // Walk back to the nearest UTF-8 char boundary so we never split a
        // multi-byte sequence (which would produce an invalid String).
        let mut cut = cap;
        while cut > 0 && !output.content.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut truncated = String::from(&output.content[..cut]);
        truncated.push_str(&format!(
            "\n...[output truncated: tool '{}' returned {} bytes, exceeded {} byte limit]",
            tool_name, original_len, cap
        ));
        output.content = truncated;
        output
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
    /// Get the tool registry.
    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// Get the tools map (shared with registry) for use by AgentLoop.
    pub fn tools_map(&self) -> Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>> {
        self.registry.tools_map()
    }

    /// Get the interaction gate.
    pub fn interaction_gate(&self) -> &Arc<dyn InteractionGate> {
        &self.interaction_gate
    }

    /// Get the configuration.
    pub fn config(&self) -> &ToolExecutorConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction_gate::{ChannelInteractionGate, NoopInteractionGate};
    use crate::local_tools::CalculatorTool;
    use crate::tool_interfaces::{FileEditTool, FileReadTool, ShellTool};
    use oneai_core::InteractionResponse;

    #[tokio::test]
    async fn test_tool_executor_auto_approve_low_risk() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();

        let executor = ToolExecutor::new(registry);

        // Calculator is low-risk — should execute without approval
        let result = executor
            .execute("calculator", serde_json::json!({"expression": "2+3"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5");
    }

    #[tokio::test]
    async fn test_tool_executor_auto_approve_gate() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(ShellTool::new())).await.unwrap();

        // NoopInteractionGate disables the ToolApproval point → auto-proceed.
        let executor = ToolExecutor::with_interaction_gate(registry, Arc::new(NoopInteractionGate));

        // Shell is high-risk — should be auto-approved
        let result = executor
            .execute("shell", serde_json::json!({"command": "echo hello"}))
            .await;
        // ShellTool requires a real system, so the result depends on the environment
        // But it should NOT be denied
        assert!(result.is_ok());
        let output = result.unwrap();
        // It should either succeed (real shell) or be denied with a different reason
        if !output.success
            && output
                .error
                .as_ref()
                .map(|e| e.contains("denied"))
                .unwrap_or(false)
        {
            panic!("Should not be denied by approval gate");
        }
    }

    #[tokio::test]
    async fn test_tool_executor_blocking_gate() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(ShellTool::new())).await.unwrap();

        // ToolExecutor::new defaults to DenyAllInteractionGate (always abort).
        let executor = ToolExecutor::new(registry);

        // Shell is high-risk — should be denied by the deny-all gate
        let result = executor
            .execute("shell", serde_json::json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("denied"));
    }

    #[tokio::test]
    async fn test_tool_executor_channel_approve() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(ShellTool::new())).await.unwrap();

        let (gate, mut receiver) = ChannelInteractionGate::new(16);

        // Spawn a task that approves all requests
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                item.response_tx.send(InteractionResponse::Proceed).unwrap();
            }
        });

        let executor = ToolExecutor::with_interaction_gate(registry, Arc::new(gate));

        let result = executor
            .execute("shell", serde_json::json!({"command": "echo hello"}))
            .await;
        assert!(result.is_ok());
        // Should not be denied
        let output = result.unwrap();
        assert!(!output
            .error
            .as_ref()
            .map(|e| e.contains("denied"))
            .unwrap_or(false));
    }

    #[tokio::test]
    async fn test_tool_executor_channel_deny() {
        let registry = Arc::new(ToolRegistry::new());
        registry.register(Arc::new(ShellTool::new())).await.unwrap();

        let (gate, mut receiver) = ChannelInteractionGate::new(16);

        // Spawn a task that denies all requests
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                item.response_tx
                    .send(InteractionResponse::Abort {
                        reason: "Forbidden".to_string(),
                    })
                    .unwrap();
            }
        });

        let executor = ToolExecutor::with_interaction_gate(registry, Arc::new(gate));

        let result = executor
            .execute("shell", serde_json::json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("Forbidden"));
    }

    #[tokio::test]
    async fn test_tool_executor_channel_modify() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();

        let (gate, mut receiver) = ChannelInteractionGate::new(16);

        // Spawn a task that would modify the args (replace them).
        tokio::spawn(async move {
            while let Some(item) = receiver.recv().await {
                item.response_tx
                    .send(InteractionResponse::ProceedWith {
                        modification: InteractionModification::ReplaceToolArgs(
                            serde_json::json!({"expression": "10 * 5"}),
                        ),
                    })
                    .unwrap();
            }
        });

        let executor = ToolExecutor::with_interaction_gate(registry, Arc::new(gate));

        // Calculator is low-risk — bypasses the ToolApproval point, so the
        // spawn task is never reached and the original expression runs.
        let result = executor
            .execute("calculator", serde_json::json!({"expression": "2+3"}))
            .await
            .unwrap();
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
        registry
            .register(Arc::new(FileEditTool::new()))
            .await
            .unwrap();

        let config = ToolExecutorConfig {
            require_approval_for_medium: true,
            default_timeout_secs: 60,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        };

        let executor =
            ToolExecutor::with_config(registry, Arc::new(DenyAllInteractionGate), config);

        // FileEditTool is Standard-permission (Medium risk) — should be denied with blocking gate
        let result = executor
            .execute(
                "edit_file",
                serde_json::json!({"file_path": "/tmp/test", "old_string": "a", "new_string": "b"}),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("denied"));
    }

    #[tokio::test]
    async fn test_tool_executor_register_and_list() {
        let registry = Arc::new(ToolRegistry::new());
        let executor = ToolExecutor::new(registry.clone());

        executor
            .register_tool(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();
        executor
            .register_tool(Arc::new(FileReadTool::new()))
            .await
            .unwrap();

        let tools = executor.list_tools().await;
        assert_eq!(tools.len(), 2);
    }

    /// A mock tool whose `execute` returns an arbitrary-size payload — used to
    /// exercise the executor-level output cap (the single chokepoint that
    /// bounds tool result size regardless of whether a tool self-truncates).
    struct VerboseTool {
        payload: String,
    }

    #[async_trait::async_trait]
    impl Tool for VerboseTool {
        fn name(&self) -> &str {
            "verbose"
        }
        fn description(&self) -> &str {
            "returns a fixed payload"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn risk_level(&self) -> RiskLevel {
            RiskLevel::Low
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput {
                success: true,
                content: self.payload.clone(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn test_tool_executor_truncates_oversized_output() {
        // 5000-byte payload, 1000-byte cap → must truncate + tag.
        let payload = "x".repeat(5000);
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(Arc::new(VerboseTool {
                payload: payload.clone(),
            }))
            .await
            .unwrap();

        let executor = ToolExecutor::with_config(
            registry,
            Arc::new(NoopInteractionGate),
            ToolExecutorConfig {
                default_timeout_secs: 60,
                require_approval_for_medium: false,
                max_output_bytes: 1000,
            },
        );

        let result = executor
            .execute("verbose", serde_json::json!({}))
            .await
            .unwrap();
        assert!(result.success);
        // Content must be well under the original 5000 bytes (truncation tag
        // adds only a short suffix).
        assert!(
            result.content.len() < 1100,
            "content not truncated: {} bytes",
            result.content.len()
        );
        assert!(result.content.contains("[output truncated"));
        assert!(result.content.contains("5000 bytes"));
        assert!(result.content.contains("1000 byte limit"));
        // Truncated content must still be valid UTF-8 (no split multibyte seq).
        assert!(std::str::from_utf8(result.content.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn test_tool_executor_preserves_small_output() {
        // Payload under the cap must pass through verbatim.
        let payload = "hello".to_string();
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(Arc::new(VerboseTool {
                payload: payload.clone(),
            }))
            .await
            .unwrap();

        let executor = ToolExecutor::with_config(
            registry,
            Arc::new(NoopInteractionGate),
            ToolExecutorConfig {
                default_timeout_secs: 60,
                require_approval_for_medium: false,
                max_output_bytes: 1000,
            },
        );

        let result = executor
            .execute("verbose", serde_json::json!({}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.content, "hello");
        assert!(!result.content.contains("truncated"));
    }

    #[tokio::test]
    async fn test_tool_executor_zero_cap_disables_guard() {
        // max_output_bytes == 0 → guard disabled, full payload returned.
        let payload = "x".repeat(5000);
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(Arc::new(VerboseTool {
                payload: payload.clone(),
            }))
            .await
            .unwrap();

        let executor = ToolExecutor::with_config(
            registry,
            Arc::new(NoopInteractionGate),
            ToolExecutorConfig {
                default_timeout_secs: 60,
                require_approval_for_medium: false,
                max_output_bytes: 0,
            },
        );

        let result = executor
            .execute("verbose", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.content.len(), 5000);
        assert!(!result.content.contains("truncated"));
    }
}
