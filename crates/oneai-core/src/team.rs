//! Agent Team Coordination — multi-agent team execution with different strategies.
//!
//! OneAI's SubAgent system provides hierarchical delegation (main → sub-agent → summary).
//! Team coordination extends this with multi-agent collaboration patterns inspired by
//! Agno's Team mode:
//!
//! - **Coordinate**: All agents work on the same task, coordinator synthesizes results
//! - **Route**: Router agent selects the best specialist for the task
//! - **Collaborate**: Agents work in sequence, each building on the previous
//! - **Debate**: Agents argue from different perspectives, judge resolves
//!
//! Each strategy produces a TeamResult containing the final answer, individual
//! agent results, token usage, and cost information.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ─── TeamStrategy ────────────────────────────────────────────────────────────

/// Team coordination strategy — how multiple agents work together.
///
/// Inspired by Agno's Team mode (coordinate, route, collaborate) and
/// extended with a Debate strategy for adversarial verification.
///
/// Each strategy has different semantics:
/// - **Coordinate**: All agents receive the same task, each produces their
///   perspective, and a coordinator synthesizes a consensus answer.
///   Best for tasks where multiple viewpoints improve quality (analysis, planning).
///
/// - **Route**: A router agent examines the task and selects the single
///   best specialist to handle it. Other agents are not invoked.
///   Best for tasks that are clearly domain-specific (coding, research).
///
/// - **Collaborate**: Agents work in sequence — agent A's output becomes
///   part of agent B's input. Like a relay race.
///   Best for tasks that have natural sequential stages (research → plan → code).
///
/// - **Debate**: Agents argue from different perspectives. A judge agent
///   evaluates all arguments and selects the best answer.
///   Best for tasks requiring critical evaluation (design decisions, trade-offs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TeamStrategy {
    /// Coordinate — all agents work on the same task, results are merged.
    /// Like a team meeting: each member shares their perspective, then the
    /// coordinator synthesizes a consensus answer.
    Coordinate,

    /// Route — a router agent decides which specialist handles the task.
    /// Like a dispatcher: incoming requests are routed to the best-suited agent.
    Route,

    /// Collaborate — agents work in sequence, each building on the previous.
    /// Like a relay: agent A's output feeds into agent B's input.
    Collaborate,

    /// Debate — agents argue from different perspectives, judge resolves.
    /// Like a panel: multiple viewpoints debated, a judge selects the best.
    Debate,
}

impl TeamStrategy {
    /// Get a human-readable name for this strategy.
    pub fn name(&self) -> &str {
        match self {
            Self::Coordinate => "coordinate",
            Self::Route => "route",
            Self::Collaborate => "collaborate",
            Self::Debate => "debate",
        }
    }

    /// Get a description of what this strategy does.
    pub fn description(&self) -> &str {
        match self {
            Self::Coordinate => "All agents work on the same task; coordinator synthesizes consensus",
            Self::Route => "Router agent selects the best specialist for the task",
            Self::Collaborate => "Agents work in sequence, each building on previous output",
            Self::Debate => "Agents argue from different perspectives; judge resolves",
        }
    }

    /// Parse a string into a TeamStrategy.
    /// Returns None for unknown strings.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "coordinate" | "coord" => Some(Self::Coordinate),
            "route" => Some(Self::Route),
            "collaborate" | "collab" => Some(Self::Collaborate),
            "debate" => Some(Self::Debate),
            _ => None,
        }
    }

    /// All available strategies.
    pub fn all() -> Vec<Self> {
        vec![Self::Coordinate, Self::Route, Self::Collaborate, Self::Debate]
    }
}

impl std::fmt::Display for TeamStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

// ─── AgentRole ───────────────────────────────────────────────────────────────

/// Agent role in a team — defines what this agent contributes.
///
/// Inspired by CrewAI's role/backstory pattern where each agent has
/// a distinct identity, expertise, and set of tools. Unlike CrewAI's
/// stringly-typed approach, OneAI's AgentRole is strongly typed with
/// explicit SubAgentKind, tool lists, and optional system prompt override.
///
/// The role determines:
/// 1. **What the agent does** (backstory + system prompt)
/// 2. **What tools it can use** (available_tools)
/// 3. **How it's implemented** (agent_kind → SubAgentFactory creates it)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRole {
    /// Role name (e.g., "analyst", "coder", "reviewer").
    /// Used for display and identification in TeamResult.
    pub name: String,

    /// Backstory/description of this agent's expertise.
    /// Provides context for the LLM about this agent's specialization.
    /// Example: "Senior security analyst with 10 years of experience in vulnerability assessment"
    pub backstory: String,

    /// The sub-agent kind that implements this role.
    /// The SubAgentFactory creates the actual agent from this kind.
    /// For custom roles, use SubAgentKind::Custom("role_name").
    #[serde(skip)]
    pub agent_kind: SubAgentKindProxy,

    /// Tools available to this role.
    /// Overrides the sub-agent kind's default tool set.
    /// If empty, the sub-agent kind's defaults are used.
    pub available_tools: Vec<String>,

    /// System prompt override for this role.
    /// If Some, replaces the sub-agent kind's default system prompt.
    /// If None, the sub-agent kind's defaults are used.
    pub system_prompt_override: Option<String>,
}

