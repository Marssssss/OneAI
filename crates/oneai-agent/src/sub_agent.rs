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

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use oneai_core::budget::TokenBudget;
use oneai_core::error::Result;
use oneai_core::traits::{InteractionGate, LlmProvider, OutputParser, Tool};
use oneai_core::{Conversation, Message};
use oneai_domain::MergedDomainPack;

use crate::agent_loop::{AgentLoop, AgentLoopConfig, AgentLoopObserver};
use crate::worktree_isolation::{MergeResult, WorktreeConfig, WorktreeHandle, WorktreeIsolation};

// Re-export so `agent_loop.rs` can reference the cancellation type without a
// second `tokio_util` import site at the call boundary.
pub use tokio_util::sync::CancellationToken;

// ─── SubAgentKind ───────────────────────────────────────────────────────────

/// System prompt for the cadence-fired `Reflect` sub-agent (Phase 2.1 Stage A).
///
/// Ports the Hermes-style learning strategy: frustration-as-signal, the
/// patch→umbrella→support-file→create preference order (Stage A: only "write
/// to memory" is actionable; the order is documented so Stage B `skill_manage`
/// slots in unchanged), and the anti-pattern of capturing transient /
/// environment failures as durable facts.
const REFLECT_SYSTEM_PROMPT: &str = "\
You are a background review sub-agent. Your job is to distill DURABLE learnings \
from the parent agent's recent activity and persist them to memory so the agent \
grows across sessions. You are NOT conversing with the user — produce no chat \
prose; call memory tools then return a 1–3 sentence summary.

## What to capture (frustration-as-signal)
Treat repeated tool failures, retries, and user corrections as the strongest \
signal. From them extract durable facts:
- preferences (\"the user wants X / the agent should prefer Y\"),
- decisions (\"chose approach A over B because …\"),
- open tasks / blockers worth resuming,
- critical files / commands / conventions discovered.

## Preference order when consolidating a learning
patch an existing skill > extend an umbrella skill > add a support-file > \
create a new skill. (In this stage you can only persist facts to memory — \
the ordering still guides WHICH fact to write: prefer updating an existing \
core-memory fact over creating a new one.)

## Anti-pattern — NEVER capture as a fact
Environment / transient failures: a missing binary, a network 429, a transient \
filesystem error, a flaky test. These are NOT durable learnings — recording \
them poisons memory. Only record what the agent or user should durably do \
differently.

## Output contract
- `core_memory_edit`: for always-on preferences / decisions (small, hot).
- `archival_memory_insert`: for episodic learnings (larger, recall-on-demand).
Then return a SubAgentSummary whose `summary` is 1–3 sentences naming what you \
persisted (or \"nothing durable — all signals were transient\").";

/// The type of sub-agent to spawn for a delegated task.
///
/// Each kind maps to a specialized agent with different capabilities:
/// - Plan: Task decomposition into ordered steps
/// - Explore: Search and understand the codebase/environment
/// - Code: Code implementation and modification
/// - Review: Review and audit existing work
/// - Reflect: Background review sub-agent (Phase 2.1 Stage A) — NOT
///   model-driven via `delegate`; spawned directly by the `AgentLoop` on a
///   cadence + on `DirectAnswer` delivery. Deliberately absent from
///   `available_kinds`/`is_available` so it never appears in the `delegate`
///   schema. Whitelist = memory tools; persists durable learnings.
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

    /// Reflect sub-agent — cadence-fired background reviewer (Phase 2.1
    /// Stage A). Distills durable learnings from the parent loop's recent
    /// activity and persists them to memory. Internal-only: spawned by the
    /// `AgentLoop`'s cadence check, never by the model via `delegate`.
    Reflect,

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
            Self::Reflect => "reflect",
            Self::Custom(name) => name,
        }
    }

    /// Parse a string into a SubAgentKind.
    /// Unknown strings are mapped to Custom.
    #[allow(clippy::should_implement_trait)] // infallible (defaults on unknown), not a true FromStr
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "plan" => Self::Plan,
            "explore" => Self::Explore,
            "code" => Self::Code,
            "review" => Self::Review,
            "reflect" => Self::Reflect,
            other => Self::Custom(other.to_string()),
        }
    }

    /// Get the default system prompt for this sub-agent kind.
    pub fn default_system_prompt(&self) -> &str {
        match self {
            Self::Plan => "You are a planning agent. Decompose the given task into ordered steps with dependencies. Return a structured plan as a numbered list.",
            Self::Explore => "You are an exploration agent. Search and understand the codebase using available tools. Return a comprehensive summary of your findings including file paths, function signatures, and key patterns.",
            Self::Code => "You are a code implementation agent. Write and modify code based on the given specification. Return a summary of all changes you made.",
            Self::Review => "You are a code review agent. Review code for correctness bugs, style issues, and potential improvements. Return a structured review with findings and suggestions.",
            Self::Reflect => REFLECT_SYSTEM_PROMPT,
            Self::Custom(_) => "You are a specialized agent. Complete the given task and return a summary of your results.",
        }
    }

    /// Get the default available tools for this sub-agent kind.
    pub fn default_tools(&self) -> &[&str] {
        match self {
            Self::Explore => &[
                "read_file",
                "grep",
                "glob",
                "list_directory",
                // Web research tools — an Explore delegation often targets
                // non-local knowledge (e.g. "explore reggae music") that the
                // local fs tools can't reach. Without these the Explore
                // sub-agent could only parrot training data. Both are
                // `PermissionLevel::Standard` (the tool's own level — the
                // sub-agent has no DomainPack), so each web call routes
                // through the InteractionGate for approval, same as the
                // parent's Standard tools (edit_file/shell).
                "web_search",
                "web_fetch",
            ],
            Self::Code => &[
                "read_file",
                "edit_file",
                "shell",
                "grep",
                "glob",
                "list_directory",
            ],
            Self::Plan => &["read_file", "grep", "glob"],
            Self::Review => &["read_file", "grep", "glob"],
            // Reflect: memory-only whitelist — it persists durable learnings,
            // never reads code / runs shell. The closed loop is memory write →
            // future-turn recall. `skill_manage` (Stage B) joins so the
            // reviewer can also patch/extend/retire skills directly — the
            // Hermes patch→umbrella→support-file→create preference order
            // becomes actionable.
            Self::Reflect => &[
                "memory_search",
                "core_memory_edit",
                "archival_memory_insert",
                "skill_manage",
            ],
            Self::Custom(_) => &["read_file", "grep", "glob", "edit_file", "shell"],
        }
    }

    /// Minimum token-budget floor for this kind — a safety guardrail against
    /// the parent under-budgeting a delegation. The parent's `budget_tokens`
    /// arg defaults to 5000 and the model often under-specifies for expensive
    /// kinds (code-gen with extended thinking burns ~8–10k tokens/iteration;
    /// a 15–18k budget exhausts in 2–4 iterations and the sub-agent dies
    /// mid-task — see /tmp/oneai-web.log 12:45+). Only kinds with a known
    /// high cost carry a floor; 0 = honor the parent's budget verbatim.
    pub fn min_budget_tokens(&self) -> u32 {
        match self {
            Self::Code => 40_000,
            // Explore does web research (multi-step fetch + synthesis) —
            // also expensive, but its failures are cheaper (no half-written
            // files), so a smaller floor.
            Self::Explore => 20_000,
            _ => 0,
        }
    }
}

