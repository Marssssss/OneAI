//! HandoffTool & HandoffManager — agent handoff protocol implementation.
//!
//! The HandoffTool implements the Tool trait, so it appears as a regular
//! tool definition to the LLM. When the model decides to hand off, it
//! "calls" the handoff tool with a target agent name and reason, just
//! like any other tool call.
//!
//! The HandoffManager detects handoff tool calls in the AgentLoop's
//! iteration flow, extracts the target and reason, creates the receiving
//! agent via SubAgentFactory, transfers conversation context (or summary),
//! and continues execution with the receiving agent.
//!
//! Key design: Handoff as a Tool Call
//! - The model naturally decides when to hand off (no manual orchestration)
//! - The tool description tells the model which targets are available
//! - The tool's parameter schema includes "target" and "reason"
//! - The receiving agent can see the full history (if transfer_conversation=true)

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use oneai_core::error::{OneAIError, Result};
use oneai_core::handoff::{
    HandoffConfig, HandoffTarget, HandoffEvent, HandoffResult,
    HandoffChainEntry, HandoffLog, InMemoryHandoffLog,
};
use oneai_core::traits::Tool;
use oneai_core::types::{RiskLevel, ToolOutput};
use oneai_core::budget::TokenBudget;
use oneai_core::team::SubAgentKindProxy;

use crate::sub_agent::{SubAgentFactory, SubAgentKind};

// ─── HandoffTool ───────────────────────────────────────────────────────────────

/// HandoffTool — a Tool that enables the model to transfer control to another agent.
///
/// This is registered as a regular tool in the ToolRegistry, so the model
/// can decide to hand off naturally through tool calling. The tool's
/// description tells the model when each handoff target is appropriate.
///
/// When the model calls this tool:
/// 1. The AgentLoop detects it's a handoff call (tool name matches config.tool_name)
/// 2. The HandoffManager processes the handoff
/// 3. The receiving agent is created via SubAgentFactory
/// 4. The receiving agent runs with the transferred context
/// 5. The result flows back to the original caller
///
/// **Important**: This tool does NOT perform the actual handoff on execute().
/// The HandoffManager handles the handoff logic. The tool is purely a
/// signaling mechanism — when the model calls it, the HandoffManager
/// intercepts and processes the handoff.
pub struct HandoffTool {
    /// The handoff configuration (targets, depth limits, etc.).
    config: HandoffConfig,

    /// Cached tool description (generated from config).
    /// The Tool trait requires description() to return &str,
    /// but tool_description() returns a String. We cache it
    /// at construction time so we can return a reference.
    cached_description: String,
}

impl HandoffTool {
    /// Create a new HandoffTool with the given configuration.
    pub fn new(config: HandoffConfig) -> Self {
        let cached_description = config.tool_description();
        Self { config, cached_description }
    }

    /// Get the handoff configuration.
    pub fn config(&self) -> &HandoffConfig {
        &self.config
    }

    /// Parse handoff call arguments into target name and reason.
    ///
    /// The model provides arguments as JSON with "target" and "reason" fields.
    /// This function extracts and validates them.
    pub fn parse_handoff_args(args: &serde_json::Value) -> Result<(String, String)> {
        let target = args.get("target")
            .and_then(|v| v.as_str())
            .ok_or_else(|| OneAIError::Handoff(
                "Handoff tool call missing 'target' argument".to_string()
            ))?;

        let reason = args.get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| OneAIError::Handoff(
                "Handoff tool call missing 'reason' argument".to_string()
            ))?;

        Ok((target.to_string(), reason.to_string()))
    }

    /// Check if a tool call name matches the handoff tool name.
    pub fn is_handoff_call(tool_name: &str, config: &HandoffConfig) -> bool {
        tool_name == config.tool_name
    }
}

#[async_trait]
impl Tool for HandoffTool {
    fn name(&self) -> &str {
        &self.config.tool_name
    }

    fn description(&self) -> &str {
        &self.cached_description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.config.tool_parameters_schema()
    }