// ─── SubAgentKindProxy ──────────────────────────────────────────────────────

/// Proxy type for SubAgentKind that is serializable.
///
/// SubAgentKind is defined in oneai-agent crate and not directly
/// serializable across crate boundaries. This proxy allows TeamConfig
/// to reference agent kinds by name string, which is resolved to
/// actual SubAgentKind at runtime by the SubAgentFactory.
///
/// The proxy stores the kind name as a string. Known kinds (plan, explore,
/// code, review) are mapped to their SubAgentKind equivalents. Unknown
/// strings become Custom kinds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubAgentKindProxy {
    /// The agent kind name (e.g., "plan", "explore", "code", "review", or custom).
    pub kind_name: String,
}

impl SubAgentKindProxy {
    /// Create a proxy for a known kind.
    pub fn plan() -> Self {
        Self { kind_name: "plan".to_string() }
    }

    /// Create a proxy for a known kind.
    pub fn explore() -> Self {
        Self { kind_name: "explore".to_string() }
    }

    /// Create a proxy for a known kind.
    pub fn code() -> Self {
        Self { kind_name: "code".to_string() }
    }

    /// Create a proxy for a known kind.
    pub fn review() -> Self {
        Self { kind_name: "review".to_string() }
    }

    /// Create a proxy for a custom kind.
    pub fn custom(name: &str) -> Self {
        Self { kind_name: name.to_string() }
    }

    /// Get the kind name.
    pub fn name(&self) -> &str {
        &self.kind_name
    }

    /// Check if this is a known (built-in) kind.
    pub fn is_builtin(&self) -> bool {
        matches!(self.kind_name.as_str(), "plan" | "explore" | "code" | "review")
    }
}

impl std::fmt::Display for SubAgentKindProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind_name)
    }
}

// ─── TeamConfig ──────────────────────────────────────────────────────────────

/// Team configuration — defines a multi-agent team.
///
/// A team consists of:
/// - A unique identifier
/// - A coordination strategy (how agents work together)
/// - A set of roles (what each agent contributes)
/// - A shared budget (total token budget for the team)
/// - Concurrency limits (max agents running simultaneously)
///
/// Example team configuration:
/// ```ignore
/// let team = TeamConfig::new("code_review_team")
///     .with_strategy(TeamStrategy::Coordinate)
///     .with_role(AgentRole {
///         name: "security_analyst".into(),
///         backstory: "Security expert".into(),
///         agent_kind: SubAgentKindProxy::custom("security"),
///         available_tools: vec!["read_file", "grep"],
///         system_prompt_override: None,
///     })
///     .with_role(AgentRole {
///         name: "style_reviewer".into(),
///         backstory: "Style expert".into(),
///         agent_kind: SubAgentKindProxy::review(),
///         available_tools: vec!["read_file", "grep"],
///         system_prompt_override: None,
///     })
///     .with_budget(TokenBudget::new(100_000));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    /// Unique team identifier.
    pub id: String,

    /// The coordination strategy for this team.
    pub strategy: TeamStrategy,

    /// The roles in this team.
    /// Each role defines a team member's expertise and tools.
    pub roles: Vec<AgentRole>,

    /// Maximum budget for the entire team.
    /// Individual agents share this budget; the coordinator
    /// allocates budget per agent based on strategy.
    #[serde(skip)]
    pub budget: TokenBudgetProxy,

    /// Maximum concurrent agents (for parallel strategies).
    /// For Coordinate and Debate, this limits how many agents
    /// run simultaneously. For Route and Collaborate, this
    /// is effectively 1 (sequential by nature).
    pub max_concurrent: usize,

    /// Whether the coordinator/judge also runs as a sub-agent.
    /// If true, a dedicated agent synthesizes/judges results.
    /// If false, the TeamCoordinator synthesizes results via
    /// template-based merging (no LLM call for synthesis).
    pub use_coordinator_agent: bool,
}

// ─── TokenBudgetProxy ───────────────────────────────────────────────────────

/// Proxy for TokenBudget that is serializable.
///
/// TokenBudget is defined in oneai-core budget module and is not
/// serde-serializable across all contexts. This proxy stores the
/// total budget value as a u32, which can be converted to a real
/// TokenBudget at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBudgetProxy {
    /// Total token budget for the team.
    pub total_tokens: u32,
}

impl TokenBudgetProxy {
    /// Create a proxy with a total token budget.
    pub fn new(total_tokens: u32) -> Self {
        Self { total_tokens }
    }

    /// Default budget (100,000 tokens — enough for 2-3 agents).
    pub fn default_budget() -> Self {
        Self::new(100_000)
    }
}

impl Default for TokenBudgetProxy {
    fn default() -> Self {
        Self::default_budget()
    }
}

impl std::fmt::Display for TokenBudgetProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} tokens", self.total_tokens)
    }
}