// ─── DelegationSpec ─────────────────────────────────────────────────────────

/// Per-delegation specialization carried from a `delegate` meta-tool call to
/// the factory (Opt 3 role-layering + Opt 4 Fork-lite context inheritance).
///
/// `system_prompt` / `tools` override the kind's defaults; `seed_messages`
/// seeds the sub-agent's conversation with the parent's recent turns (a
/// Copy-On-Write clone — the parent's durable log is never mutated). All
/// fields optional → absent means "use the kind default / start from
/// scratch", i.e. today's behavior.
#[derive(Debug, Clone, Default)]
pub struct DelegationSpec {
    /// Override the kind's default system prompt (role layering).
    pub system_prompt: Option<String>,
    /// Narrow the sub-agent's toolset below the kind default. Names outside
    /// the kind's default set are dropped at resolution time (never widened).
    pub tools: Option<Vec<String>>,
    /// Seed the sub-agent conversation with these (already-stripped-of-system)
    /// parent messages before the task. Enables "continue from where I am".
    pub seed_messages: Option<Vec<Message>>,
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

    /// Run the sub-agent with optional progress forwarding + cancellation.
    ///
    /// `observer`, when `Some`, receives the sub-agent's iteration events
    /// (so the parent loop / UI can see mid-run progress — Opt 1). `cancel`,
    /// when `Some`, propagates a parent interrupt into the sub-agent's loop
    /// at its iteration boundary. Both default to `None` → the call degrades
    /// to `run` (silent, uncancellable), preserving the legacy contract.
    async fn run_with_observer(
        &self,
        task: &str,
        observer: Option<&dyn AgentLoopObserver>,
        cancel: Option<CancellationToken>,
    ) -> Result<SubAgentSummary> {
        let _ = (observer, cancel);
        self.run(task).await
    }

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
    /// Seed messages (Opt 4 Fork-lite). When `Some`, the sub-agent's
    /// conversation is pre-seeded with these parent messages (system
    /// messages already stripped) before the task, so the sub-agent
    /// continues from the parent's current reasoning instead of from
    /// scratch. `None` = start from scratch (today's behavior).
    seed_messages: Option<Vec<Message>>,
}

impl SubAgentWrapper {
    /// Create a new SubAgentWrapper from an existing AgentLoop with scoped configuration.
    pub fn new(kind: SubAgentKind, budget: TokenBudget, agent_loop: AgentLoop) -> Self {
        Self {
            kind,
            budget,
            agent_loop,
            worktree_config: WorktreeConfig::read_only(),
            project_path: None,
            structured_output_schema: None,
            seed_messages: None,
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
            seed_messages: None,
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
            seed_messages: None,
        }
    }

    /// Set seed messages for Fork-lite context inheritance (Opt 4). The
    /// sub-agent's conversation is pre-seeded with these (system-stripped)
    /// parent messages before the task. Builder-style.
    pub fn with_seed_messages(mut self, messages: Vec<Message>) -> Self {
        self.seed_messages = Some(messages);
        self
    }