    fn risk_level(&self) -> RiskLevel {
        // Handoff is medium risk — it changes agent execution flow
        // but doesn't modify files or execute commands
        RiskLevel::Medium
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        // Parse the handoff arguments
        let (target, reason) = Self::parse_handoff_args(&args)?;

        // Validate the target
        if self.config.target_by_name(&target).is_none() {
            return Ok(ToolOutput {
                success: false,
                content: format!("Unknown handoff target '{}'. Available targets: {}",
                    target, self.config.target_names().join(", ")),
                error: Some(format!("Unknown target: {}", target)),
            });
        }

        // The actual handoff is handled by HandoffManager.
        // This tool just validates the arguments and returns a signal.
        // The AgentLoop/HandoffManager will intercept this tool call
        // and process the actual handoff.
        Ok(ToolOutput {
            success: true,
            content: format!("Handoff signal: target='{}', reason='{}'. The HandoffManager will process this handoff.", target, reason),
            error: None,
        })
    }
}

// ─── HandoffManager ────────────────────────────────────────────────────────────

/// Handoff manager — orchestrates agent handoff execution.
///
/// The HandoffManager is the runtime engine for the handoff protocol.
/// It takes a HandoffConfig and processes handoff requests by:
///
/// 1. Validating the target and depth constraints
/// 2. Creating the receiving agent via SubAgentFactory
/// 3. Transferring conversation context (or summary)
/// 4. Running the receiving agent with the transferred context
/// 5. Tracking the handoff chain and budget
/// 6. Returning a HandoffResult
///
/// The manager prevents infinite handoff loops by tracking depth
/// and checking against max_depth.
pub struct HandoffManager {
    /// The sub-agent factory for creating receiving agents.
    factory: Arc<dyn SubAgentFactory>,

    /// Default budget for handoff execution.
    default_budget: TokenBudget,

    /// Handoff log for recording events.
    log: Arc<dyn HandoffLog>,
}

impl HandoffManager {
    /// Create a new HandoffManager with the given sub-agent factory.
    pub fn new(factory: Arc<dyn SubAgentFactory>) -> Self {
        Self {
            factory,
            default_budget: TokenBudget::new(50_000),
            log: Arc::new(InMemoryHandoffLog::new()),
        }
    }

    /// Create a HandoffManager with a custom default budget.
    pub fn with_budget(factory: Arc<dyn SubAgentFactory>, budget: TokenBudget) -> Self {
        Self {
            factory,
            default_budget: budget,
            log: Arc::new(InMemoryHandoffLog::new()),
        }
    }

    /// Create a HandoffManager with all optional components.
    pub fn with_components(
        factory: Arc<dyn SubAgentFactory>,
        budget: TokenBudget,
        log: Arc<dyn HandoffLog>,
    ) -> Self {
        Self {
            factory,
            default_budget: budget,
            log,
        }
    }

    /// Get the handoff log.
    pub fn log(&self) -> &Arc<dyn HandoffLog> {
        &self.log
    }

    // ─── Execute Handoff ──────────────────────────────────────────────────────

    /// Execute a handoff from one agent to another.
    ///
    /// Takes the current agent's name, the target agent name, the reason,
    /// the current conversation context, the handoff config, and the current
    /// depth. Returns a HandoffResult with the receiving agent's output
    /// and the handoff chain history.
    ///
    /// The depth is tracked to prevent infinite handoff loops:
    /// - Each handoff increments depth
    /// - When depth >= max_depth, no further handoffs are allowed
    /// - The receiving agent produces a final answer
    pub async fn execute_handoff(
        &self,
        from_agent: &str,
        target_name: &str,
        reason: &str,
        conversation_context: &str,
        config: &HandoffConfig,
        current_depth: usize,
    ) -> Result<HandoffResult> {
        // Validate depth constraint
        if current_depth >= config.max_depth {
            return Err(OneAIError::Handoff(
                format!("Handoff depth {} exceeds max_depth {}. The current agent must produce a final answer.",
                    current_depth, config.max_depth)
            ));
        }

        // Find the target
        let target = config.target_by_name(target_name)
            .cloned()
            .ok_or_else(|| OneAIError::Handoff(
                format!("Unknown handoff target '{}'. Available targets: {}",
                    target_name, config.target_names().join(", "))
            ))?;

        // Log the handoff event
        self.log.log_handoff(HandoffEvent {
            from_agent: from_agent.to_string(),
            to_agent: target.agent_name.clone(),
            reason: reason.to_string(),
            conversation_transferred: config.transfer_conversation,
            depth: current_depth + 1,
            timestamp: Utc::now(),
        }).await;

        // Create the receiving agent
        let kind = self.resolve_kind(&target.agent_kind);
        let agent = self.factory.create(kind, self.default_budget.clone()).await?;

        // Build the task with context transfer
        let task = self.build_handoff_task(
            &target,
            reason,
            conversation_context,
            config.transfer_conversation,
            current_depth,
            config,
        );

        // Run the receiving agent
        let summary = agent.run(&task).await?;

        // Build the chain entry
        let chain_entry = HandoffChainEntry {
            from_agent: from_agent.to_string(),
            to_agent: target.agent_name.clone(),
            reason: reason.to_string(),
            partial_output: conversation_context.chars().take(200).collect(),
            tokens_used: summary.tokens_used,
        };

        // Build the result
        let result = HandoffResult {
            final_answer: summary.summary.clone(),
            chain: vec![chain_entry],
            total_tokens: summary.tokens_used,
            conversation_transferred: config.transfer_conversation,
            handoff_count: 1,
        };

        Ok(result)
    }