impl TeamConfig {
    /// Create a new team configuration with the given ID and strategy.
    pub fn new(id: &str, strategy: TeamStrategy) -> Self {
        Self {
            id: id.to_string(),
            strategy,
            roles: Vec::new(),
            budget: TokenBudgetProxy::default_budget(),
            max_concurrent: 4,
            use_coordinator_agent: true,
        }
    }

    /// Create a coordinate strategy team (all agents work on same task).
    pub fn coordinate(id: &str) -> Self {
        Self::new(id, TeamStrategy::Coordinate)
    }

    /// Create a route strategy team (router selects best agent).
    pub fn route(id: &str) -> Self {
        Self::new(id, TeamStrategy::Route)
    }

    /// Create a collaborate strategy team (sequential relay).
    pub fn collaborate(id: &str) -> Self {
        Self::new(id, TeamStrategy::Collaborate)
    }

    /// Create a debate strategy team (adversarial debate + judge).
    pub fn debate(id: &str) -> Self {
        Self::new(id, TeamStrategy::Debate)
    }

    /// Add a role to the team.
    pub fn with_role(mut self, role: AgentRole) -> Self {
        self.roles.push(role);
        self
    }

    /// Set the team budget.
    pub fn with_budget(mut self, budget: TokenBudgetProxy) -> Self {
        self.budget = budget;
        self
    }

    /// Set max concurrent agents.
    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.max_concurrent = max;
        self
    }

    /// Set whether to use a coordinator agent for synthesis.
    pub fn with_coordinator_agent(mut self, use_agent: bool) -> Self {
        self.use_coordinator_agent = use_agent;
        self
    }

    /// Validate the team configuration.
    ///
    /// Checks:
    /// - At least 1 role is defined
    /// - Budget > 0
    /// - max_concurrent > 0
    /// - Role names are unique
    /// - Route strategy requires at least 2 roles (router + specialist)
    /// - Collaborate strategy requires at least 2 roles (relay chain)
    /// - Debate strategy requires at least 3 roles (debaters + judge)
    pub fn validate(&self) -> Result<()> {
        if self.roles.is_empty() {
            return Err(crate::error::OneAIError::Team(
                "Team must have at least 1 role".to_string()
            ));
        }

        if self.budget.total_tokens == 0 {
            return Err(crate::error::OneAIError::Team(
                "Team budget must be > 0".to_string()
            ));
        }

        if self.max_concurrent == 0 {
            return Err(crate::error::OneAIError::Team(
                "max_concurrent must be > 0".to_string()
            ));
        }

        // Check unique role names
        let mut names = HashMap::new();
        for role in &self.roles {
            if let Some(_prev) = names.insert(&role.name, role) {
                return Err(crate::error::OneAIError::Team(
                    format!("Duplicate role name '{}' in team '{}'", role.name, self.id)
                ));
            }
        }

        // Strategy-specific validation
        match self.strategy {
            TeamStrategy::Route => {
                if self.roles.len() < 2 {
                    return Err(crate::error::OneAIError::Team(
                        "Route strategy requires at least 2 roles (router + specialist)".to_string()
                    ));
                }
            }
            TeamStrategy::Collaborate => {
                if self.roles.len() < 2 {
                    return Err(crate::error::OneAIError::Team(
                        "Collaborate strategy requires at least 2 roles (relay chain)".to_string()
                    ));
                }
            }
            TeamStrategy::Debate => {
                if self.roles.len() < 3 {
                    return Err(crate::error::OneAIError::Team(
                        "Debate strategy requires at least 3 roles (debaters + judge)".to_string()
                    ));
                }
            }
            TeamStrategy::Coordinate => {
                // Coordinate can work with 1+ agents
            }
        }

        Ok(())
    }

    /// Get a role by name.
    pub fn role_by_name(&self, name: &str) -> Option<&AgentRole> {
        self.roles.iter().find(|r| r.name == name)
    }

    /// Get the number of roles.
    pub fn role_count(&self) -> usize {
        self.roles.len()
    }

    /// Get role names.
    pub fn role_names(&self) -> Vec<String> {
        self.roles.iter().map(|r| r.name.clone()).collect()
    }
}

// ─── TeamResult ──────────────────────────────────────────────────────────────

/// Result of a team execution.
///
/// Contains the team's final answer (synthesized from all agent results),
/// individual results from each agent, token usage, and cost information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamResult {
    /// The team's final answer.
    /// For Coordinate: synthesized consensus from all agents
    /// For Route: the specialist agent's answer
    /// For Collaborate: the last agent's answer (after relay chain)
    /// For Debate: the judge's resolution
    pub final_answer: String,

    /// Individual results from each agent.
    /// Each entry contains the agent's role, kind, summary, and completion status.
    pub agent_results: Vec<AgentResultEntry>,

    /// Total tokens used by the team (sum of all agent tokens).
    pub total_tokens: u32,

    /// Total cost incurred by the team (sum of all agent costs).
    pub total_cost: f64,

    /// The strategy that was used.
    pub strategy: TeamStrategy,

    /// The team ID.
    pub team_id: String,
}

impl TeamResult {
    /// Create an empty team result (no agents completed).
    pub fn empty(team_id: &str, strategy: TeamStrategy) -> Self {
        Self {
            final_answer: String::new(),
            agent_results: Vec::new(),
            total_tokens: 0,
            total_cost: 0.0,
            strategy,
            team_id: team_id.to_string(),
        }
    }

