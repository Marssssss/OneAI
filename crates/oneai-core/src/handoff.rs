//! Agent Handoff Protocol — enables agents to transfer conversation control.
//!
//! OneAI's SubAgent system provides hierarchical delegation (main → sub-agent → summary).
//! The handoff protocol extends this with seamless control transfer (OpenAI SDK-style
//! Handoff-as-tool-call):
//!
//! - The model naturally decides when to hand off (no manual orchestration)
//! - Conversation context transfers seamlessly (not summary-only)
//! - The receiving agent can see the full history and continue naturally
//! - Handoff depth tracking prevents infinite handoff loops
//!
//! Key innovation: **Handoff as a Tool**. The HandoffTool is registered as a regular
//! tool in the ToolRegistry, so the model can "call" it just like any other tool.
//! When called, the AgentLoop detects it's a handoff (not a regular tool), packages
//! the conversation context, creates the receiving agent, and continues the loop.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::team::SubAgentKindProxy;
use crate::error::Result;

// ─── HandoffConfig ─────────────────────────────────────────────────────────────

/// Agent handoff configuration — defines how agents can transfer control.
///
/// Inspired by OpenAI SDK's Handoff-as-tool-call pattern where the model
/// decides when to hand off by "calling" the handoff tool. The tool's
/// description tells the model when each handoff target is appropriate,
/// enabling natural, model-driven orchestration.
///
/// Configuration controls:
/// - Available handoff targets (agents that can receive control)
/// - Whether conversation history transfers (vs summary-only)
/// - Maximum handoff chain depth (prevents infinite loops)
/// - Whether a handoff message is added to the conversation
///
/// **Usage**:
/// ```ignore
/// let config = HandoffConfig::new()
///     .with_target(HandoffTarget::new("coding", "Code implementation agent")
///         .with_agent_kind(SubAgentKindProxy::code())
///         .with_can_handoff(true))
///     .with_target(HandoffTarget::new("research", "Research agent")
///         .with_agent_kind(SubAgentKindProxy::explore())
///         .with_can_handoff(false))
///     .with_transfer_conversation(true)
///     .with_max_depth(3);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HandoffConfig {
    /// Available handoff targets — agents that can receive a handoff.
    pub targets: Vec<HandoffTarget>,

    /// Whether the conversation history is transferred (vs summary-only).
    /// If true, the receiving agent gets the full conversation history.
    /// If false, only a summary is passed (less context, but cheaper).
    pub transfer_conversation: bool,

    /// Maximum handoff chain depth (prevents infinite handoff loops).
    /// Each handoff increments the depth counter. When max_depth is reached,
    /// no further handoffs are allowed — the current agent must produce a
    /// final answer.
    pub max_depth: usize,

    /// Whether to add a handoff message to the conversation when a handoff occurs.
    /// The message includes: from_agent, to_agent, and reason.
    /// This helps the receiving agent understand why it was handed off to.
    pub add_handoff_message: bool,

    /// Handoff tool name in the ToolRegistry.
    /// Default: "handoff". Can be customized if needed.
    pub tool_name: String,
}

impl HandoffConfig {
    /// Create a new handoff config with empty targets.
    pub fn new() -> Self {
        Self {
            targets: Vec::new(),
            transfer_conversation: true,
            max_depth: 3,
            add_handoff_message: true,
            tool_name: "handoff".to_string(),
        }
    }

    /// Add a handoff target.
    pub fn with_target(mut self, target: HandoffTarget) -> Self {
        self.targets.push(target);
        self
    }

    /// Set whether conversation history is transferred.
    pub fn with_transfer_conversation(mut self, transfer: bool) -> Self {
        self.transfer_conversation = transfer;
        self
    }

    /// Set maximum handoff chain depth.
    pub fn with_max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// Set whether to add a handoff message.
    pub fn with_add_handoff_message(mut self, add: bool) -> Self {
        self.add_handoff_message = add;
        self
    }