    // ─── Helpers ──────────────────────────────────────────────────────────────

    /// Resolve a SubAgentKindProxy to an actual SubAgentKind.
    fn resolve_kind(&self, proxy: &SubAgentKindProxy) -> SubAgentKind {
        SubAgentKind::from_str(proxy.name())
    }

    /// Build the task description for the receiving agent.
    ///
    /// Depending on transfer_conversation:
    /// - If true: full conversation history is included
    /// - If false: only a summary is included
    ///
    /// The task also includes:
    /// - The handoff reason (why the previous agent handed off)
    /// - The target's system prompt override (if set)
    /// - Depth information (whether further handoffs are allowed)
    fn build_handoff_task(
        &self,
        target: &HandoffTarget,
        reason: &str,
        conversation_context: &str,
        transfer_conversation: bool,
        current_depth: usize,
        config: &HandoffConfig,
    ) -> String {
        let mut task_parts = Vec::new();

        // Add context
        if transfer_conversation && !conversation_context.is_empty() {
            task_parts.push(format!(
                "You received a handoff from agent '{}'.\n\
                 Reason for handoff: {}\n\n\
                 Previous conversation context:\n{}",
                target.agent_name, reason, conversation_context
            ));
        } else if !conversation_context.is_empty() {
            // Summary-only: truncate context
            let summary = conversation_context.chars().take(500).collect::<String>();
            task_parts.push(format!(
                "You received a handoff from agent '{}'.\n\
                 Reason for handoff: {}\n\n\
                 Summary of previous work:\n{}",
                target.agent_name, reason, summary
            ));
        } else {
            task_parts.push(format!(
                "You received a handoff.\n\
                 Reason: {}",
                reason
            ));
        }

        // Add depth information
        let remaining_depth = config.max_depth - current_depth - 1;
        if remaining_depth > 0 && target.can_handoff {
            task_parts.push(format!(
                "\n\nYou can hand off to another agent if needed (remaining handoff depth: {}).",
                remaining_depth
            ));
        } else {
            task_parts.push("\n\nYou must produce a final answer — no further handoffs are available.".to_string());
        }

        // Add system prompt override if set
        if let Some(prompt) = &target.system_prompt_override {
            task_parts.push(format!("\n\nAdditional instructions: {}", prompt));
        }

        task_parts.join("")
    }

    /// Check if a handoff is allowed at the given depth.
    pub fn can_handoff_at_depth(&self, config: &HandoffConfig, depth: usize) -> bool {
        depth < config.max_depth
    }

    /// Get the remaining handoff depth.
    pub fn remaining_depth(&self, config: &HandoffConfig, current_depth: usize) -> usize {
        config.max_depth.saturating_sub(current_depth)
    }