    /// Whether any agent completed successfully.
    pub fn has_successful_results(&self) -> bool {
        self.agent_results.iter().any(|r| r.completed)
    }

    /// Get successful agent results only.
    pub fn successful_results(&self) -> Vec<&AgentResultEntry> {
        self.agent_results.iter().filter(|r| r.completed).collect()
    }

    /// Get failed agent results only.
    pub fn failed_results(&self) -> Vec<&AgentResultEntry> {
        self.agent_results.iter().filter(|r| !r.completed).collect()
    }
}

// ─── AgentResultEntry ────────────────────────────────────────────────────────

/// An individual agent's result within a team execution.
///
/// Each entry records:
/// - The role the agent played
/// - The agent kind used
/// - The summary the agent produced
/// - Whether the agent completed successfully
/// - How many tokens the agent used
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResultEntry {
    /// The agent's role name.
    pub role: String,

    /// The agent kind proxy.
    pub agent_kind: SubAgentKindProxy,

    /// The result text from this agent.
    pub result_text: String,

    /// Key findings from this agent.
    pub key_findings: Vec<String>,

    /// Whether this agent completed successfully.
    pub completed: bool,

    /// Tokens used by this agent.
    pub tokens_used: u32,

    /// Cost incurred by this agent.
    pub cost: f64,
}

// ─── TeamCoordinationLog trait ───────────────────────────────────────────────

/// Log trait for team coordination events.
///
/// Records team execution events for observability and debugging:
/// - When a team execution starts
/// - When each agent starts/completes
/// - When results are synthesized
/// - Budget and cost tracking per agent
///
/// Implementations can persist logs to memory, SQLite, or external services.
#[async_trait]
pub trait TeamCoordinationLog: Send + Sync {
    /// Log a team execution start.
    async fn log_team_start(&self, team_id: &str, strategy: TeamStrategy, task: &str);

    /// Log an agent start within a team execution.
    async fn log_agent_start(&self, team_id: &str, role: &str, agent_kind: &str);

    /// Log an agent completion within a team execution.
    async fn log_agent_complete(
        &self,
        team_id: &str,
        role: &str,
        agent_kind: &str,
        tokens_used: u32,
        cost: f64,
        completed: bool,
    );

    /// Log the synthesis/final answer production.
    async fn log_synthesis(
        &self,
        team_id: &str,
        strategy: TeamStrategy,
        final_answer_length: usize,
    );

    /// Log a team execution completion.
    async fn log_team_complete(&self, team_id: &str, total_tokens: u32, total_cost: f64);

    /// Get recent team coordination events.
    async fn recent_events(&self, limit: usize) -> Vec<TeamCoordinationEvent>;

    /// Get events for a specific team execution.
    async fn events_for_team(&self, team_id: &str) -> Vec<TeamCoordinationEvent>;
}

// ─── TeamCoordinationEvent ───────────────────────────────────────────────────

/// An event in the team coordination log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamCoordinationEvent {
    /// The team ID this event belongs to.
    pub team_id: String,

    /// The event kind.
    pub kind: TeamCoordinationEventKind,

    /// The role involved (for agent events).
    pub role: Option<String>,

    /// The agent kind involved (for agent events).
    pub agent_kind: Option<String>,

    /// Timestamp of the event.
    pub timestamp: DateTime<Utc>,

    /// Additional details.
    pub details: HashMap<String, String>,
}

// ─── TeamCoordinationEventKind ───────────────────────────────────────────────

/// Kind of team coordination event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TeamCoordinationEventKind {
    /// Team execution started.
    TeamStart,
    /// An agent started working.
    AgentStart,
    /// An agent completed (success or failure).
    AgentComplete,
    /// Results were synthesized into a final answer.
    Synthesis,
    /// Team execution completed.
    TeamComplete,
}

// ─── InMemoryTeamCoordinationLog ─────────────────────────────────────────────

/// In-memory implementation of TeamCoordinationLog.
///
/// Stores events in a Vec protected by a RwLock.
/// Suitable for testing and single-session scenarios.
/// Not suitable for production persistence (use SqliteTeamCoordinationLog).
pub struct InMemoryTeamCoordinationLog {
    events: Arc<tokio::sync::RwLock<Vec<TeamCoordinationEvent>>>,
}

impl InMemoryTeamCoordinationLog {
    /// Create a new in-memory log.
    pub fn new() -> Self {
        Self {
            events: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }

    /// Get the total number of events.
    pub async fn event_count(&self) -> usize {
        self.events.read().await.len()
    }
}

impl Default for InMemoryTeamCoordinationLog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TeamCoordinationLog for InMemoryTeamCoordinationLog {
    async fn log_team_start(&self, team_id: &str, strategy: TeamStrategy, task: &str) {
        let mut events = self.events.write().await;
        events.push(TeamCoordinationEvent {
            team_id: team_id.to_string(),
            kind: TeamCoordinationEventKind::TeamStart,
            role: None,
            agent_kind: None,
            timestamp: Utc::now(),
            details: HashMap::from([
                ("strategy".to_string(), strategy.name().to_string()),
                ("task".to_string(), task.to_string()),
            ]),
        });
    }