    /// Determine the appropriate worktree config based on the sub-agent kind.
    ///
    /// Code agents need worktree isolation (they modify files).
    /// Read-only agents don't (they only read/search).
    pub fn default_worktree_config_for_kind(kind: &SubAgentKind) -> WorktreeConfig {
        match kind {
            SubAgentKind::Code | SubAgentKind::Custom(_) => WorktreeConfig::coding(),
            SubAgentKind::Plan
            | SubAgentKind::Explore
            | SubAgentKind::Review
            | SubAgentKind::Reflect => WorktreeConfig::read_only(),
        }
    }
}

/// A no-op observer used when a sub-agent is run without progress
/// forwarding (the `delegate` factory's own `run` path, or the seed path
/// with no parent observer). Mirrors `AgentLoop::run`'s internal
/// `SilentObserver` but lives here so it can be borrowed across the
/// `run_with_conversation` call.
struct NullSubAgentObserver;
impl AgentLoopObserver for NullSubAgentObserver {
    fn on_iteration_start(&self, _: usize, _: crate::agent_loop::ParadigmKind) {}
    fn on_direct_answer(&self, _: &str) {}
    fn on_tool_calls(&self, _: &[crate::agent_loop::ToolCallRequest]) {}
    fn on_tool_result(&self, _: &str, _: &str, _: &oneai_core::ToolOutput) {}
    fn on_delegate(&self, _: &str, _: &str, _: &SubAgentKind) {}
    fn on_paradigm_switch(&self, _: crate::agent_loop::ParadigmKind) {}
    fn on_checkpoint(&self, _: usize) {}
    fn on_complete(&self, _: &crate::agent_loop::AgentLoopResult) {}
}

#[async_trait]
impl SubAgent for SubAgentWrapper {
    /// Legacy entry point — runs silently and uncancellably (today's
    /// contract). Delegates to [`Self::run_with_observer`] with no observer
    /// and no cancel token.
    async fn run(&self, task: &str) -> Result<SubAgentSummary> {
        self.run_with_observer(task, None, None).await
    }