    /// Set the handoff tool name.
    pub fn with_tool_name(mut self, name: &str) -> Self {
        self.tool_name = name.to_string();
        self
    }

    /// Validate the handoff configuration.
    ///
    /// Checks:
    /// - At least 1 target is defined (if targets are expected)
    /// - max_depth > 0
    /// - Target names are unique
    /// - At least 1 target has can_handoff=true if max_depth > 1
    pub fn validate(&self) -> Result<()> {
        if self.max_depth == 0 {
            return Err(crate::error::OneAIError::Handoff(
                "max_depth must be > 0".to_string()
            ));
        }

        // Check unique target names
        let mut names = HashMap::new();
        for target in &self.targets {
            if let Some(_prev) = names.insert(&target.agent_name, target) {
                return Err(crate::error::OneAIError::Handoff(
                    format!("Duplicate handoff target name '{}'", target.agent_name)
                ));
            }
        }

        // If max_depth > 1, at least one target should allow further handoffs
        if self.max_depth > 1 {
            let can_handoff_count = self.targets.iter().filter(|t| t.can_handoff).count();
            if can_handoff_count == 0 && self.targets.len() > 1 {
                return Err(crate::error::OneAIError::Handoff(
                    "With max_depth > 1 and multiple targets, at least one target should have can_handoff=true".to_string()
                ));
            }
        }

        Ok(())
    }

    /// Get a target by name.
    pub fn target_by_name(&self, name: &str) -> Option<&HandoffTarget> {
        self.targets.iter().find(|t| t.agent_name == name)
    }

    /// Get all target names.
    pub fn target_names(&self) -> Vec<String> {
        self.targets.iter().map(|t| t.agent_name.clone()).collect()
    }

    /// Get the number of targets.
    pub fn target_count(&self) -> usize {
        self.targets.len()
    }

    /// Build the tool description for the handoff tool.
    ///
    /// This description is shown to the model when the handoff tool
    /// is listed as an available tool. It tells the model when each
    /// target is appropriate, enabling natural handoff decisions.
    ///
    /// Example output:
    /// "Transfer control to another agent. Available targets:
    ///  - 'coding': Hand off when the task requires code implementation
    ///  - 'research': Hand off when the task requires deep research
    ///  - 'review': Hand off when the task requires review/audit"
    pub fn tool_description(&self) -> String {
        let mut desc = "Transfer control to another specialized agent. You should hand off when the task requires expertise that another agent has. Available targets:\n".to_string();
        for target in &self.targets {
            desc.push_str(&format!("  - '{}': {}\n", target.agent_name, target.description));
        }
        desc.push_str("\nCall this tool with the target agent name and your reason for handing off.");
        desc
    }

    /// Build the parameters schema for the handoff tool.
    ///
    /// The schema defines two parameters:
    /// - "target": The agent to hand off to (must match a target name)
    /// - "reason": Why you're handing off (model's decision rationale)
    pub fn tool_parameters_schema(&self) -> serde_json::Value {
        let target_enum_values = self.targets.iter()
            .map(|t| serde_json::Value::String(t.agent_name.clone()))
            .collect::<Vec<_>>();

        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "The agent to hand off to",
                    "enum": target_enum_values
                },
                "reason": {
                    "type": "string",
                    "description": "Why you're handing off — your rationale for the transfer"
                }
            },
            "required": ["target", "reason"]
        })
    }
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self::new()
    }
}

// ─── HandoffTarget ─────────────────────────────────────────────────────────────