    async fn log_agent_start(&self, team_id: &str, role: &str, agent_kind: &str) {
        let mut events = self.events.write().await;
        events.push(TeamCoordinationEvent {
            team_id: team_id.to_string(),
            kind: TeamCoordinationEventKind::AgentStart,
            role: Some(role.to_string()),
            agent_kind: Some(agent_kind.to_string()),
            timestamp: Utc::now(),
            details: HashMap::new(),
        });
    }

    async fn log_agent_complete(
        &self,
        team_id: &str,
        role: &str,
        agent_kind: &str,
        tokens_used: u32,
        cost: f64,
        completed: bool,
    ) {
        let mut events = self.events.write().await;
        events.push(TeamCoordinationEvent {
            team_id: team_id.to_string(),
            kind: TeamCoordinationEventKind::AgentComplete,
            role: Some(role.to_string()),
            agent_kind: Some(agent_kind.to_string()),
            timestamp: Utc::now(),
            details: HashMap::from([
                ("tokens_used".to_string(), tokens_used.to_string()),
                ("cost".to_string(), format!("{:.4}", cost)),
                ("completed".to_string(), completed.to_string()),
            ]),
        });
    }

    async fn log_synthesis(&self, team_id: &str, strategy: TeamStrategy, final_answer_length: usize) {
        let mut events = self.events.write().await;
        events.push(TeamCoordinationEvent {
            team_id: team_id.to_string(),
            kind: TeamCoordinationEventKind::Synthesis,
            role: None,
            agent_kind: None,
            timestamp: Utc::now(),
            details: HashMap::from([
                ("strategy".to_string(), strategy.name().to_string()),
                ("final_answer_length".to_string(), final_answer_length.to_string()),
            ]),
        });
    }

    async fn log_team_complete(&self, team_id: &str, total_tokens: u32, total_cost: f64) {
        let mut events = self.events.write().await;
        events.push(TeamCoordinationEvent {
            team_id: team_id.to_string(),
            kind: TeamCoordinationEventKind::TeamComplete,
            role: None,
            agent_kind: None,
            timestamp: Utc::now(),
            details: HashMap::from([
                ("total_tokens".to_string(), total_tokens.to_string()),
                ("total_cost".to_string(), format!("{:.4}", total_cost)),
            ]),
        });
    }

    async fn recent_events(&self, limit: usize) -> Vec<TeamCoordinationEvent> {
        let events = self.events.read().await;
        events.iter().rev().take(limit).cloned().collect()
    }

    async fn events_for_team(&self, team_id: &str) -> Vec<TeamCoordinationEvent> {
        let events = self.events.read().await;
        events.iter()
            .filter(|e| e.team_id == team_id)
            .cloned()
            .collect()
    }
}

// ─── Team Presets ────────────────────────────────────────────────────────────

/// Preset team configurations for common use cases.
///
/// These presets provide ready-to-use team configurations for
/// typical multi-agent scenarios. Each preset comes with appropriate
/// roles, strategy, and budget.
pub struct TeamPresets;

impl TeamPresets {
    /// Code review team — Coordinate strategy with 3 reviewers.
    ///
    /// Security analyst, style reviewer, and correctness reviewer
    /// all examine the code, then coordinator synthesizes a consensus review.
    pub fn code_review_team() -> TeamConfig {
        TeamConfig::coordinate("code_review")
            .with_role(AgentRole {
                name: "security_analyst".into(),
                backstory: "Security expert specializing in vulnerability assessment and injection prevention".into(),
                agent_kind: SubAgentKindProxy::custom("security"),
                available_tools: vec!["read_file".into(), "grep".into(), "glob".into()],
                system_prompt_override: Some("You are a security analyst. Review code for security vulnerabilities, injection risks, and unsafe patterns. Focus on: SQL injection, XSS, path traversal, unsafe deserialization, and authentication flaws.".into()),
            })
            .with_role(AgentRole {
                name: "style_reviewer".into(),
                backstory: "Style expert specializing in code quality and best practices".into(),
                agent_kind: SubAgentKindProxy::review(),
                available_tools: vec!["read_file".into(), "grep".into()],
                system_prompt_override: Some("You are a style reviewer. Review code for readability, naming conventions, error handling patterns, and Rust best practices. Focus on: clear naming, proper error types, idiomatic patterns, and documentation.".into()),
            })
            .with_role(AgentRole {
                name: "correctness_reviewer".into(),
                backstory: "Correctness expert specializing in logic bugs and edge cases".into(),
                agent_kind: SubAgentKindProxy::review(),
                available_tools: vec!["read_file".into(), "grep".into()],
                system_prompt_override: Some("You are a correctness reviewer. Review code for logic bugs, edge cases, off-by-one errors, and incorrect assumptions. Focus on: boundary conditions, error paths, race conditions, and data consistency.".into()),
            })
            .with_budget(TokenBudgetProxy::new(80_000))
            .with_max_concurrent(3)
    }