    /// Run the sub-agent, optionally forwarding progress to a parent observer
    /// (Opt 1) and/or propagating a parent interrupt (Opt 1).
    ///
    /// Runs **inline** — the outer `spawn_sub_agents_batch` already wraps each
    /// delegation in its own `JoinSet` task, so an inner `tokio::spawn` here
    /// was redundant (and blocked passing a borrowed observer through). A
    /// sub-agent panic now surfaces via the outer `JoinSet`'s `Err(join)`.
    ///
    /// **Cancel propagation**: when `cancel` is `Some`, a fire-and-forget
    /// watcher task awaits its cancellation and fires the sub-agent loop's
    /// own `cancel_token()` — the loop already checks `cancelled()` at every
    /// iteration boundary (and mid-`infer` via `tokio::select!`), so a parent
    /// interrupt lands at the next boundary without touching `AgentLoop`
    /// internals. The sub-agent's `AgentLoop` is a fresh per-delegation
    /// instance, so its token is private to this run.
    ///
    /// **Fork-lite seed (Opt 4)**: when `seed_messages` is set on this
    /// wrapper, the sub-agent conversation is pre-seeded with the parent's
    /// recent turns (system messages already stripped) before the task,
    /// starting from the parent's current reasoning. The seed is a
    /// Copy-On-Write clone — the parent's durable log is never mutated.
    async fn run_with_observer(
        &self,
        task: &str,
        observer: Option<&dyn AgentLoopObserver>,
        cancel: Option<CancellationToken>,
    ) -> Result<SubAgentSummary> {
        // ─── Worktree isolation ──────────────────────────────────────────
        // If the sub-agent modifies files (Code, Custom), create a git worktree
        // for isolated execution. Read-only agents skip this step.
        let worktree_handle = if let Some(project_path) = &self.project_path {
            let isolation =
                WorktreeIsolation::new(project_path.clone(), self.worktree_config.clone());
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

        // ─── Cancel watcher (Opt 1) ─────────────────────────────────────
        // Parent interrupt → sub-agent loop's per-iteration cancellation
        // check. Detached; dies with the sub-agent run.
        let has_cancel = cancel.is_some();
        let _cancel_watcher = cancel.map(|c| {
            let inner = self.agent_loop.cancel_token();
            let c = c.clone();
            tokio::spawn(async move {
                c.cancelled().await;
                inner.cancel();
            })
        });

        // ─── Run the sub-agent ───────────────────────────────────────────
        let agent_loop = self.agent_loop.clone(); // Cheap Arc clone
        let kind = self.kind.clone();
        let task_owned = task.to_string();
        let is_isolated = worktree_handle.is_isolated;
        let project_path = worktree_handle.project_path.clone();
        let seed = self.seed_messages.clone();

        // Resolve the observer (None → silent).
        let null = NullSubAgentObserver;
        let obs: &dyn AgentLoopObserver = observer.unwrap_or(&null);

        tracing::info!(
            sub_agent = %self.kind.name(),
            seed = self.seed_messages.is_some(),
            cancel = has_cancel,
            "sub-agent run start"
        );
        let run_result = if let Some(msgs) = seed {
            // Fork-lite: pre-seed conversation with the parent's recent turns.
            let mut conv = Conversation::new();
            for m in msgs {
                conv.add_message(m);
            }
            agent_loop
                .run_with_conversation(conv, &task_owned, obs)
                .await
        } else {
            agent_loop.run_with_observer(&task_owned, obs).await
        };

        let summary = run_result.map(|result| {
            // Extract key findings from the conversation
            let key_findings = extract_key_findings(&result.final_answer);

            // Estimate token usage from the number of iterations
            let tokens_used = (result.iterations as u32) * 2000;

            tracing::info!(
                sub_agent = %self.kind.name(),
                completed = result.completed,
                iterations = result.iterations,
                "sub-agent run end"
            );

            SubAgentSummary {
                completed: result.completed,
                summary: result.final_answer,
                key_findings,
                budget_exceeded: false,
                agent_kind: kind,
                tokens_used,
            }
        });

        // ─── Merge worktree changes back ─────────────────────────────────
        if is_isolated && summary.is_ok() {
            let isolation = WorktreeIsolation::new(project_path, self.worktree_config.clone());
            let merge_result = isolation.merge_back(&worktree_handle)?;

            // Include merge result information in the summary
            if let Ok(mut s) = summary {
                if !matches!(merge_result, MergeResult::Skipped { .. }) {
                    s.key_findings
                        .push(format!("Worktree merge: {}", merge_result.description()));
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
            let validation =
                crate::structured_output::validate_json_schema(&summary.summary, schema);

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
                if let Some(_parsed) = validation.parsed_output {
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
        if trimmed.contains('/')
            && (trimmed.contains(".rs")
                || trimmed.contains(".py")
                || trimmed.contains(".ts")
                || trimmed.contains(".js")
                || trimmed.contains(".toml")
                || trimmed.contains(".json")
                || trimmed.contains(".md"))
        {
            findings.push(trimmed.to_string());
        }

        // Error/critical patterns
        if trimmed.starts_with("Error:")
            || trimmed.starts_with("CRITICAL:")
            || trimmed.starts_with("BUG:")
            || trimmed.starts_with("\u{26A0}")
        {
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

    /// Create a sub-agent with per-delegation specialization (Opt 3 role
    /// layering + Opt 4 Fork-lite). The default implementation ignores the
    /// spec and delegates to [`Self::create`], so existing factories keep
    /// working. `DefaultSubAgentFactory` overrides it to honor
    /// `system_prompt` / `tools` / `seed_messages`.
    async fn create_with_spec(
        &self,
        kind: SubAgentKind,
        budget: TokenBudget,
        spec: DelegationSpec,
    ) -> Result<Box<dyn SubAgent>> {
        let _ = spec;
        self.create(kind, budget).await
    }

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
        Err(oneai_core::error::OneAIError::Agent(
            "Sub-agents cannot spawn further sub-agents".to_string(),
        ))
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
    interaction_gate: Arc<dyn InteractionGate>,
    tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    /// The project directory (root of the git repository).
    /// Used for git worktree isolation when creating Code sub-agents.
    project_path: Option<PathBuf>,
    /// Worktree isolation configuration.
    /// Defaults to WorktreeConfig::coding() for Code agents,
    /// WorktreeConfig::read_only() for read-only agents.
    worktree_config: Option<WorktreeConfig>,
    /// Factory embedded in each sub-agent's `AgentLoop` for NESTED delegation
    /// (Opt 2 depth control). Defaults to [`SubAgentFactoryNone`] (sub-agents
    /// can't spawn further sub-agents — today's behavior). A
    /// [`DepthLimitedSubAgentFactory`] chain is injected when `max_depth > 1`.
    child_factory: Arc<dyn SubAgentFactory>,
    /// Inherited permission pack (the PARENT's `MergedDomainPack`) threaded
    /// into each built sub-agent via `AgentLoop::with_permission_pack`, so the
    /// sub-agent inherits the parent's permission policy (e.g. CodingPack
    /// auto-approves `web_search`/`web_fetch` → an Explore sub-agent's web
    /// calls don't prompt). Permission ONLY — exposure/context/paradigm stay
    /// None (tool defaults). `None` (no parent domain pack) → sub-agents fall
    /// back to each tool's own `permission_level` (today's behavior).
    permission_pack: Option<Arc<MergedDomainPack>>,
}

impl DefaultSubAgentFactory {
    /// Create a new default factory with the given dependencies.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        parser: Arc<dyn OutputParser>,
        interaction_gate: Arc<dyn InteractionGate>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    ) -> Self {
        Self {
            provider,
            parser,
            interaction_gate,
            tools,
            project_path: None,
            worktree_config: None,
            child_factory: Arc::new(SubAgentFactoryNone),
            permission_pack: None,
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
        interaction_gate: Arc<dyn InteractionGate>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
        project_path: PathBuf,
        worktree_config: WorktreeConfig,
    ) -> Self {
        Self {
            provider,
            parser,
            interaction_gate,
            tools,
            project_path: Some(project_path),
            worktree_config: Some(worktree_config),
            child_factory: Arc::new(SubAgentFactoryNone),
            permission_pack: None,
        }
    }

    /// Set the factory used for NESTED delegation inside built sub-agents
    /// (Opt 2). Default is `SubAgentFactoryNone` (no nesting). Pass a
    /// [`DepthLimitedSubAgentFactory`] chain to allow deeper delegation.
    pub fn with_child_factory(mut self, child: Arc<dyn SubAgentFactory>) -> Self {
        self.child_factory = child;
        self
    }

    /// Inherit the parent's permission policy: thread the parent's
    /// `MergedDomainPack` so each built sub-agent's `AgentLoop` consults its
    /// `resolve_permission` for tool-approval decisions. This makes a
    /// delegated sub-agent inherit the parent's auto-approve / require-
    /// confirmation policy — e.g. the CodingPack auto-approves
    /// `web_search`/`web_fetch`, so an Explore sub-agent's web calls don't
    /// prompt. Permission ONLY (exposure/context/paradigm stay None).
    pub fn with_permission_pack(mut self, pack: Arc<MergedDomainPack>) -> Self {
        self.permission_pack = Some(pack);
        self
    }

    /// Create a scoped tool registry containing only the specified tools.
    ///
    /// Filters the full tool registry to only include tools listed in
    /// available_tools, creating an isolated tool environment for the sub-agent.
    ///
    /// If none of the preferred tools are registered (e.g. a Code sub-agent's
    /// `[read_file, edit_file, shell, ...]` against a registry without those
    /// tools — typical when no DomainPack is loaded), fall back to **all**
    /// registered tools rather than handing the sub-agent an empty toolset
    /// while its prompt tells it to "use available tools". The least-privilege
    /// scoping only bites when the named tools actually exist; when they don't,
    /// the useful behavior is to expose whatever is available.
    async fn create_scoped_tools(
        &self,
        available_tools: &[&str],
        strict: bool,
    ) -> Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>> {
        let full_tools = self.tools.read().await;
        let mut scoped = HashMap::new();

        for tool_name in available_tools {
            let key: &str = tool_name; // Borrow as &str for HashMap lookup
            if let Some(tool) = full_tools.get(key) {
                scoped.insert(tool_name.to_string(), tool.clone());
            }
        }

        // Fall back to the full tool set when the preferred tools are absent,
        // so the sub-agent isn't left with zero tools against a prompt that
        // asks it to use them. `strict` (Reflect) opts out: an empty whitelist
        // stays empty rather than widening to tools the reviewer mustn't touch.
        if scoped.is_empty() && !full_tools.is_empty() && !strict {
            tracing::info!(
                "Sub-agent preferred tools {:?} not registered — falling back to all {} available tools",
                available_tools,
                full_tools.len()
            );
            scoped = full_tools.clone();
        }

        Arc::new(tokio::sync::RwLock::new(scoped))
    }

    /// Build a sub-agent of `kind` honoring a [`DelegationSpec`] (Opt 3 role
    /// layering + Opt 4 Fork-lite). Shared by [`Self::create`] (default spec)
    /// and [`Self::create_with_spec`].
    ///
    /// - `spec.system_prompt` overrides the kind's default role prompt.
    /// - `spec.tools` NARROWS the kind's default toolset (intersection —
    ///   names outside the default set are dropped+warned, never widened).
    /// - `spec.seed_messages` is stashed on the wrapper so
    ///   [`SubAgentWrapper::run_with_observer`] seeds the sub-agent
    ///   conversation with the parent's recent turns.
    /// - The sub-agent's `AgentLoop` gets `self.child_factory` (default
    ///   [`SubAgentFactoryNone`]; a [`DepthLimitedSubAgentFactory`] chain
    ///   when `max_depth > 1`) for nested delegation (Opt 2).
    async fn build(
        &self,
        kind: SubAgentKind,
        budget: TokenBudget,
        spec: DelegationSpec,
    ) -> Result<Box<dyn SubAgent>> {
        // Per-kind budget floor: the parent's `budget_tokens` is advisory;
        // expensive kinds (code-gen + thinking) get a minimum so a
        // well-intentioned-but-too-small budget doesn't starve the sub-agent
        // to death mid-task. Higher budgets pass through unchanged.
        let budget = {
            let floor = kind.min_budget_tokens();
            if budget.total < floor {
                tracing::info!(
                    kind = kind.name(),
                    requested = budget.total,
                    floor,
                    "sub-agent budget below kind floor; raising to the floor"
                );
                TokenBudget::new(floor)
            } else {
                budget
            }
        };
        // Resolve the system prompt (Opt 3 role layering).
        let system_prompt = spec
            .system_prompt
            .unwrap_or_else(|| kind.default_system_prompt().to_string());

        // Resolve the effective tool set (Opt 3 least-privilege narrowing).
        let default_tools: Vec<&str> = kind.default_tools().to_vec();
        let effective_tools: Vec<String> = match &spec.tools {
            Some(req) => {
                // Intersect with the kind default — never widen. Out-of-set
                // names are dropped with a warning so the model learns the
                // privilege ceiling of the chosen kind.
                let default_lookup: std::collections::HashSet<&str> =
                    default_tools.iter().copied().collect();
                req.iter()
                    .filter(|t| {
                        let ok = default_lookup.contains(t.as_str());
                        if !ok {
                            tracing::warn!(
                                "Delegate tools override '{}' not in kind '{}' default set — dropped (never widened)",
                                t,
                                kind.name()
                            );
                        }
                        ok
                    })
                    .cloned()
                    .collect()
            }
            None => default_tools.iter().map(|s| s.to_string()).collect(),
        };
        let effective_slice: Vec<&str> = effective_tools.iter().map(|s| s.as_str()).collect();

        // `Reflect` is strict (empty whitelist stays empty). Other kinds
        // keep the fallback-to-all behavior when their preferred tools are
        // unregistered (no DomainPack).
        let strict_whitelist = matches!(kind, SubAgentKind::Reflect);
        let scoped_tools = self
            .create_scoped_tools(&effective_slice, strict_whitelist)
            .await;

        let _paradigm_config = crate::agent_loop::ParadigmConfig::for_paradigm(match kind {
            SubAgentKind::Plan => crate::agent_loop::ParadigmKind::Plan,
            SubAgentKind::Explore => crate::agent_loop::ParadigmKind::Explore,
            SubAgentKind::Code => crate::agent_loop::ParadigmKind::ReAct,
            SubAgentKind::Review | SubAgentKind::Reflect => {
                crate::agent_loop::ParadigmKind::Reflect
            }
            _ => crate::agent_loop::ParadigmKind::ReAct,
        });

        let config = AgentLoopConfig {
            system_prompt,
            // Sub-agents stream their inference. Two reasons (Opt 1 hardening):
            // 1) the streaming path carries the 60s idle-timeout guard (a
            //    stalled SSE → retryable error, not an indefinite hang) that the
            //    non-streaming `infer` path lacks — a delegated Explore agent
            //    whose provider stalls must not hang the parent's batch
            //    (and thus the whole turn, since the directive pump holds the
            //    session lock for `run_turn`).
            // 2) `infer_stream`'s `tokio::select!` honors the loop's
            //    `cancel_token`, so a parent interrupt actually breaks a
            //    mid-flight sub-agent inference (the cancel watcher fires the
            //    sub-agent's token; non-streaming `infer` has no such select).
            // Stream chunks stay inside the sub-agent (ForwardingObserver
            //    no-ops `on_stream_chunk`) — no parent/UI flooding.
            use_streaming: true,
            temperature: Some(0.3),
            top_p: None,
            // Defer max_tokens to the provider (it knows its own model ceiling),
            // like the main agent — NOT `Some(budget.total)`. Coupling the
            // per-inference cap to the run budget let a single inference burn
            // the ENTIRE budget on extended thinking (glm-5.2 generates huge
            // thinking blobs): with `max_tokens = budget.total = 40000`, one
            // inference produced ~40k tokens of reasoning over ~7–8 min and
            // the run-cost budget (also 40k) was exhausted after 1–2
            // iterations — the sub-agent thought itself to death, doing no
            // real work (see /tmp/oneai-web.log 15:52→16:12). With None, each
            // inference is bounded by the provider's sane default (~4k), and
            // the 40k run budget buys ~10 iterations of actual work.
            max_tokens: None,
            // Disable extended thinking for execute-style sub-agents (Code).
            // Verified live against glm-5.2 via DashScope: with default
            // reasoning effort "max", a single inference emits 100k–170k chars
            // of `reasoning_content` (≈30–50k tokens) and burns the whole 40k
            // run-cost budget in ONE iteration, producing no code. Bounding via
            // `max_tokens` does NOT help — GLM spends the entire cap on
            // reasoning and emits zero content (max_tokens=8192 → 25k chars
            // thinking, 0 chars code). `enable_thinking: false` (mapped from
            // `thinking_budget: Some(0)` in the OpenAI provider) fully
            // suppresses reasoning: the model emits complete pure-code output
            // in one shot (~8k tokens for a full gomoku module) — so 40k buys
            // ~5 productive iterations. Reasoning-style sub-agents
            // (Reflect/Review/Plan) keep thinking (None) — they must reason.
            thinking_budget: if matches!(kind, SubAgentKind::Code) {
                Some(0)
            } else {
                None
            },
            stop_sequences: Vec::new(),
            hard_max_iterations: Some(if matches!(kind, SubAgentKind::Reflect) {
                16
            } else {
                50
            }),
            token_budget: Some(budget.clone().charge_completion_only()),
            inject_skills: false,
            usage_tracker: None,
            rate_limiter: None,
            circuit_breaker: None,
            token_counter: None,
            context_manager: None,
            structured_output: None,
            constrained_output_policy: oneai_core::ConstrainedOutputPolicy::Auto,
            trace_context: None,
            #[cfg(feature = "otel")]
            metrics_provider: None,
            plan_mode: false,
            prompt_cache_policy: oneai_core::PromptCachePolicy::Auto,
            reflection_cadence: None,
        };

        let context_assembler = crate::context_assembler::ContextAssembler::new();
        let stream_parser = crate::streaming::IncrementalStreamParser::new();

        let agent_loop = AgentLoop::new(
            self.provider.clone(),
            scoped_tools,
            self.parser.clone(),
            self.interaction_gate.clone(),
            Arc::new(oneai_skill::SkillSelector::new()),
            Arc::new(oneai_core::budget::ContextBudgetManager::new(
                budget.clone(),
                oneai_core::budget::BudgetAllocation::default(),
                Arc::new(oneai_core::budget::TruncationCompressor::default()),
            )),
            self.child_factory.clone(),
            context_assembler,
            stream_parser,
            config,
        )
        .with_optional_permission_pack(self.permission_pack.clone());

        let worktree_config = self
            .worktree_config
            .clone()
            .unwrap_or_else(|| SubAgentWrapper::default_worktree_config_for_kind(&kind));

        let mut wrapper = if let Some(project_path) = &self.project_path {
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

        // Opt 4: stash seed messages for the run path (COW injection).
        if let Some(msgs) = spec.seed_messages {
            wrapper = wrapper.with_seed_messages(msgs);
        }

        Ok(Box::new(wrapper))
    }
}

#[async_trait]
impl SubAgentFactory for DefaultSubAgentFactory {
    async fn create(&self, kind: SubAgentKind, budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
        self.build(kind, budget, DelegationSpec::default()).await
    }

    async fn create_with_spec(
        &self,
        kind: SubAgentKind,
        budget: TokenBudget,
        spec: DelegationSpec,
    ) -> Result<Box<dyn SubAgent>> {
        self.build(kind, budget, spec).await
    }

    fn available_kinds(&self) -> Vec<SubAgentKind> {
        vec![
            SubAgentKind::Plan,
            SubAgentKind::Explore,
            SubAgentKind::Code,
            SubAgentKind::Review,
        ]
    }

    fn is_available(&self, kind: &SubAgentKind) -> bool {
        matches!(
            kind,
            SubAgentKind::Plan | SubAgentKind::Explore | SubAgentKind::Code | SubAgentKind::Review
        )
    }
}

// ─── DepthLimitedSubAgentFactory ─────────────────────────────────────────────

/// Configurable-depth gate installed as a sub-agent's *child* factory to
/// control how deep nested delegation can go (Opt 2 resource bound). The gate
/// represents the factory of a sub-agent at nesting `level` (parent's direct
/// sub-agent = level 1; its grandchild = level 2; …). It allows creating a
/// child iff `level <= max_depth`; otherwise it refuses, so a model that
/// over-decomposes can't recurse without bound (the "递归风暴" gap).
///
/// The PARENT loop's own `sub_agent_factory` is **not** a gate — it's a plain
/// `DefaultSubAgentFactory` whose `child_factory` is the level-2 gate, so the
/// parent's direct sub-agents (level 1) are always creatable. That keeps
/// today's behavior intact when `max_depth = 1`: the level-2 gate refuses
/// (2 > 1), identical to the old hard-coded `SubAgentFactoryNone`. See
/// [`DepthLimitedSubAgentFactory::build_parent_factory`].
pub struct DepthLimitedSubAgentFactory {
    inner: Arc<dyn SubAgentFactory>,
    level: usize,
    max_depth: usize,
}

impl DepthLimitedSubAgentFactory {
    /// Wrap `inner` with a depth gate. `level` is the nesting level of the
    /// sub-agent whose factory this is (parent's direct sub-agent = 1). A
    /// `create` here proceeds iff `level <= max_depth`.
    pub fn new(inner: Arc<dyn SubAgentFactory>, level: usize, max_depth: usize) -> Self {
        Self {
            inner,
            level,
            max_depth,
        }
    }

    /// Build the factory to install as the PARENT loop's `sub_agent_factory`.
    /// Returns a plain `DefaultSubAgentFactory` whose embedded `child_factory`
    /// is the level-2 gate (so the parent's direct sub-agents — level 1 — are
    /// always creatable, and their nested delegation is gated by `max_depth`).
    ///
    /// `max_depth = 1` ⇒ the level-2 gate refuses (2 > 1) ⇒ no nesting,
    /// identical to today's `DefaultSubAgentFactory::new` (whose default
    /// `child_factory` is `SubAgentFactoryNone`). Raise `max_depth` to unlock
    /// deeper delegation.
    pub fn build_parent_factory(
        template: DefaultSubAgentFactory,
        max_depth: usize,
    ) -> Arc<dyn SubAgentFactory> {
        // The child_factory for a sub-agent at nesting `level`:
        //   - level > max_depth → refuse (SubAgentFactoryNone)
        //   - else → a Gate at `level` wrapping a template whose child_factory
        //     is the gate for `level + 1`.
        fn child_for_level(
            template: &DefaultSubAgentFactory,
            level: usize,
            max_depth: usize,
        ) -> Arc<dyn SubAgentFactory> {
            if level > max_depth {
                return Arc::new(SubAgentFactoryNone);
            }
            let next = child_for_level(template, level + 1, max_depth);
            let inner = template.clone().with_child_factory(next);
            Arc::new(DepthLimitedSubAgentFactory::new(
                Arc::new(inner),
                level,
                max_depth,
            ))
        }
        // Parent's direct sub-agents are level 1 (always creatable); their
        // child_factory gates level 2 upward.
        let child = child_for_level(&template, 2, max_depth);
        Arc::new(template.with_child_factory(child))
    }
}

#[async_trait]
impl SubAgentFactory for DepthLimitedSubAgentFactory {
    async fn create(&self, kind: SubAgentKind, budget: TokenBudget) -> Result<Box<dyn SubAgent>> {
        if self.level > self.max_depth {
            return Err(oneai_core::error::OneAIError::Agent(format!(
                "Delegation depth limit (level {} > max {}) reached — sub-agent '{}' cannot spawn further sub-agents",
                self.level,
                self.max_depth,
                kind.name()
            )));
        }
        self.inner.create(kind, budget).await
    }

    async fn create_with_spec(
        &self,
        kind: SubAgentKind,
        budget: TokenBudget,
        spec: DelegationSpec,
    ) -> Result<Box<dyn SubAgent>> {
        if self.level > self.max_depth {
            return Err(oneai_core::error::OneAIError::Agent(format!(
                "Delegation depth limit (level {} > max {}) reached — sub-agent '{}' cannot spawn further sub-agents",
                self.level,
                self.max_depth,
                kind.name()
            )));
        }
        self.inner.create_with_spec(kind, budget, spec).await
    }

    fn available_kinds(&self) -> Vec<SubAgentKind> {
        self.inner.available_kinds()
    }

    fn is_available(&self, kind: &SubAgentKind) -> bool {
        self.inner.is_available(kind)
    }
}

impl Clone for DefaultSubAgentFactory {
    /// Cheap clone — all fields are Arc. Lets `chain` build layered factories
    /// that share the provider/parser/tools/worktree wiring but differ in
    /// their embedded `child_factory`.
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            parser: self.parser.clone(),
            interaction_gate: self.interaction_gate.clone(),
            tools: self.tools.clone(),
            project_path: self.project_path.clone(),
            worktree_config: self.worktree_config.clone(),
            child_factory: self.child_factory.clone(),
            permission_pack: self.permission_pack.clone(),
        }
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
        let result = sub_agent.run_with_observer(task, None, None).await?;
        Ok(result.summary)
    }
}
#[cfg(test)]
mod scoped_tools_tests {
    //! Verifies the no-DomainPack sub-agent tool fallback: when the hardcoded
    //! preferred tool names are absent from the registry, the scoped set falls
    //! back to all registered tools instead of leaving the sub-agent with an
    //! empty toolset against a prompt that asks it to use tools.
    use super::*;
    use crate::mock_provider::MockProvider;
    use crate::mock_tool::MockTool;
    use oneai_core::traits::Tool;
    use oneai_parser::ThreeLayerParser;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[allow(clippy::type_complexity)]
    fn build_factory(
        tool_names: &[&str],
    ) -> (
        DefaultSubAgentFactory,
        Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    ) {
        let mut map: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        for n in tool_names {
            map.insert(
                n.to_string(),
                Arc::new(MockTool::success_tool(*n, "ok")) as Arc<dyn Tool>,
            );
        }
        let tools = Arc::new(tokio::sync::RwLock::new(map));
        let factory = DefaultSubAgentFactory::new(
            Arc::new(MockProvider::from_script(vec![])),
            Arc::new(ThreeLayerParser::new()),
            Arc::new(oneai_tool::NoopInteractionGate),
            tools.clone(),
        );
        (factory, tools)
    }

    #[tokio::test]
    async fn scoped_tools_match_preferred_set() {
        // Coding tools present → scoped to the Code kind's preferred set only.
        let (factory, _tools) = build_factory(&[
            "read_file",
            "edit_file",
            "shell",
            "grep",
            "glob",
            "memory_search",
        ]);
        let scoped = factory
            .create_scoped_tools(SubAgentKind::Code.default_tools(), false)
            .await;
        let scoped_names: Vec<String> = scoped.read().await.keys().cloned().collect();
        assert!(scoped_names.contains(&"read_file".to_string()));
        assert!(scoped_names.contains(&"edit_file".to_string()));
        // memory_search is registered but not in the Code preferred set → excluded.
        assert!(!scoped_names.contains(&"memory_search".to_string()));
    }

    #[tokio::test]
    async fn scoped_tools_fall_back_to_all_when_preferred_absent() {
        // No coding tools registered (no DomainPack) → fallback exposes whatever
        // IS available, rather than an empty set.
        let (factory, _tools) = build_factory(&["memory_search", "web_fetch"]);
        let scoped = factory
            .create_scoped_tools(SubAgentKind::Code.default_tools(), false)
            .await;
        let scoped_names: Vec<String> = scoped.read().await.keys().cloned().collect();
        assert_eq!(scoped_names.len(), 2);
        assert!(scoped_names.contains(&"memory_search".to_string()));
        assert!(scoped_names.contains(&"web_fetch".to_string()));
    }

    #[tokio::test]
    async fn scoped_tools_empty_registry_yields_empty() {
        // Truly empty registry → no fallback possible, empty set (honest).
        let (factory, _tools) = build_factory(&[]);
        let scoped = factory
            .create_scoped_tools(SubAgentKind::Explore.default_tools(), false)
            .await;
        assert!(scoped.read().await.is_empty());
    }
}
