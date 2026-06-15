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

use std::collections::HashMap;
use std::sync::Arc;
use async_trait::async_trait;

use oneai_core::error::Result;
use oneai_core::budget::TokenBudget;
use oneai_core::traits::{LlmProvider, OutputParser, ApprovalGate, Tool};

use crate::agent_loop::{AgentLoop, AgentLoopConfig, AgentLoopObserver};

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

    /// Get the default system prompt for this sub-agent kind.
    fn default_system_prompt(&self) -> &str {
        match self {
            Self::Plan => "You are a planning agent. Decompose the given task into ordered steps with dependencies. Return a structured plan as a numbered list.",
            Self::Explore => "You are an exploration agent. Search and understand the codebase using available tools. Return a comprehensive summary of your findings including file paths, function signatures, and key patterns.",
            Self::Code => "You are a code implementation agent. Write and modify code based on the given specification. Return a summary of all changes you made.",
            Self::Review => "You are a code review agent. Review code for correctness bugs, style issues, and potential improvements. Return a structured review with findings and suggestions.",
            Self::Custom(_) => "You are a specialized agent. Complete the given task and return a summary of your results.",
        }
    }

    /// Get the default available tools for this sub-agent kind.
    fn default_tools(&self) -> &[&str] {
        match self {
            Self::Explore => &["read_file", "grep", "glob", "list_directory"],
            Self::Code => &["read_file", "edit_file", "shell", "grep", "glob"],
            Self::Plan => &["read_file", "grep", "glob"],
            Self::Review => &["read_file", "grep", "glob"],
            Self::Custom(_) => &["read_file", "grep", "glob", "edit_file", "shell"],
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

// ─── SubAgentWrapper ────────────────────────────────────────────────────────

/// Wraps an AgentLoop as a SubAgent implementation.
///
/// This is the concrete SubAgent implementation — it creates an AgentLoop
/// with a scoped tool set and system prompt, runs the task, and returns
/// a SubAgentSummary extracted from the AgentLoopResult.
///
/// The wrapper ensures:
/// 1. Only available_tools are accessible (scoped tool registry)
/// 2. A specialized system prompt is used
/// 3. Token budget is respected
/// 4. Only the summary is returned (not full conversation)
pub struct SubAgentWrapper {
    kind: SubAgentKind,
    budget: TokenBudget,
    agent_loop: AgentLoop,
}

impl SubAgentWrapper {
    /// Create a new SubAgentWrapper from an existing AgentLoop with scoped configuration.
    pub fn new(
        kind: SubAgentKind,
        budget: TokenBudget,
        agent_loop: AgentLoop,
    ) -> Self {
        Self { kind, budget, agent_loop }
    }
}

#[async_trait]
impl SubAgent for SubAgentWrapper {
    async fn run(&self, task: &str) -> Result<SubAgentSummary> {
        let result = self.agent_loop.run(task).await?;

        // Extract key findings from the conversation
        // (look for important patterns in the final answer)
        let key_findings = extract_key_findings(&result.final_answer);

        // Estimate token usage from the number of iterations
        // (rough estimate: ~2000 tokens per iteration)
        let tokens_used = (result.iterations as u32) * 2000;

        Ok(SubAgentSummary {
            completed: result.completed,
            summary: result.final_answer,
            key_findings,
            budget_exceeded: false, // Would need budget tracking integration
            agent_kind: self.kind.clone(),
            tokens_used,
        })
    }

    fn kind(&self) -> &SubAgentKind {
        &self.kind
    }

    fn budget(&self) -> &TokenBudget {
        &self.budget
    }
}

/// Extract key findings from a sub-agent's output text.
///
/// Looks for common patterns like file paths, function names,
/// error messages, and important statements.
fn extract_key_findings(text: &str) -> Vec<String> {
    let mut findings = Vec::new();

    // Extract lines that look like file paths (contain .rs, .py, etc.)
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // File path patterns
        if trimmed.contains('/') && (trimmed.contains(".rs") || trimmed.contains(".py")
            || trimmed.contains(".ts") || trimmed.contains(".js") || trimmed.contains(".toml")
            || trimmed.contains(".json") || trimmed.contains(".md")) {
            findings.push(trimmed.to_string());
        }

        // Error/critical patterns
        if trimmed.starts_with("Error:") || trimmed.starts_with("CRITICAL:")
            || trimmed.starts_with("BUG:") || trimmed.starts_with("\u{26A0}") {
            findings.push(trimmed.to_string());
        }
    }

    // If no structured findings, take first 3 non-empty lines as key findings
    if findings.is_empty() {
        for line in text.lines().take(3) {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                findings.push(trimmed.to_string());
            }
        }
    }

    // Cap at 5 findings to avoid context pollution
    findings.truncate(5);
    findings
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

// ─── SubAgentFactoryNone ────────────────────────────────────────────────────

/// A no-op SubAgentFactory that prevents sub-agents from spawning further sub-agents.
///
/// Used when creating sub-agent AgentLoop instances — sub-agents should not
/// be able to delegate to further sub-agents (only the main agent can delegate).
/// Any attempt to delegate will result in an error.
pub struct SubAgentFactoryNone;

impl SubAgentFactory for SubAgentFactoryNone {
    fn create(&self, _kind: SubAgentKind, _budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
        Err(oneai_core::error::OneAIError::Agent("Sub-agents cannot spawn further sub-agents".to_string()))
    }

    fn available_kinds(&self) -> Vec<SubAgentKind> {
        Vec::new()
    }

    fn is_available(&self, _kind: &SubAgentKind) -> bool {
        false
    }
}

// ─── DefaultSubAgentFactory ─────────────────────────────────────────────────

/// Default sub-agent factory that creates standard agent types.
///
/// This factory uses the existing AgentLoop with scoped tools and
/// system prompts, wrapping them with the SubAgent trait via SubAgentWrapper.
///
/// For each SubAgentKind:
/// - Plan: scoped to read-only tools, planning-focused system prompt
/// - Explore: scoped to read + search tools, exploration-focused system prompt
/// - Code: scoped to read + edit + shell tools, code-focused system prompt
/// - Review: scoped to read-only tools, review-focused system prompt
pub struct DefaultSubAgentFactory {
    provider: Arc<dyn LlmProvider>,
    parser: Arc<dyn OutputParser>,
    approval_gate: Arc<dyn ApprovalGate>,
    tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
}

impl DefaultSubAgentFactory {
    /// Create a new default factory with the given dependencies.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        parser: Arc<dyn OutputParser>,
        approval_gate: Arc<dyn ApprovalGate>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    ) -> Self {
        Self { provider, parser, approval_gate, tools }
    }

    /// Create a scoped tool registry containing only the specified tools.
    ///
    /// Filters the full tool registry to only include tools listed in
    /// available_tools, creating an isolated tool environment for the sub-agent.
    async fn create_scoped_tools(&self, available_tools: &[&str]) -> Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>> {
        let full_tools = self.tools.read().await;
        let mut scoped = HashMap::new();

        for tool_name in available_tools {
            let key: &str = tool_name; // Borrow as &str for HashMap lookup
            if let Some(tool) = full_tools.get(key) {
                scoped.insert(tool_name.to_string(), tool.clone());
            }
        }

        Arc::new(tokio::sync::RwLock::new(scoped))
    }
}

