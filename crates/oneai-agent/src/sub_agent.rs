//! Sub-agent delegation system — hierarchical task decomposition.
//!
//! The sub-agent system is the core mechanism for hierarchical delegation:
//! the main agent can delegate complex subtasks to specialized sub-agents
//! (Plan, Explore, Code, Review, etc.), each running with its own
//! context window and token budget.
//!
//! Key principle: sub-agents return only a **summary** to the main agent,
//! not their full conversation. This keeps the main agent's context window clean
//! and allows complex tasks to be decomposed without context pollution.
//!
//! This addresses Issue #7: the need for hierarchical delegation where
//! the main agent can spawn specialized sub-agents for different aspects
//! of a complex task.

use std::sync::Arc;
use async_trait::async_trait;

use oneai_core::error::Result;
use oneai_core::budget::TokenBudget;
use oneai_core::traits::{LlmProvider, OutputParser, ApprovalGate, Tool};

// ─── SubAgentKind ───────────────────────────────────────────────────────────

/// The type of sub-agent to spawn for a delegated task.
///
/// Each kind maps to a specialized agent with different capabilities:
/// - Plan: Task decomposition into ordered steps
/// - Explore: Search and understand the codebase/environment
/// - Code: Code implementation and modification
/// - Review: Review and audit existing work
/// - Custom: User-defined sub-agent types
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SubAgentKind {
    /// Plan agent — decomposes complex tasks into ordered steps.
    /// Returns a structured plan (list of steps with dependencies).
    Plan,

    /// Explore agent — searches and understands the codebase/environment.
    /// Returns a comprehensive summary of findings.
    Explore,

    /// Code agent — implements and modifies code.
    /// Returns a summary of changes made.
    Code,

    /// Review agent — reviews and audits existing work.
    /// Returns a structured review with findings and suggestions.
    Review,

    /// Custom sub-agent type (user-defined).
    /// The string identifier maps to a registered sub-agent factory method.
    Custom(String),
}

impl SubAgentKind {
    /// Get a human-readable name for this sub-agent kind.
    pub fn name(&self) -> &str {
        match self {
            Self::Plan => "plan",
            Self::Explore => "explore",
            Self::Code => "code",
            Self::Review => "review",
            Self::Custom(name) => name,
        }
    }

    /// Parse a string into a SubAgentKind.
    /// Unknown strings are mapped to Custom.
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "plan" => Self::Plan,
            "explore" => Self::Explore,
            "code" => Self::Code,
            "review" => Self::Review,
            other => Self::Custom(other.to_string()),
        }
    }
}

// ─── SubAgentSummary ────────────────────────────────────────────────────────

/// The summary returned by a sub-agent to the main agent.
///
/// This is the only data that flows back from the sub-agent to the main agent.
/// The sub-agent's full conversation is NOT included — only the summary and
/// key findings are passed back, keeping the main agent's context window clean.
///
/// This is inspired by Claude Code's agent delegation pattern where
/// sub-agents return their final text as the return value, not the
/// full conversation transcript.
#[derive(Debug, Clone)]
pub struct SubAgentSummary {
    /// Whether the sub-agent completed its task successfully.
    pub completed: bool,

    /// A concise summary of the sub-agent's result.
    /// This is NOT the full output — it's a distilled summary
    /// that captures the essential information the main agent needs.
    pub summary: String,

    /// Key findings or data from the sub-agent.
    /// These are the most important pieces of information extracted
    /// from the sub-agent's work (e.g., file paths, function names,
    /// error messages, test results).
    pub key_findings: Vec<String>,

    /// Whether the sub-agent exceeded its token budget.
    /// If true, the main agent should consider whether to allocate
    /// more budget or adjust its approach.
    pub budget_exceeded: bool,

    /// The sub-agent kind that produced this summary.
    pub agent_kind: SubAgentKind,

    /// Token usage by the sub-agent (for budget tracking).
    pub tokens_used: u32,
}

// ─── SubAgent trait ─────────────────────────────────────────────────────────

/// Sub-agent trait — the interface for all specialized sub-agents.
///
/// Each sub-agent implementation:
/// 1. Receives a task description and token budget
/// 2. Runs independently with its own context window
/// 3. Returns only a SubAgentSummary (not its full conversation)
///
/// The main agent never sees the sub-agent's intermediate steps,
/// only the final summary. This enables deep task decomposition
/// without context pollution.
#[async_trait]
pub trait SubAgent: Send + Sync {
    /// Run the sub-agent on a task.
    ///
    /// The task description should be specific and actionable.
    /// The sub-agent uses its own context window and the provided budget.
    /// After completion, only the summary is returned.
    async fn run(&self, task: &str) -> Result<SubAgentSummary>;

    /// Get the kind of this sub-agent.
    fn kind(&self) -> &SubAgentKind;

    /// Get the token budget allocated to this sub-agent.
    fn budget(&self) -> &TokenBudget;
}

// ─── SubAgentFactory trait ──────────────────────────────────────────────────

/// Factory for creating sub-agents of different kinds.
///
/// The factory pattern allows the main agent to dynamically spawn
/// specialized sub-agents based on the task requirements.
/// Each kind of sub-agent may have different configurations,
/// tools, and system prompts.
///
/// The factory is provided to the AgentLoop at construction time,
/// allowing the loop to delegate tasks without knowing the
/// specific sub-agent implementations.
pub trait SubAgentFactory: Send + Sync {
    /// Create a sub-agent of the specified kind with the given budget.
    ///
    /// The factory selects the appropriate configuration, tools,
    /// and system prompt for the requested sub-agent kind.
    fn create(&self, kind: SubAgentKind, budget: TokenBudget) -> Result<Box<dyn SubAgent>>;

    /// List the available sub-agent kinds.
    fn available_kinds(&self) -> Vec<SubAgentKind>;

    /// Check if a specific sub-agent kind is available.
    fn is_available(&self, kind: &SubAgentKind) -> bool;
}

// ─── DefaultSubAgentFactory ─────────────────────────────────────────────────

/// Default sub-agent factory that creates standard agent types.
///
/// This factory uses the existing PlanAgent, ReActAgent, ReflectionAgent
/// as sub-agent implementations, wrapping them with the SubAgent trait.
pub struct DefaultSubAgentFactory {
    provider: Arc<dyn LlmProvider>,
    parser: Arc<dyn OutputParser>,
    approval_gate: Arc<dyn ApprovalGate>,
    tools: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<dyn Tool>>>>,
}

impl DefaultSubAgentFactory {
    /// Create a new default factory with the given dependencies.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        parser: Arc<dyn OutputParser>,
        approval_gate: Arc<dyn ApprovalGate>,
        tools: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<dyn Tool>>>>,
    ) -> Self {
        Self { provider, parser, approval_gate, tools }
    }
}

impl SubAgentFactory for DefaultSubAgentFactory {
    fn create(&self, kind: SubAgentKind, budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
        // Implementation: create appropriate agent based on kind
        // Plan → PlanSubAgent wrapper
        // Explore → ReActSubAgent with exploration-focused config
        // Code → ReActSubAgent with code-focused config
        // Review → ReflectionSubAgent wrapper
        // Custom → lookup in registry
        todo!("Implementation in full code phase")
    }

    fn available_kinds(&self) -> Vec<SubAgentKind> {
        vec![SubAgentKind::Plan, SubAgentKind::Explore, SubAgentKind::Code, SubAgentKind::Review]
    }

    fn is_available(&self, kind: &SubAgentKind) -> bool {
        matches!(kind, SubAgentKind::Plan | SubAgentKind::Explore | SubAgentKind::Code | SubAgentKind::Review)
    }
}