/// A handoff target — an agent that can receive control.
///
/// Each target has:
/// - A unique name (matches a registered agent kind)
/// - A description (tells the model when this target is appropriate)
/// - A sub-agent kind (determines how the receiving agent is created)
/// - Whether it can itself hand off (enables multi-step handoff chains)
///
/// The target's description is critical — it's shown to the model as part
/// of the handoff tool's description, helping the model decide when to
/// hand off to this target. Good descriptions are specific about what
/// tasks the target handles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffTarget {
    /// Target agent name (matches a registered agent).
    /// Must be unique within a HandoffConfig.
    pub agent_name: String,

    /// Brief description of when to hand off to this agent.
    /// Shown to the model as part of the handoff tool description.
    /// Should be specific: "Hand off when the task requires code implementation"
    /// rather than generic: "Hand off for coding tasks".
    pub description: String,

    /// The sub-agent kind for this target.
    /// The SubAgentFactory creates the receiving agent from this kind.
    #[serde(skip)]
    pub agent_kind: SubAgentKindProxy,

    /// Whether this target can itself hand off to others.
    /// If true, the receiving agent can also call the handoff tool.
    /// If false, the receiving agent must produce a final answer.
    /// This enables multi-step handoff chains while preventing infinite loops.
    pub can_handoff: bool,

    /// System prompt override for this handoff target.
    /// If Some, replaces the receiving agent's default system prompt.
    /// Useful for specializing the handoff behavior (e.g., "You received a
    /// handoff from the research agent. Continue the investigation.")
    pub system_prompt_override: Option<String>,
}

impl HandoffTarget {
    /// Create a new handoff target with a name and description.
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            agent_name: name.to_string(),
            description: description.to_string(),
            agent_kind: SubAgentKindProxy::custom(name),
            can_handoff: false,
            system_prompt_override: None,
        }
    }

    /// Create a handoff target with a specific agent kind.
    pub fn with_kind(name: &str, description: &str, kind: SubAgentKindProxy) -> Self {
        Self {
            agent_name: name.to_string(),
            description: description.to_string(),
            agent_kind: kind,
            can_handoff: false,
            system_prompt_override: None,
        }
    }

    /// Set the agent kind.
    pub fn with_agent_kind(mut self, kind: SubAgentKindProxy) -> Self {
        self.agent_kind = kind;
        self
    }

    /// Set whether this target can hand off further.
    pub fn with_can_handoff(mut self, can: bool) -> Self {
        self.can_handoff = can;
        self
    }

    /// Set a system prompt override.
    pub fn with_system_prompt(mut self, prompt: &str) -> Self {
        self.system_prompt_override = Some(prompt.to_string());
        self
    }
}

// ─── HandoffEvent ──────────────────────────────────────────────────────────────

/// Handoff event — recorded when a handoff occurs.
///
/// Each handoff produces an event that includes:
/// - Which agent initiated the handoff (from_agent)
/// - Which agent received it (to_agent)
/// - The model's reason for the handoff
/// - Whether conversation history was transferred
/// - The timestamp
///
/// Events are stored in the HandoffLog for observability and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffEvent {
    /// The agent that initiated the handoff.
    pub from_agent: String,

    /// The agent receiving the handoff.
    pub to_agent: String,

    /// The reason for the handoff (from the model's decision).
    pub reason: String,

    /// Whether conversation history was transferred.
    pub conversation_transferred: bool,

    /// Current handoff chain depth (how many handoffs have occurred).
    pub depth: usize,

    /// Timestamp of the handoff.
    pub timestamp: DateTime<Utc>,
}

// ─── HandoffResult ──────────────────────────────────────────────────────────────

/// Result of a handoff execution.
///
/// Contains the final answer from the handoff chain (the last agent's
/// output), the handoff chain history, and aggregate statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffResult {
    /// The final answer from the handoff chain.
    /// This is the output of the last agent in the chain.
    pub final_answer: String,

    /// The handoff chain — ordered list of agents that handled the task.
    pub chain: Vec<HandoffChainEntry>,

    /// Total tokens used across the handoff chain.
    pub total_tokens: u32,

    /// Total cost across the handoff chain.
    pub total_cost: f64,

    /// Whether conversation history was transferred at each handoff.
    pub conversation_transferred: bool,

    /// The number of handoffs in the chain.
    pub handoff_count: usize,
}