    /// Extract the handoff target name from a model's response.
    ///
    /// The model may include extra text around the target name.
    /// This function attempts to match the response against available target names.
    pub fn extract_target_from_response(
        response: &str,
        config: &HandoffConfig,
    ) -> Option<String> {
        let response_lower = response.to_lowercase().trim().to_string();

        // Try exact match first
        for target in &config.targets {
            if response_lower == target.agent_name.to_lowercase() {
                return Some(target.agent_name.clone());
            }
        }

        // Try substring match
        for target in &config.targets {
            if response_lower.contains(&target.agent_name.to_lowercase()) {
                return Some(target.agent_name.clone());
            }
        }

        None
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::handoff::HandoffConfig;
    use oneai_core::handoff::HandoffPresets;
    // Test-only imports (kept out of the lib build to avoid unused-import warnings):
    use crate::sub_agent::{SubAgent, SubAgentSummary};

    // ─── Mock SubAgent ────────────────────────────────────────────────────────

    struct MockSubAgent {
        kind: SubAgentKind,
        response: String,
    }

    #[async_trait]
    impl SubAgent for MockSubAgent {
        async fn run(&self, task: &str) -> Result<SubAgentSummary> {
            Ok(SubAgentSummary {
                completed: true,
                summary: format!("{} (handoff task: {})", self.response, task.chars().take(50).collect::<String>()),
                key_findings: vec![format!("Finding from {}", self.kind.name())],
                budget_exceeded: false,
                agent_kind: self.kind.clone(),
                tokens_used: 2000,
            })
        }
        fn kind(&self) -> &SubAgentKind { &self.kind }
        fn budget(&self) -> &TokenBudget {
            static BUDGET: TokenBudget = TokenBudget { total: 10000, consumed: 0 };
            &BUDGET
        }
    }

    // ─── Mock Factory ────────────────────────────────────────────────────────

    struct MockFactory;

    #[async_trait]
    impl SubAgentFactory for MockFactory {
        async fn create(&self, kind: SubAgentKind, _budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
            let response: String = match &kind {
                SubAgentKind::Plan => "Plan: structured approach".to_string(),
                SubAgentKind::Explore => "Explore: comprehensive findings".to_string(),
                SubAgentKind::Code => "Code: implementation complete".to_string(),
                SubAgentKind::Review => "Review: quality assessment".to_string(),
                SubAgentKind::Custom(name) => format!("Custom {}: specialized result", name),
            };
            Ok(Box::new(MockSubAgent { kind, response }))
        }

        fn available_kinds(&self) -> Vec<SubAgentKind> {
            vec![SubAgentKind::Plan, SubAgentKind::Explore, SubAgentKind::Code, SubAgentKind::Review]
        }

        fn is_available(&self, kind: &SubAgentKind) -> bool {
            matches!(kind, SubAgentKind::Plan | SubAgentKind::Explore | SubAgentKind::Code | SubAgentKind::Review | SubAgentKind::Custom(_))
        }
    }

    // ─── HandoffTool Tests ────────────────────────────────────────────────────

    #[test]
    fn test_handoff_tool_name() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"));
        let tool = HandoffTool::new(config);
        assert_eq!(tool.name(), "handoff");
    }