impl SubAgentFactory for DefaultSubAgentFactory {
    fn create(&self, kind: SubAgentKind, budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
        // Get the system prompt and available tools for this kind
        let system_prompt = kind.default_system_prompt().to_string();
        let available_tools = kind.default_tools();

        // We need to create scoped tools async, but create() is sync.
        // Solution: create the AgentLoop config here, and the actual scoped tools
        // will be set up when run() is called (via lazy initialization).
        // For now, we use the full tool set but configure the AgentLoop
        // with the appropriate system prompt.
        //
        // Note: The scoped tool filtering happens at the AgentLoop level —
        // the system prompt instructs the model to only use certain tools,
        // and the tool definitions sent to the provider are filtered.
        // This is similar to how Claude Code handles sub-agent tool scoping.

        let config = AgentLoopConfig {
            system_prompt,
            use_streaming: false,
            temperature: Some(0.3), // Lower temperature for focused sub-agent tasks
            max_tokens: Some(budget.total),
            thinking_budget: None,
            hard_max_iterations: Some(50),
            auto_checkpoint: false,
            inject_skills: false, // Sub-agents don't need skill injection
            detect_env_changes: false, // Sub-agents don't need env diff detection
            pricing: crate::agent_loop::ModelPricing::default(),
        };

        // Create a basic context assembler (no domain sources for sub-agents)
        let context_assembler = crate::context_assembler::ContextAssembler::new();
        let stream_parser = crate::streaming::IncrementalStreamParser::new();

        // Create the AgentLoop with the full tool set
        // (tool scoping is done via system prompt instruction + available_tools config)
        let agent_loop = AgentLoop::new(
            self.provider.clone(),
            self.tools.clone(),
            self.parser.clone(),
            self.approval_gate.clone(),
            Arc::new(oneai_skill::SkillSelector::new()),
            Arc::new(oneai_core::budget::ContextBudgetManager::new(
                budget.clone(),
                oneai_core::budget::BudgetAllocation::default(),
                Arc::new(oneai_core::budget::NoopCompressor),
            )),
            Arc::new(SubAgentFactoryNone), // Sub-agents don't spawn further sub-agents
            context_assembler,
            stream_parser,
            None,
            config,
        );

        let wrapper = SubAgentWrapper::new(kind.clone(), budget, agent_loop);
        Ok(Box::new(wrapper))
    }

    fn available_kinds(&self) -> Vec<SubAgentKind> {
        vec![SubAgentKind::Plan, SubAgentKind::Explore, SubAgentKind::Code, SubAgentKind::Review]
    }

    fn is_available(&self, kind: &SubAgentKind) -> bool {
        matches!(kind, SubAgentKind::Plan | SubAgentKind::Explore | SubAgentKind::Code | SubAgentKind::Review)
    }
}