impl HandoffResult {
    /// Create an empty handoff result (no handoffs occurred).
    pub fn empty() -> Self {
        Self {
            final_answer: String::new(),
            chain: Vec::new(),
            total_tokens: 0,
            total_cost: 0.0,
            conversation_transferred: false,
            handoff_count: 0,
        }
    }

    /// Whether any handoffs occurred.
    pub fn has_handoffs(&self) -> bool {
        self.handoff_count > 0
    }

    /// Get the handoff chain as a formatted string.
    pub fn chain_description(&self) -> String {
        if self.chain.is_empty() {
            return "No handoffs".to_string();
        }
        self.chain.iter()
            .map(|e| format!("{} → {} (reason: {})", e.from_agent, e.to_agent, e.reason))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ─── HandoffChainEntry ─────────────────────────────────────────────────────────

/// An entry in the handoff chain — records one handoff step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffChainEntry {
    /// The agent that initiated the handoff.
    pub from_agent: String,

    /// The agent that received the handoff.
    pub to_agent: String,

    /// The reason for this handoff.
    pub reason: String,

    /// The agent's output before handoff (partial result).
    pub partial_output: String,

    /// Tokens used by the from_agent before handoff.
    pub tokens_used: u32,

    /// Cost incurred by the from_agent.
    pub cost: f64,
}

// ─── HandoffLog trait ──────────────────────────────────────────────────────────

/// Log trait for handoff events.
///
/// Records handoff events for observability and debugging:
/// - When a handoff occurs
/// - The from/to agents and reason
/// - Whether conversation context was transferred
/// - Handoff depth tracking
///
/// Implementations can persist logs to memory, SQLite, or external services.
#[async_trait]
pub trait HandoffLog: Send + Sync {
    /// Log a handoff event.
    async fn log_handoff(&self, event: HandoffEvent);

    /// Get recent handoff events.
    async fn recent_events(&self, limit: usize) -> Vec<HandoffEvent>;

    /// Get events for a specific agent (as either from or to).
    async fn events_for_agent(&self, agent_name: &str) -> Vec<HandoffEvent>;

    /// Get the total number of handoff events.
    async fn event_count(&self) -> usize;
}

// ─── InMemoryHandoffLog ────────────────────────────────────────────────────────

/// In-memory implementation of HandoffLog.
///
/// Stores events in a Vec protected by a RwLock.
/// Suitable for testing and single-session scenarios.
/// Not suitable for production persistence (use SqliteHandoffLog).
pub struct InMemoryHandoffLog {
    events: Arc<tokio::sync::RwLock<Vec<HandoffEvent>>>,
}

impl InMemoryHandoffLog {
    /// Create a new in-memory log.
    pub fn new() -> Self {
        Self {
            events: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }
}

impl Default for InMemoryHandoffLog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HandoffLog for InMemoryHandoffLog {
    async fn log_handoff(&self, event: HandoffEvent) {
        let mut events = self.events.write().await;
        events.push(event);
    }

    async fn recent_events(&self, limit: usize) -> Vec<HandoffEvent> {
        let events = self.events.read().await;
        events.iter().rev().take(limit).cloned().collect()
    }

    async fn events_for_agent(&self, agent_name: &str) -> Vec<HandoffEvent> {
        let events = self.events.read().await;
        events.iter()
            .filter(|e| e.from_agent == agent_name || e.to_agent == agent_name)
            .cloned()
            .collect()
    }