    /// Research routing team — Route strategy with router + 3 specialists.
    ///
    /// Router decides whether to send the task to web_search, academic,
    /// or codebase specialist.
    pub fn research_routing_team() -> TeamConfig {
        TeamConfig::route("research_route")
            .with_role(AgentRole {
                name: "router".into(),
                backstory: "Task router that determines the best research specialist".into(),
                agent_kind: SubAgentKindProxy::plan(),
                available_tools: vec!["read_file".into(), "grep".into()],
                system_prompt_override: Some("You are a task router. Analyze the research request and decide which specialist should handle it: 'web' for internet research, 'academic' for scholarly analysis, or 'codebase' for code search. Reply with just the specialist name.".into()),
            })
            .with_role(AgentRole {
                name: "web_researcher".into(),
                backstory: "Web research specialist".into(),
                agent_kind: SubAgentKindProxy::explore(),
                available_tools: vec!["web_fetch".into(), "read_file".into()],
                system_prompt_override: Some("You are a web researcher. Search the internet for information and provide comprehensive findings.".into()),
            })
            .with_role(AgentRole {
                name: "academic_analyst".into(),
                backstory: "Academic analysis specialist".into(),
                agent_kind: SubAgentKindProxy::explore(),
                available_tools: vec!["read_file".into(), "grep".into()],
                system_prompt_override: Some("You are an academic analyst. Provide rigorous, well-structured analysis with citations and evidence.".into()),
            })
            .with_budget(TokenBudgetProxy::new(60_000))
    }

    /// Development pipeline team — Collaborate strategy with 3 stages.
    ///
    /// Research → Plan → Code relay chain.
    pub fn development_pipeline_team() -> TeamConfig {
        TeamConfig::collaborate("dev_pipeline")
            .with_role(AgentRole {
                name: "researcher".into(),
                backstory: "Research agent that explores the codebase and gathers context".into(),
                agent_kind: SubAgentKindProxy::explore(),
                available_tools: vec!["read_file".into(), "grep".into(), "glob".into(), "list_directory".into()],
                system_prompt_override: None,
            })
            .with_role(AgentRole {
                name: "planner".into(),
                backstory: "Planning agent that designs the implementation approach".into(),
                agent_kind: SubAgentKindProxy::plan(),
                available_tools: vec!["read_file".into(), "grep".into()],
                system_prompt_override: None,
            })
            .with_role(AgentRole {
                name: "implementer".into(),
                backstory: "Code implementation agent".into(),
                agent_kind: SubAgentKindProxy::code(),
                available_tools: vec!["read_file".into(), "edit_file".into(), "shell".into()],
                system_prompt_override: None,
            })
            .with_budget(TokenBudgetProxy::new(120_000))
    }

