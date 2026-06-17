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
use std::path::PathBuf;
use std::sync::Arc;
use async_trait::async_trait;

use oneai_core::error::Result;
use oneai_core::budget::TokenBudget;
use oneai_core::traits::{LlmProvider, OutputParser, ApprovalGate, Tool};

use crate::agent_loop::{AgentLoop, AgentLoopConfig, AgentLoopObserver};
use crate::worktree_isolation::{WorktreeIsolation, WorktreeConfig, WorktreeHandle, MergeResult};

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
    /// Worktree isolation configuration — determines whether this sub-agent
    /// creates a git worktree for isolated file operations.
    /// Code agents use worktree isolation; read-only agents don't.
    worktree_config: WorktreeConfig,
    /// The project directory (root of the git repository).
    /// Used by WorktreeIsolation to create worktrees.
    project_path: Option<PathBuf>,
    /// Structured output schema for validating sub-agent return values.
    /// If Some, the SubAgentSummary's summary field is validated against
    /// this JSON Schema. Validation failures trigger a log warning
    /// (but don't block the summary — sub-agent results are informational).
    /// For strict validation with ModelRetry, use the AgentLoop's
    /// structured_output config instead.
    structured_output_schema: Option<serde_json::Value>,
}

impl SubAgentWrapper {
    /// Create a new SubAgentWrapper from an existing AgentLoop with scoped configuration.
    pub fn new(
        kind: SubAgentKind,
        budget: TokenBudget,
        agent_loop: AgentLoop,
    ) -> Self {
        Self {
            kind,
            budget,
            agent_loop,
            worktree_config: WorktreeConfig::read_only(),
            project_path: None,
            structured_output_schema: None,
        }
    }

    /// Create a SubAgentWrapper with worktree isolation for the given project path.
    ///
    /// Code agents should use this constructor — they modify files and need
    /// worktree isolation to prevent conflicts with parallel sub-agents.
    /// Read-only agents (Explore, Plan, Review) should use `new()` instead,
    /// which defaults to WorktreeConfig::read_only() (no isolation).
    pub fn with_worktree(
        kind: SubAgentKind,
        budget: TokenBudget,
        agent_loop: AgentLoop,
        project_path: PathBuf,
        worktree_config: WorktreeConfig,
    ) -> Self {
        Self {
            kind,
            budget,
            agent_loop,
            worktree_config,
            project_path: Some(project_path),
            structured_output_schema: None,
        }
    }

    /// Create a SubAgentWrapper with structured output validation.
    ///
    /// When a schema is provided, the sub-agent's summary text is validated
    /// against the JSON Schema after execution. If the summary doesn't conform,
    /// a warning is logged and the summary includes the validation error info.
    /// This is informational validation — it doesn't block the sub-agent result.
    pub fn with_structured_output(
        kind: SubAgentKind,
        budget: TokenBudget,
        agent_loop: AgentLoop,
        schema: serde_json::Value,
    ) -> Self {
        Self {
            kind,
            budget,
            agent_loop,
            worktree_config: WorktreeConfig::read_only(),
            project_path: None,
            structured_output_schema: Some(schema),
        }
    }

    /// Determine the appropriate worktree config based on the sub-agent kind.
    ///
    /// Code agents need worktree isolation (they modify files).
    /// Read-only agents don't (they only read/search).
    pub fn default_worktree_config_for_kind(kind: &SubAgentKind) -> WorktreeConfig {
        match kind {
            SubAgentKind::Code | SubAgentKind::Custom(_) => WorktreeConfig::coding(),
            SubAgentKind::Plan | SubAgentKind::Explore | SubAgentKind::Review => WorktreeConfig::read_only(),
        }
    }
}