    async fn event_count(&self) -> usize {
        self.events.read().await.len()
    }
}

// ─── HandoffPresets ────────────────────────────────────────────────────────────

/// Preset handoff configurations for common use cases.
///
/// These presets provide ready-to-use handoff configurations for
/// typical agent delegation scenarios. Each preset comes with appropriate
/// targets, depth limits, and conversation transfer settings.
pub struct HandoffPresets;

impl HandoffPresets {
    /// Development handoff — main → coding → review chain.
    ///
    /// The main agent can hand off to coding for implementation,
    /// and coding can hand off to review for quality checking.
    /// Max depth: 3 (main → coding → review → back to main).
    pub fn development_chain() -> HandoffConfig {
        HandoffConfig::new()
            .with_target(HandoffTarget::with_kind(
                "coding",
                "Hand off when the task requires code implementation or modification",
                SubAgentKindProxy::code(),
            ).with_can_handoff(true)
                .with_system_prompt("You are a code implementation agent. You can hand off to the review agent when you've finished implementing and need quality verification."))
            .with_target(HandoffTarget::with_kind(
                "review",
                "Hand off when the task requires code review, audit, or quality verification",
                SubAgentKindProxy::review(),
            ).with_can_handoff(false)
                .with_system_prompt("You are a review agent. You received a handoff and must produce a final review assessment."))
            .with_target(HandoffTarget::with_kind(
                "research",
                "Hand off when the task requires research, exploration, or information gathering",
                SubAgentKindProxy::explore(),
            ).with_can_handoff(true)
                .with_system_prompt("You are a research agent. You can hand off to the coding agent once you've gathered enough context for implementation."))
            .with_transfer_conversation(true)
            .with_max_depth(3)
    }

    /// Research handoff — main → research → analysis.
    ///
    /// Research agent can hand off to analysis for deep interpretation.
    /// Max depth: 2 (main → research or main → analysis).
    pub fn research_chain() -> HandoffConfig {
        HandoffConfig::new()
            .with_target(HandoffTarget::with_kind(
                "research",
                "Hand off when the task requires web research, codebase exploration, or data gathering",
                SubAgentKindProxy::explore(),
            ).with_can_handoff(true)
                .with_system_prompt("You are a research agent. Gather information and hand off to the analysis agent for interpretation when you have sufficient data."))
            .with_target(HandoffTarget::with_kind(
                "analysis",
                "Hand off when the task requires deep analysis, synthesis, or interpretation of research results",
                SubAgentKindProxy::plan(),
            ).with_can_handoff(false)
                .with_system_prompt("You are an analysis agent. Produce a final analytical summary based on the research data provided."))
            .with_transfer_conversation(true)
            .with_max_depth(2)
    }

    /// Support handoff — triage → specialist routing.
    ///
    /// Triage agent routes to the appropriate specialist.
    /// No further handoffs (specialist must produce final answer).
    pub fn support_routing() -> HandoffConfig {
        HandoffConfig::new()
            .with_target(HandoffTarget::with_kind(
                "coding_specialist",
                "Hand off when the issue is about code implementation, debugging, or technical development",
                SubAgentKindProxy::code(),
            ).with_can_handoff(false))
            .with_target(HandoffTarget::with_kind(
                "research_specialist",
                "Hand off when the issue requires research, documentation lookup, or information gathering",
                SubAgentKindProxy::explore(),
            ).with_can_handoff(false))
            .with_target(HandoffTarget::with_kind(
                "review_specialist",
                "Hand off when the issue requires review, quality assessment, or security audit",
                SubAgentKindProxy::review(),
            ).with_can_handoff(false))
            .with_transfer_conversation(false) // Summary-only for support (cheaper)
            .with_max_depth(1)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handoff_config_creation() {
        let config = HandoffConfig::new();
        assert!(config.targets.is_empty());
        assert!(config.transfer_conversation);
        assert_eq!(config.max_depth, 3);
        assert!(config.add_handoff_message);
        assert_eq!(config.tool_name, "handoff");
    }

    #[test]
    fn test_handoff_config_with_target() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code implementation"));
        assert_eq!(config.target_count(), 1);
        assert_eq!(config.target_names(), vec!["coding"]);
    }