    /// Architecture debate team — Debate strategy with 3 perspectives + judge.
    ///
    /// Simplicity advocate, performance advocate, and extensibility advocate
    /// debate, then judge resolves.
    pub fn architecture_debate_team() -> TeamConfig {
        TeamConfig::debate("arch_debate")
            .with_role(AgentRole {
                name: "simplicity_advocate".into(),
                backstory: "Advocates for simple, straightforward solutions".into(),
                agent_kind: SubAgentKindProxy::custom("simplicity"),
                available_tools: vec!["read_file".into(), "grep".into()],
                system_prompt_override: Some("You are a simplicity advocate. Argue for the simplest possible solution. Focus on: minimal code, easy understanding, fewer dependencies, and straightforward logic. Complexity is the enemy.".into()),
            })
            .with_role(AgentRole {
                name: "performance_advocate".into(),
                backstory: "Advocates for optimized, high-performance solutions".into(),
                agent_kind: SubAgentKindProxy::custom("performance"),
                available_tools: vec!["read_file".into(), "grep".into()],
                system_prompt_override: Some("You are a performance advocate. Argue for optimized solutions. Focus on: efficient algorithms, minimal overhead, cache-friendly patterns, and benchmark-driven decisions. Performance matters.".into()),
            })
            .with_role(AgentRole {
                name: "extensibility_advocate".into(),
                backstory: "Advocates for extensible, future-proof solutions".into(),
                agent_kind: SubAgentKindProxy::custom("extensibility"),
                available_tools: vec!["read_file".into(), "grep".into()],
                system_prompt_override: Some("You are an extensibility advocate. Argue for extensible solutions. Focus on: plugin points, trait boundaries, configuration options, and forward-compatible design. Tomorrow's features need today's architecture.".into()),
            })
            .with_role(AgentRole {
                name: "judge".into(),
                backstory: "Neutral judge that evaluates all arguments and decides".into(),
                agent_kind: SubAgentKindProxy::review(),
                available_tools: vec!["read_file".into()],
                system_prompt_override: Some("You are a judge. Evaluate the arguments from each advocate and decide the best approach. Consider: simplicity, performance, extensibility, and practical constraints. Provide a reasoned decision with trade-off analysis.".into()),
            })
            .with_budget(TokenBudgetProxy::new(100_000))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_team_strategy_names() {
        assert_eq!(TeamStrategy::Coordinate.name(), "coordinate");
        assert_eq!(TeamStrategy::Route.name(), "route");
        assert_eq!(TeamStrategy::Collaborate.name(), "collaborate");
        assert_eq!(TeamStrategy::Debate.name(), "debate");
    }

    #[test]
    fn test_team_strategy_descriptions() {
        assert!(TeamStrategy::Coordinate.description().contains("synthesizes"));
        assert!(TeamStrategy::Route.description().contains("specialist"));
        assert!(TeamStrategy::Collaborate.description().contains("sequence"));
        assert!(TeamStrategy::Debate.description().contains("judge"));
    }

    #[test]
    fn test_team_strategy_from_str() {
        assert_eq!(TeamStrategy::from_str_opt("coordinate"), Some(TeamStrategy::Coordinate));
        assert_eq!(TeamStrategy::from_str_opt("coord"), Some(TeamStrategy::Coordinate));
        assert_eq!(TeamStrategy::from_str_opt("route"), Some(TeamStrategy::Route));
        assert_eq!(TeamStrategy::from_str_opt("collab"), Some(TeamStrategy::Collaborate));
        assert_eq!(TeamStrategy::from_str_opt("debate"), Some(TeamStrategy::Debate));
        assert_eq!(TeamStrategy::from_str_opt("unknown"), None);
    }

    #[test]
    fn test_team_strategy_all() {
        let all = TeamStrategy::all();
        assert_eq!(all.len(), 4);
        assert!(all.contains(&TeamStrategy::Coordinate));
    }

    #[test]
    fn test_team_strategy_display() {
        assert_eq!(format!("{}", TeamStrategy::Coordinate), "coordinate");
    }

    #[test]
    fn test_team_strategy_serialization() {
        for strategy in TeamStrategy::all() {
            let json = serde_json::to_string(&strategy).unwrap();
            let parsed: TeamStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(strategy, parsed);
        }
    }

    #[test]
    fn test_sub_agent_kind_proxy() {
        assert_eq!(SubAgentKindProxy::plan().name(), "plan");
        assert_eq!(SubAgentKindProxy::explore().name(), "explore");
        assert_eq!(SubAgentKindProxy::code().name(), "code");
        assert_eq!(SubAgentKindProxy::review().name(), "review");
        assert_eq!(SubAgentKindProxy::custom("security").name(), "security");
        assert!(SubAgentKindProxy::plan().is_builtin());
        assert!(!SubAgentKindProxy::custom("security").is_builtin());
    }

    #[test]
    fn test_token_budget_proxy() {
        let budget = TokenBudgetProxy::new(100_000);
        assert_eq!(budget.total_tokens, 100_000);
        assert_eq!(TokenBudgetProxy::default_budget().total_tokens, 100_000);
        assert_eq!(format!("{}", budget), "100000 tokens");
    }

    #[test]
    fn test_agent_role_creation() {
        let role = AgentRole {
            name: "analyst".into(),
            backstory: "Expert analyst".into(),
            agent_kind: SubAgentKindProxy::explore(),
            available_tools: vec!["read_file".into(), "grep".into()],
            system_prompt_override: None,
        };
        assert_eq!(role.name, "analyst");
        assert_eq!(role.available_tools.len(), 2);
    }

    #[test]
    fn test_team_config_creation() {
        let config = TeamConfig::new("test_team", TeamStrategy::Coordinate);
        assert_eq!(config.id, "test_team");
        assert_eq!(config.strategy, TeamStrategy::Coordinate);
        assert!(config.roles.is_empty());
        assert_eq!(config.max_concurrent, 4);
        assert!(config.use_coordinator_agent);
    }

    #[test]
    fn test_team_config_with_role() {
        let config = TeamConfig::coordinate("review_team")
            .with_role(AgentRole {
                name: "analyst".into(),
                backstory: "Expert".into(),
                agent_kind: SubAgentKindProxy::explore(),
                available_tools: vec!["read_file".into()],
                system_prompt_override: None,
            });
        assert_eq!(config.role_count(), 1);
    }

    #[test]
    fn test_team_config_validate_empty_roles() {
        let config = TeamConfig::coordinate("bad_team");
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least 1 role"));
    }

    #[test]
    fn test_team_config_validate_zero_budget() {
        let config = TeamConfig::coordinate("bad_team")
            .with_role(AgentRole {
                name: "analyst".into(),
                backstory: "Expert".into(),
                agent_kind: SubAgentKindProxy::explore(),
                available_tools: vec!["read_file".into()],
                system_prompt_override: None,
            })
            .with_budget(TokenBudgetProxy::new(0));
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("budget must be > 0"));
    }

    #[test]
    fn test_team_config_validate_route_min_roles() {
        let config = TeamConfig::route("bad_route")
            .with_role(AgentRole {
                name: "router".into(),
                backstory: "Router".into(),
                agent_kind: SubAgentKindProxy::plan(),
                available_tools: vec!["read_file".into()],
                system_prompt_override: None,
            });
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least 2 roles"));
    }