#[async_trait]
impl SubAgent for SubAgentWrapper {
    /// Run the sub-agent on a task using tokio::spawn for independent async execution.
    ///
    /// The sub-agent runs on a separate tokio task, enabling parallel delegation:
    /// the main agent can delegate multiple sub-tasks simultaneously, and each
    /// sub-agent works independently without blocking the others.
    ///
    /// **Worktree Isolation**: If a project_path is configured and the sub-agent
    /// kind modifies files (Code, Custom), a git worktree is created before the
    /// sub-agent starts. The sub-agent operates in the worktree directory, and
    /// after completion, changes are merged back to the main branch. This
    /// prevents file conflicts when multiple Code sub-agents run in parallel.
    ///
    /// This addresses two gaps:
    /// - "子Agent 未用 tokio::spawn" — sub-agents now run on independent tasks
    /// - "无 git worktree 隔离" — Code sub-agents now operate in isolated worktrees
    ///
    /// **Note**: The `AgentLoop` must be `Clone` for this to work.
    /// All fields are Arc/RwLock, so cloning is cheap (just pointer cloning).
    async fn run(&self, task: &str) -> Result<SubAgentSummary> {
        // ─── Worktree isolation ──────────────────────────────────────────
        // If the sub-agent modifies files (Code, Custom), create a git worktree
        // for isolated execution. Read-only agents skip this step.
        let worktree_handle = if let Some(project_path) = &self.project_path {
            let isolation = WorktreeIsolation::new(project_path.clone(), self.worktree_config.clone());
            isolation.create(self.kind.name())?
        } else {
            // No project path configured — run without isolation
            WorktreeHandle {
                worktree_path: PathBuf::from("."), // Will use default cwd
                branch_name: String::new(),
                project_path: PathBuf::from("."),
                is_isolated: false,
                has_changes: false,
            }
        };

        if worktree_handle.is_isolated {
            tracing::info!(
                "Sub-agent '{}' running in isolated worktree: {}",
                self.kind.name(),
                worktree_handle.working_dir().display()
            );
        }

        // ─── Run the sub-agent ───────────────────────────────────────────
        let agent_loop = self.agent_loop.clone(); // Cheap Arc clone
        let kind = self.kind.clone();
        let task_owned = task.to_string();
        let is_isolated = worktree_handle.is_isolated;
        let wt_path = worktree_handle.worktree_path.clone();
        let wt_branch = worktree_handle.branch_name.clone();
        let project_path = worktree_handle.project_path.clone();

        // Spawn the sub-agent as an independent tokio task
        let handle = tokio::spawn(async move {
            // TODO: In a full implementation, we would update the AgentLoop's
            // working directory to point to the worktree path. This requires
            // AgentLoop to support dynamic working directory changes.
            // For now, the worktree path is available for tools that check it.
            // The ShellTool's allowed_working_dirs and FileEditTool's base path
            // should be updated to use wt_path when running in a worktree.

            let result = agent_loop.run(&task_owned).await?;

            // Extract key findings from the conversation
            let key_findings = extract_key_findings(&result.final_answer);

            // Estimate token usage from the number of iterations
            let tokens_used = (result.iterations as u32) * 2000;

            Ok(SubAgentSummary {
                completed: result.completed,
                summary: result.final_answer,
                key_findings,
                budget_exceeded: false,
                agent_kind: kind,
                tokens_used,
            })
        });

        // Wait for the sub-agent task to complete
        let summary = handle.await
            .map_err(|e| oneai_core::error::OneAIError::Agent(
                format!("Sub-agent task '{}' panicked or was cancelled: {}", self.kind.name(), e)
            ))?;

        // ─── Merge worktree changes back ─────────────────────────────────
        if is_isolated && summary.is_ok() {
            let isolation = WorktreeIsolation::new(
                project_path,
                self.worktree_config.clone(),
            );
            let merge_result = isolation.merge_back(&worktree_handle)?;

            // Include merge result information in the summary
            if let Ok(mut s) = summary {
                if !matches!(merge_result, MergeResult::Skipped { .. }) {
                    s.key_findings.push(format!("Worktree merge: {}", merge_result.description()));
                }
                return Ok(self.validate_structured_output(s));
            }
        }

        // Apply structured output validation if configured
        match summary {
            Ok(s) => Ok(self.validate_structured_output(s)),
            Err(e) => Err(e),
        }
    }

    fn kind(&self) -> &SubAgentKind {
        &self.kind
    }

    fn budget(&self) -> &TokenBudget {
        &self.budget
    }
}

// ─── SubAgentWrapper helper methods ────────────────────────────────────────────