    #[test]
    fn test_handoff_config_validate_empty() {
        let config = HandoffConfig::new();
        // Empty targets are valid — just means no handoffs available
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_handoff_config_validate_zero_depth() {
        let config = HandoffConfig::new()
            .with_max_depth(0)
            .with_target(HandoffTarget::new("coding", "Code"));
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max_depth must be > 0"));
    }

    #[test]
    fn test_handoff_config_validate_duplicate_names() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "First"))
            .with_target(HandoffTarget::new("coding", "Second"));
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Duplicate handoff target name"));
    }

    #[test]
    fn test_handoff_config_validate_depth_no_handoff() {
        let config = HandoffConfig::new()
            .with_max_depth(2)
            .with_target(HandoffTarget::new("a", "A").with_can_handoff(false))
            .with_target(HandoffTarget::new("b", "B").with_can_handoff(false));
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("can_handoff=true"));
    }

    #[test]
    fn test_handoff_config_validate_valid() {
        let config = HandoffPresets::development_chain();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_handoff_target_creation() {
        let target = HandoffTarget::new("coding", "Code implementation agent");
        assert_eq!(target.agent_name, "coding");
        assert_eq!(target.description, "Code implementation agent");
        assert!(!target.can_handoff);
        assert!(target.system_prompt_override.is_none());
    }

    #[test]
    fn test_handoff_target_with_kind() {
        let target = HandoffTarget::with_kind(
            "review", "Review agent", SubAgentKindProxy::review()
        );
        assert_eq!(target.agent_kind.name(), "review");
    }

    #[test]
    fn test_handoff_target_with_can_handoff() {
        let target = HandoffTarget::new("coding", "Code")
            .with_can_handoff(true);
        assert!(target.can_handoff);
    }

    #[test]
    fn test_handoff_target_with_system_prompt() {
        let target = HandoffTarget::new("coding", "Code")
            .with_system_prompt("You are a coding agent that received a handoff.");
        assert!(target.system_prompt_override.is_some());
        assert!(target.system_prompt_override.unwrap().contains("handoff"));
    }

    #[test]
    fn test_handoff_config_target_by_name() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"))
            .with_target(HandoffTarget::new("research", "Research"));

        assert!(config.target_by_name("coding").is_some());
        assert!(config.target_by_name("research").is_some());
        assert!(config.target_by_name("unknown").is_none());
    }

    #[test]
    fn test_handoff_config_tool_description() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code implementation"))
            .with_target(HandoffTarget::new("research", "Deep research"));

        let desc = config.tool_description();
        assert!(desc.contains("coding"));
        assert!(desc.contains("research"));
        assert!(desc.contains("Code implementation"));
        assert!(desc.contains("Deep research"));
        assert!(desc.contains("Transfer control"));
    }

    #[test]
    fn test_handoff_config_tool_parameters_schema() {
        let config = HandoffConfig::new()
            .with_target(HandoffTarget::new("coding", "Code"))
            .with_target(HandoffTarget::new("research", "Research"));

        let schema = config.tool_parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["target"]["enum"].is_array());
        assert!(schema["properties"]["reason"]["type"] == "string");
        assert!(schema["required"].is_array());
    }

    #[test]
    fn test_handoff_event_creation() {
        let event = HandoffEvent {
            from_agent: "main".to_string(),
            to_agent: "coding".to_string(),
            reason: "Task requires code implementation".to_string(),
            conversation_transferred: true,
            depth: 1,
            timestamp: Utc::now(),
        };
        assert_eq!(event.from_agent, "main");
        assert_eq!(event.to_agent, "coding");
        assert_eq!(event.depth, 1);
    }

    #[test]
    fn test_handoff_event_serialization() {
        let event = HandoffEvent {
            from_agent: "main".to_string(),
            to_agent: "coding".to_string(),
            reason: "Code task".to_string(),
            conversation_transferred: true,
            depth: 1,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: HandoffEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.from_agent, parsed.from_agent);
        assert_eq!(event.depth, parsed.depth);
    }

    #[test]
    fn test_handoff_result_empty() {
        let result = HandoffResult::empty();
        assert!(result.final_answer.is_empty());
        assert!(result.chain.is_empty());
        assert!(!result.has_handoffs());
        assert_eq!(result.chain_description(), "No handoffs");
    }

    #[test]
    fn test_handoff_result_serialization() {
        let result = HandoffResult {
            final_answer: "Implemented feature X".into(),
            chain: vec![HandoffChainEntry {
                from_agent: "main".into(),
                to_agent: "coding".into(),
                reason: "Needs code implementation".into(),
                partial_output: "Analyzing requirements...".into(),
                tokens_used: 5000,
                cost: 0.05,
            }],
            total_tokens: 5000,
            total_cost: 0.05,
            conversation_transferred: true,
            handoff_count: 1,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: HandoffResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.final_answer, parsed.final_answer);
        assert_eq!(result.handoff_count, parsed.handoff_count);
    }

    #[test]
    fn test_handoff_chain_entry() {
        let entry = HandoffChainEntry {
            from_agent: "main".into(),
            to_agent: "research".into(),
            reason: "Needs exploration".into(),
            partial_output: "Starting research...".into(),
            tokens_used: 3000,
            cost: 0.03,
        };
        assert_eq!(entry.from_agent, "main");
        assert_eq!(entry.to_agent, "research");
    }

    #[test]
    fn test_handoff_result_chain_description() {
        let result = HandoffResult {
            final_answer: "Done".into(),
            chain: vec![
                HandoffChainEntry {
                    from_agent: "main".into(),
                    to_agent: "research".into(),
                    reason: "Research needed".into(),
                    partial_output: "".into(),
                    tokens_used: 3000,
                    cost: 0.03,
                },
                HandoffChainEntry {
                    from_agent: "research".into(),
                    to_agent: "coding".into(),
                    reason: "Ready to implement".into(),
                    partial_output: "Found 3 files".into(),
                    tokens_used: 5000,
                    cost: 0.05,
                },
            ],
            total_tokens: 8000,
            total_cost: 0.08,
            conversation_transferred: true,
            handoff_count: 2,
        };
        let desc = result.chain_description();
        assert!(desc.contains("main → research"));
        assert!(desc.contains("research → coding"));
    }

    #[tokio::test]
    async fn test_in_memory_handoff_log() {
        let log = InMemoryHandoffLog::new();

        log.log_handoff(HandoffEvent {
            from_agent: "main".to_string(),
            to_agent: "coding".to_string(),
            reason: "Code task".to_string(),
            conversation_transferred: true,
            depth: 1,
            timestamp: Utc::now(),
        }).await;

        log.log_handoff(HandoffEvent {
            from_agent: "coding".to_string(),
            to_agent: "review".to_string(),
            reason: "Review needed".to_string(),
            conversation_transferred: true,
            depth: 2,
            timestamp: Utc::now(),
        }).await;

        assert_eq!(log.event_count().await, 2);

        let events = log.recent_events(1).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].from_agent, "coding"); // Most recent

        let coding_events = log.events_for_agent("coding").await;
        assert_eq!(coding_events.len(), 2); // As both from and to
    }

    #[test]
    fn test_presets_development_chain() {
        let config = HandoffPresets::development_chain();
        assert_eq!(config.target_count(), 3);
        assert!(config.transfer_conversation);
        assert_eq!(config.max_depth, 3);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_presets_research_chain() {
        let config = HandoffPresets::research_chain();
        assert_eq!(config.target_count(), 2);
        assert!(config.transfer_conversation);
        assert_eq!(config.max_depth, 2);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_presets_support_routing() {
        let config = HandoffPresets::support_routing();
        assert_eq!(config.target_count(), 3);
        assert!(!config.transfer_conversation); // Summary-only for support
        assert_eq!(config.max_depth, 1);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_handoff_config_defaults() {
        let config = HandoffConfig::default();
        assert!(config.targets.is_empty());
        assert!(config.transfer_conversation);
        assert_eq!(config.max_depth, 3);
    }
}