    #[test]
    fn test_team_config_validate_debate_min_roles() {
        let config = TeamConfig::debate("bad_debate")
            .with_role(AgentRole {
                name: "debater1".into(),
                backstory: "D1".into(),
                agent_kind: SubAgentKindProxy::custom("d1"),
                available_tools: vec!["read_file".into()],
                system_prompt_override: None,
            })
            .with_role(AgentRole {
                name: "debater2".into(),
                backstory: "D2".into(),
                agent_kind: SubAgentKindProxy::custom("d2"),
                available_tools: vec!["read_file".into()],
                system_prompt_override: None,
            });
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least 3 roles"));
    }

    #[test]
    fn test_team_config_validate_duplicate_names() {
        let config = TeamConfig::coordinate("dup_team")
            .with_role(AgentRole {
                name: "analyst".into(),
                backstory: "First".into(),
                agent_kind: SubAgentKindProxy::explore(),
                available_tools: vec!["read_file".into()],
                system_prompt_override: None,
            })
            .with_role(AgentRole {
                name: "analyst".into(),
                backstory: "Second".into(),
                agent_kind: SubAgentKindProxy::review(),
                available_tools: vec!["read_file".into()],
                system_prompt_override: None,
            });
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Duplicate role name"));
    }

    #[test]
    fn test_team_config_validate_valid_coordinate() {
        let config = TeamPresets::code_review_team();
        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn test_team_config_role_by_name() {
        let config = TeamPresets::code_review_team();
        let role = config.role_by_name("security_analyst");
        assert!(role.is_some());
        assert_eq!(role.unwrap().name, "security_analyst");
    }

    #[test]
    fn test_team_result_empty() {
        let result = TeamResult::empty("test", TeamStrategy::Coordinate);
        assert!(result.final_answer.is_empty());
        assert!(result.agent_results.is_empty());
        assert!(!result.has_successful_results());
    }

    #[test]
    fn test_team_result_serialization() {
        let result = TeamResult {
            final_answer: "Consensus: the code is mostly correct".into(),
            agent_results: vec![AgentResultEntry {
                role: "analyst".into(),
                agent_kind: SubAgentKindProxy::explore(),
                result_text: "Found no major issues".into(),
                key_findings: vec!["Issue 1".into()],
                completed: true,
                tokens_used: 5000,
                cost: 0.05,
            }],
            total_tokens: 5000,
            total_cost: 0.05,
            strategy: TeamStrategy::Coordinate,
            team_id: "test".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: TeamResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.final_answer, parsed.final_answer);
    }

    #[test]
    fn test_presets_code_review() {
        let config = TeamPresets::code_review_team();
        assert_eq!(config.id, "code_review");
        assert_eq!(config.strategy, TeamStrategy::Coordinate);
        assert_eq!(config.role_count(), 3);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_presets_research_routing() {
        let config = TeamPresets::research_routing_team();
        assert_eq!(config.id, "research_route");
        assert_eq!(config.strategy, TeamStrategy::Route);
        assert_eq!(config.role_count(), 3);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_presets_development_pipeline() {
        let config = TeamPresets::development_pipeline_team();
        assert_eq!(config.id, "dev_pipeline");
        assert_eq!(config.strategy, TeamStrategy::Collaborate);
        assert_eq!(config.role_count(), 3);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_presets_architecture_debate() {
        let config = TeamPresets::architecture_debate_team();
        assert_eq!(config.id, "arch_debate");
        assert_eq!(config.strategy, TeamStrategy::Debate);
        assert_eq!(config.role_count(), 4);
        assert!(config.validate().is_ok());
    }

    #[tokio::test]
    async fn test_in_memory_team_coordination_log() {
        let log = InMemoryTeamCoordinationLog::new();

        log.log_team_start("test_team", TeamStrategy::Coordinate, "Review this code").await;
        log.log_agent_start("test_team", "analyst", "explore").await;
        log.log_agent_complete("test_team", "analyst", "explore", 5000, 0.05, true).await;
        log.log_synthesis("test_team", TeamStrategy::Coordinate, 200).await;
        log.log_team_complete("test_team", 5000, 0.05).await;

        assert_eq!(log.event_count().await, 5);

        let events = log.events_for_team("test_team").await;
        assert_eq!(events.len(), 5);

        let recent = log.recent_events(3).await;
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn test_agent_result_entry_serialization() {
        let entry = AgentResultEntry {
            role: "analyst".into(),
            agent_kind: SubAgentKindProxy::explore(),
            result_text: "Found 3 issues".into(),
            key_findings: vec!["Issue A".into(), "Issue B".into()],
            completed: true,
            tokens_used: 3000,
            cost: 0.03,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: AgentResultEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.role, parsed.role);
        assert_eq!(entry.completed, parsed.completed);
    }

    #[test]
    fn test_team_coordination_event_kind() {
        assert_eq!(TeamCoordinationEventKind::TeamStart, TeamCoordinationEventKind::TeamStart);
        assert_ne!(TeamCoordinationEventKind::AgentStart, TeamCoordinationEventKind::AgentComplete);
    }

    #[test]
    fn test_team_config_role_names() {
        let config = TeamPresets::code_review_team();
        let names = config.role_names();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"security_analyst".to_string()));
    }
}