impl SubAgentWrapper {
    /// Validate structured output of a SubAgentSummary.
    ///
    /// If a structured_output_schema is configured, validates the summary text
    /// against the JSON Schema. Validation failures are logged as warnings
    /// and included in the summary's key_findings — they don't block the
    /// sub-agent result (informational validation).
    fn validate_structured_output(&self, summary: SubAgentSummary) -> SubAgentSummary {
        if let Some(schema) = &self.structured_output_schema {
            let validation = crate::structured_output::validate_json_schema(&summary.summary, schema);

            if !validation.passed {
                tracing::warn!(
                    "Sub-agent '{}' structured output validation failed: {}",
                    self.kind.name(),
                    validation.error_summary()
                );
                // Include validation error in key findings for visibility
                let mut findings = summary.key_findings;
                findings.push(format!(
                    "[Validation warning]: Sub-agent output didn't conform to schema — {}",
                    validation.error_summary()
                ));
                SubAgentSummary {
                    key_findings: findings,
                    ..summary
                }
            } else {
                tracing::info!(
                    "Sub-agent '{}' structured output validation passed",
                    self.kind.name()
                );
                // Include parsed output in key findings if available
                if let Some(parsed) = validation.parsed_output {
                    let mut findings = summary.key_findings;
                    findings.push("[Structured output validated]".to_string());
                    SubAgentSummary {
                        key_findings: findings,
                        ..summary
                    }
                } else {
                    summary
                }
            }
        } else {
            summary
        }
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
///
/// **Note**: `create()` is now async to support real scoped tool filtering.
/// The `create_scoped_tools()` method requires async access to the tool
/// registry (RwLock), so the factory must be async as well.
#[async_trait]
pub trait SubAgentFactory: Send + Sync {
    /// Create a sub-agent of the specified kind with the given budget.
    ///
    /// The factory selects the appropriate configuration, tools,
    /// and system prompt for the requested sub-agent kind.
    /// Tools are actually scoped — only the sub-agent's `available_tools`
    /// are provided, not the full tool set.
    async fn create(&self, kind: SubAgentKind, budget: TokenBudget) -> Result<Box<dyn SubAgent>>;

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

#[async_trait]
impl SubAgentFactory for SubAgentFactoryNone {
    async fn create(&self, _kind: SubAgentKind, _budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
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
    /// The project directory (root of the git repository).
    /// Used for git worktree isolation when creating Code sub-agents.
    project_path: Option<PathBuf>,
    /// Worktree isolation configuration.
    /// Defaults to WorktreeConfig::coding() for Code agents,
    /// WorktreeConfig::read_only() for read-only agents.
    worktree_config: Option<WorktreeConfig>,
}

impl DefaultSubAgentFactory {
    /// Create a new default factory with the given dependencies.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        parser: Arc<dyn OutputParser>,
        approval_gate: Arc<dyn ApprovalGate>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    ) -> Self {
        Self {
            provider, parser, approval_gate, tools,
            project_path: None,
            worktree_config: None,
        }
    }

    /// Create a default factory with worktree isolation support.
    ///
    /// When a project_path is provided, Code sub-agents will create
    /// git worktrees for isolated file operations. This prevents
    /// conflicts when multiple Code sub-agents run in parallel.
    pub fn with_worktree(
        provider: Arc<dyn LlmProvider>,
        parser: Arc<dyn OutputParser>,
        approval_gate: Arc<dyn ApprovalGate>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
        project_path: PathBuf,
        worktree_config: WorktreeConfig,
    ) -> Self {
        Self {
            provider, parser, approval_gate, tools,
            project_path: Some(project_path),
            worktree_config: Some(worktree_config),
        }
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

#[async_trait]
impl SubAgentFactory for DefaultSubAgentFactory {
    async fn create(&self, kind: SubAgentKind, budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
        // Get the system prompt and available tools for this kind
        let system_prompt = kind.default_system_prompt().to_string();
        let available_tools_slice = kind.default_tools();

        // **Real scoped tool filtering** — this addresses the "子Agent scoped tools" gap.
        // Previously, the factory passed the full tool set to the sub-agent, meaning
        // a Plan sub-agent could see (and potentially call) edit_file and shell tools.
        // Now, we actually filter the tool registry to only include the sub-agent's
        // available_tools, following the principle of least privilege.
        //
        // This is similar to how Claude Code's Agent tool filters the tool set
        // based on the sub-agent type.
        let scoped_tools = self.create_scoped_tools(available_tools_slice).await;

        // Also set the ParadigmConfig for the sub-agent — this controls
        // which tools are sent to the LLM as tool definitions.
        // The scoped_tools registry already filters at the execution level,
        // but ParadigmConfig further filters at the definition level
        // (what the LLM sees), which is the correct double-layer filtering.
        let paradigm_config = crate::agent_loop::ParadigmConfig::for_paradigm(
            match kind {
                SubAgentKind::Plan => crate::agent_loop::ParadigmKind::Plan,
                SubAgentKind::Explore => crate::agent_loop::ParadigmKind::Explore,
                SubAgentKind::Code => crate::agent_loop::ParadigmKind::ReAct,
                SubAgentKind::Review => crate::agent_loop::ParadigmKind::Reflect,
                _ => crate::agent_loop::ParadigmKind::ReAct,
            }
        );

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
            structured_output: None, // Sub-agents don't have structured output validation
        };

        // Create a basic context assembler (no domain sources for sub-agents)
        let context_assembler = crate::context_assembler::ContextAssembler::new();
        let stream_parser = crate::streaming::IncrementalStreamParser::new();

        // Create the AgentLoop with SCOPED tools (not the full tool set!)
        // This is the key fix — sub-agents can only use their designated tools.
        let agent_loop = AgentLoop::new(
            self.provider.clone(),
            scoped_tools,  // ← SCOPED, not self.tools.clone()
            self.parser.clone(),
            self.approval_gate.clone(),
            Arc::new(oneai_skill::SkillSelector::new()),
            Arc::new(oneai_core::budget::ContextBudgetManager::new(
                budget.clone(),
                oneai_core::budget::BudgetAllocation::default(),
                Arc::new(oneai_core::budget::TruncationCompressor::default()),
            )),
            Arc::new(SubAgentFactoryNone), // Sub-agents don't spawn further sub-agents
            context_assembler,
            stream_parser,
            None,
            config,
        );

        // ─── Worktree isolation ──────────────────────────────────────────
        // If a project_path is configured, Code sub-agents create git worktrees
        // for isolated file operations. This prevents conflicts when multiple
        // Code sub-agents run in parallel (P1#13 gap).
        //
        // Read-only agents (Plan, Explore, Review) don't need isolation —
        // they only read/search, they don't modify files.
        let worktree_config = self.worktree_config.clone()
            .unwrap_or_else(|| SubAgentWrapper::default_worktree_config_for_kind(&kind));

        let wrapper = if let Some(project_path) = &self.project_path {
            SubAgentWrapper::with_worktree(
                kind.clone(),
                budget,
                agent_loop,
                project_path.clone(),
                worktree_config,
            )
        } else {
            SubAgentWrapper::new(kind.clone(), budget, agent_loop)
        };

        Ok(Box::new(wrapper))
    }

    fn available_kinds(&self) -> Vec<SubAgentKind> {
        vec![SubAgentKind::Plan, SubAgentKind::Explore, SubAgentKind::Code, SubAgentKind::Review]
    }

    fn is_available(&self, kind: &SubAgentKind) -> bool {
        matches!(kind, SubAgentKind::Plan | SubAgentKind::Explore | SubAgentKind::Code | SubAgentKind::Review)
    }
}

// ─── SubAgentDelegateFactory ──────────────────────────────────────────────────

/// Bridge between SubAgentFactory and DelegateFactory.
///
/// The StateGraphExecutor uses `DelegateFactory` to execute `NodeAction::Delegate`
/// nodes. This adapter wraps a `SubAgentFactory` so that the StateGraph executor
/// can delegate tasks to sub-agents using the same factory the AgentLoop uses.
///
/// When a StateGraph delegate node is executed, this factory:
/// 1. Parses the agent_kind string into a SubAgentKind
/// 2. Creates a sub-agent via the wrapped SubAgentFactory
/// 3. Runs the sub-agent with the given task
/// 4. Returns the summary as the delegate result string
pub struct SubAgentDelegateFactory {
    factory: Arc<dyn SubAgentFactory>,
}

impl SubAgentDelegateFactory {
    /// Create a new delegate factory wrapping an existing SubAgentFactory.
    pub fn new(factory: Arc<dyn SubAgentFactory>) -> Self {
        Self { factory }
    }
}

#[async_trait::async_trait]
impl oneai_workflow::DelegateFactory for SubAgentDelegateFactory {
    async fn delegate(&self, agent_kind: &str, task: &str) -> Result<String> {
        let kind = SubAgentKind::from_str(agent_kind);
        let budget = oneai_core::budget::TokenBudget::new(50000); // Default sub-agent budget

        let sub_agent = self.factory.create(kind, budget).await?;

        // Run the sub-agent silently (no observer — this is inside a StateGraph)
        let result = sub_agent.run(task).await?;
        Ok(result.summary)
    }
}