    #[test]
    fn test_handoff_tool_custom_name() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"))
            .with_tool_name("transfer");
        let tool = HandoffTool::new(config);
        assert_eq!(tool.name(), "transfer");
    }

    #[test]
    fn test_handoff_tool_description() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code implementation"));
        let tool = HandoffTool::new(config);
        // Note: description() returns a reference, but tool_description() returns a String
        // The Tool trait requires description() to return &str, so we need to work around that
        // In practice, the description is generated from config
        assert!(tool.config().tool_description().contains("coding"));
    }

    #[test]
    fn test_handoff_tool_parameters_schema() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"));
        let tool = HandoffTool::new(config);
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["target"].is_object());
        assert!(schema["properties"]["reason"].is_object());
    }

    #[test]
    fn test_handoff_tool_risk_level() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"));
        let tool = HandoffTool::new(config);
        assert_eq!(tool.risk_level(), RiskLevel::Medium);
    }

    #[tokio::test]
    async fn test_handoff_tool_execute_valid() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"));
        let tool = HandoffTool::new(config);

        let result = tool.execute(serde_json::json!({
            "target": "coding",
            "reason": "Needs code implementation"
        })).await.unwrap();

        assert!(result.success);
        assert!(result.content.contains("Handoff signal"));
        assert!(result.content.contains("coding"));
    }

    #[tokio::test]
    async fn test_handoff_tool_execute_unknown_target() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"));
        let tool = HandoffTool::new(config);

        let result = tool.execute(serde_json::json!({
            "target": "unknown",
            "reason": "Some reason"
        })).await.unwrap();

        assert!(!result.success);
        assert!(result.content.contains("Unknown handoff target"));
    }

    #[tokio::test]
    async fn test_handoff_tool_execute_missing_args() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"));
        let tool = HandoffTool::new(config);

        let result = tool.execute(serde_json::json!({
            "reason": "Some reason"
        })).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'target'"));
    }

    #[test]
    fn test_parse_handoff_args() {
        let args = serde_json::json!({
            "target": "coding",
            "reason": "Needs code"
        });
        let (target, reason) = HandoffTool::parse_handoff_args(&args).unwrap();
        assert_eq!(target, "coding");
        assert_eq!(reason, "Needs code");
    }

    #[test]
    fn test_parse_handoff_args_missing_target() {
        let args = serde_json::json!({"reason": "Some reason"});
        let result = HandoffTool::parse_handoff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_handoff_args_missing_reason() {
        let args = serde_json::json!({"target": "coding"});
        let result = HandoffTool::parse_handoff_args(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_is_handoff_call() {
        let config = HandoffConfig::new()
            .with_tool_name("handoff");
        assert!(HandoffTool::is_handoff_call("handoff", &config));
        assert!(!HandoffTool::is_handoff_call("shell", &config));

        let custom_config = HandoffConfig::new()
            .with_tool_name("transfer");
        assert!(HandoffTool::is_handoff_call("transfer", &custom_config));
        assert!(!HandoffTool::is_handoff_call("handoff", &custom_config));
    }

    // ─── HandoffManager Tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_handoff_manager_execute_handoff() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffPresets::development_chain();
        let context = "Previous agent found that the codebase needs authentication implementation.";

        let result = manager.execute_handoff(
            "main", "coding", "Needs code implementation",
            context, &config, 0
        ).await.unwrap();

        assert!(!result.final_answer.is_empty());
        assert_eq!(result.handoff_count, 1);
        assert!(result.chain[0].from_agent == "main");
        assert!(result.chain[0].to_agent == "coding");
    }

    #[tokio::test]
    async fn test_handoff_manager_depth_exceeded() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffConfig::new()
            .with_max_depth(1)
            .with_target(HandoffTarget::new("coding", "Code"));

        let result = manager.execute_handoff(
            "main", "coding", "Needs code",
            "", &config, 1  // depth already at 1, max is 1
        ).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("depth"));
    }

    #[tokio::test]
    async fn test_handoff_manager_unknown_target() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"));

        let result = manager.execute_handoff(
            "main", "unknown", "Unknown target",
            "", &config, 0
        ).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown handoff target"));
    }

    #[tokio::test]
    async fn test_handoff_manager_with_context_transfer() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"))
            .with_transfer_conversation(true);

        let context = "The main agent analyzed the requirements and identified 3 key components.";
        let result = manager.execute_handoff(
            "main", "coding", "Implementation needed",
            context, &config, 0
        ).await.unwrap();

        assert!(result.conversation_transferred);
        assert!(!result.final_answer.is_empty());
    }

    #[tokio::test]
    async fn test_handoff_manager_summary_only() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"))
            .with_transfer_conversation(false);

        let long_context = "A very long conversation context that would be expensive to transfer fully. \
                           The main agent analyzed requirements, found patterns, and identified key components. \
                           This is a summary of what happened in the previous agent's execution.";

        let result = manager.execute_handoff(
            "main", "coding", "Implementation needed",
            long_context, &config, 0
        ).await.unwrap();

        assert!(!result.conversation_transferred);
    }

    #[tokio::test]
    async fn test_handoff_manager_logging() {
        let log = Arc::new(InMemoryHandoffLog::new());
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::with_components(
            factory,
            TokenBudget::new(50_000),
            log.clone(),
        );

        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"))
            .with_transfer_conversation(true);

        let _result = manager.execute_handoff(
            "main", "coding", "Needs implementation",
            "Context info", &config, 0
        ).await.unwrap();

        assert_eq!(log.event_count().await, 1);

        let events = log.events_for_agent("coding").await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].from_agent, "main");
        assert_eq!(events[0].to_agent, "coding");
    }

    #[test]
    fn test_can_handoff_at_depth() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffConfig::new().with_max_depth(3);
        assert!(manager.can_handoff_at_depth(&config, 0));
        assert!(manager.can_handoff_at_depth(&config, 1));
        assert!(manager.can_handoff_at_depth(&config, 2));
        assert!(!manager.can_handoff_at_depth(&config, 3));
    }

    #[test]
    fn test_remaining_depth() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffConfig::new().with_max_depth(3);
        assert_eq!(manager.remaining_depth(&config, 0), 3);
        assert_eq!(manager.remaining_depth(&config, 1), 2);
        assert_eq!(manager.remaining_depth(&config, 2), 1);
        assert_eq!(manager.remaining_depth(&config, 3), 0);
    }

    #[test]
    fn test_extract_target_from_response() {
        // `extract_target_from_response` is an associated function, so no manager
        // instance is needed — only the config is exercised here.
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"))
            .with_target(HandoffTarget::new("research", "Research"));

        // Exact match
        assert_eq!(HandoffManager::extract_target_from_response("coding", &config), Some("coding".to_string()));

        // Case insensitive
        assert_eq!(HandoffManager::extract_target_from_response("CODING", &config), Some("coding".to_string()));

        // Substring match
        assert_eq!(
            HandoffManager::extract_target_from_response("I think coding should handle this", &config),
            Some("coding".to_string())
        );

        // Unknown
        assert_eq!(HandoffManager::extract_target_from_response("unknown", &config), None);
    }

    #[test]
    fn test_build_handoff_task_with_context() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let target = HandoffTarget::new("coding", "Code implementation");
        let config = HandoffConfig::new().with_transfer_conversation(true);

        let task = manager.build_handoff_task(
            &target, "Needs code", "Previous context here",
            true, 0, &config
        );

        assert!(task.contains("Previous context here"));
        assert!(task.contains("Needs code"));
        assert!(task.contains("handoff"));
    }

    #[test]
    fn test_build_handoff_task_summary_only() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let target = HandoffTarget::new("coding", "Code");
        let config = HandoffConfig::new().with_transfer_conversation(false);

        let task = manager.build_handoff_task(
            &target, "Needs code", "Long context here",
            false, 0, &config
        );

        assert!(task.contains("Summary"));
        assert!(task.contains("Needs code"));
    }

    #[test]
    fn test_build_handoff_task_with_system_prompt() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let target = HandoffTarget::new("coding", "Code")
            .with_system_prompt("Focus on Rust implementation");
        let config = HandoffConfig::new().with_transfer_conversation(true);

        let task = manager.build_handoff_task(
            &target, "Needs code", "", true, 0, &config
        );

        assert!(task.contains("Focus on Rust implementation"));
    }

    #[tokio::test]
    async fn test_presets_development_chain_handoff() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffPresets::development_chain();
        let result = manager.execute_handoff(
            "main", "coding", "Task needs implementation",
            "Requirements identified: add authentication feature",
            &config, 0
        ).await.unwrap();

        assert!(!result.final_answer.is_empty());
        assert!(result.handoff_count > 0);
    }

    #[tokio::test]
    async fn test_presets_research_chain_handoff() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffPresets::research_chain();
        let result = manager.execute_handoff(
            "main", "research", "Need codebase exploration",
            "", &config, 0
        ).await.unwrap();

        assert!(!result.final_answer.is_empty());
    }

    #[tokio::test]
    async fn test_presets_support_routing_handoff() {
        let factory = Arc::new(MockFactory);
        let manager = HandoffManager::new(factory);

        let config = HandoffPresets::support_routing();
        let result = manager.execute_handoff(
            "triage", "coding_specialist", "Debugging issue",
            "User reports error in authentication module",
            &config, 0
        ).await.unwrap();

        assert!(!result.final_answer.is_empty());
        assert!(!result.conversation_transferred); // Summary-only
    }
}
