//! Agent Swarm & Dynamic Orchestration — dynamic agent pools for complex tasks.
//!
//! OneAI's SubAgent system provides hierarchical delegation, Teams provide fixed-role
//! coordination, and Handoffs provide sequential control transfer. The swarm pattern
//! adds **dynamic, capability-driven orchestration**:
//!
//! - Agents register their capabilities (what they can do, how well, how fast)
//! - The swarm router assigns subtasks based on real-time capability matching
//! - Agents can dynamically join/leave the swarm
//! - Results are aggregated and quality-validated before acceptance
//! - Failed tasks are retried with alternative agents
//!
//! This is different from Teams (fixed roles) and Handoffs (one-to-one transfer):
//! - **Teams** have predefined roles and strategies
//! - **Handoffs** are sequential control transfers
//! - **Swarms** are dynamic, capability-driven, and self-organizing
//!
//! The swarm pattern is inspired by:
//! - **AutoGen**: Event-driven runtime where agents communicate through messages
//! - **CrewAI**: Dynamic task delegation based on agent capabilities
//! - **OpenAI Swarm**: Lightweight multi-agent orchestration with dynamic routing

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::team::SubAgentKindProxy;
use crate::team::TokenBudgetProxy;
use crate::error::Result;

// ─── AgentCapability ───────────────────────────────────────────────────────────

/// Agent capability profile — what an agent can do and how well.
///
/// Each agent in a swarm registers its capabilities, which determine:
/// - What task categories it handles (e.g., "code", "research", "review")
/// - How well it handles each category (quality score 0.0–1.0)
/// - How fast it handles each category (speed score 0.0–1.0)
/// - How many concurrent tasks it can handle
/// - How much it costs per 1000 tokens
///
/// The SwarmOrchestrator uses these profiles to route tasks to the
/// best-suited agent, considering quality, speed, cost, and current load.
///
/// **Usage**:
/// ```ignore
/// let capability = AgentCapability::new()
///     .with_category("code", 0.9, 0.7)   // High quality, moderate speed
///     .with_category("review", 0.85, 0.8) // Good quality, fast
///     .with_max_concurrent(2)
///     .with_cost_per_1k(0.03);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapability {
    /// The task categories this agent handles (e.g., "code", "research", "review").
    pub categories: Vec<String>,

    /// Quality score per category (0.0–1.0).
    /// Higher = better output quality. Used by BestFit and CostOptimized routing.
    pub quality_scores: HashMap<String, f64>,

    /// Speed score per category (0.0–1.0, where 1.0 = fastest).
    /// Used by Fastest routing strategy.
    pub speed_scores: HashMap<String, f64>,

    /// Maximum concurrent tasks this agent can handle.
    /// Used by LoadBalanced routing to avoid overloading agents.
    pub max_concurrent: usize,

    /// Cost per 1000 tokens for this agent.
    /// Used by CostOptimized routing to minimize total cost.
    pub cost_per_1k: f64,
}

impl AgentCapability {
    /// Create a new empty capability profile.
    pub fn new() -> Self {
        Self {
            categories: Vec::new(),
            quality_scores: HashMap::new(),
            speed_scores: HashMap::new(),
            max_concurrent: 1,
            cost_per_1k: 0.0,
        }
    }

    /// Create a capability profile for a single category.
    pub fn single_category(category: &str, quality: f64, speed: f64) -> Self {
        Self {
            categories: vec![category.to_string()],
            quality_scores: HashMap::from([(category.to_string(), quality)]),
            speed_scores: HashMap::from([(category.to_string(), speed)]),
            max_concurrent: 1,
            cost_per_1k: 0.0,
        }
    }

    /// Add a category with quality and speed scores.
    pub fn with_category(mut self, category: &str, quality: f64, speed: f64) -> Self {
        self.categories.push(category.to_string());
        self.quality_scores.insert(category.to_string(), quality);
        self.speed_scores.insert(category.to_string(), speed);
        self
    }

    /// Set maximum concurrent tasks.
    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.max_concurrent = max;
        self
    }

    /// Set cost per 1000 tokens.
    pub fn with_cost_per_1k(mut self, cost: f64) -> Self {
        self.cost_per_1k = cost;
        self
    }

    /// Get the quality score for a category.
    /// Returns 0.0 if the category is not registered.
    pub fn quality_for(&self, category: &str) -> f64 {
        self.quality_scores.get(category).copied().unwrap_or(0.0)
    }

    /// Get the speed score for a category.
    /// Returns 0.0 if the category is not registered.
    pub fn speed_for(&self, category: &str) -> f64 {
        self.speed_scores.get(category).copied().unwrap_or(0.0)
    }

    /// Check if this agent can handle a given category.
    pub fn handles_category(&self, category: &str) -> bool {
        self.categories.iter().any(|c| c == category)
    }

    /// Get the best category match for a task description.
    ///
    /// Performs simple keyword matching: if the task description
    /// contains any of the agent's category keywords, that category
    /// is returned. If multiple categories match, the one with the
    /// highest quality score is returned.
    pub fn best_match_for_task(&self, task: &str) -> Option<String> {
        let task_lower = task.to_lowercase();
        let matches: Vec<String> = self.categories.iter()
            .filter(|c| task_lower.contains(&c.to_lowercase()))
            .cloned()
            .collect();

        if matches.is_empty() {
            // If no keyword match, return the category with highest quality
            self.categories.iter()
                .max_by_key(|c| (self.quality_for(c) * 1000.0) as u64)
                .cloned()
        } else {
            // Return the matching category with highest quality
            matches.iter()
                .max_by_key(|c| (self.quality_scores.get(c.as_str()).copied().unwrap_or(0.0) * 1000.0) as u64)
                .cloned()
        }
    }

    /// Validate the capability profile.
    pub fn validate(&self) -> Result<()> {
        if self.categories.is_empty() {
            return Err(crate::error::OneAIError::Swarm(
                "Agent capability must have at least 1 category".to_string()
            ));
        }

        // Check that every category has quality and speed scores
        for category in &self.categories {
            if !self.quality_scores.contains_key(category) {
                return Err(crate::error::OneAIError::Swarm(
                    format!("Category '{}' missing quality score", category)
                ));
            }
            if !self.speed_scores.contains_key(category) {
                return Err(crate::error::OneAIError::Swarm(
                    format!("Category '{}' missing speed score", category)
                ));
            }
        }

        // Check score ranges
        for (cat, score) in &self.quality_scores {
            if *score < 0.0 || *score > 1.0 {
                return Err(crate::error::OneAIError::Swarm(
                    format!("Quality score for '{}' must be 0.0–1.0, got {}", cat, score)
                ));
            }
        }
        for (cat, score) in &self.speed_scores {
            if *score < 0.0 || *score > 1.0 {
                return Err(crate::error::OneAIError::Swarm(
                    format!("Speed score for '{}' must be 0.0–1.0, got {}", cat, score)
                ));
            }
        }

        if self.max_concurrent == 0 {
            return Err(crate::error::OneAIError::Swarm(
                "max_concurrent must be > 0".to_string()
            ));
        }

        Ok(())
    }
}

impl Default for AgentCapability {
    fn default() -> Self {
        Self::new()
    }
}

// ─── SwarmRouting ──────────────────────────────────────────────────────────────

/// Swarm routing strategy — how tasks are assigned to agents.
///
/// Different strategies optimize for different goals:
/// - **BestFit**: Highest quality for the task category
/// - **LoadBalanced**: Distribute across agents, considering current load
/// - **CostOptimized**: Cheapest agent that meets quality threshold
/// - **Fastest**: Agent with highest speed score
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SwarmRouting {
    /// Best-fit — assign to the agent with highest quality score for the category.
    /// Like a specialist referral: the best expert handles the task.
    BestFit,

    /// Load-balanced — distribute across agents, considering current load.
    /// Like a call center: distribute calls to available operators.
    LoadBalanced,

    /// Cost-optimized — assign to the cheapest agent that meets quality threshold.
    /// Like budget shopping: find the best value for the price.
    CostOptimized,

    /// Fastest — assign to the agent with highest speed score.
    /// Like express delivery: get results as quickly as possible.
    Fastest,
}

impl SwarmRouting {
    /// Get a human-readable name for this routing strategy.
    pub fn name(&self) -> &str {
        match self {
            Self::BestFit => "best-fit",
            Self::LoadBalanced => "load-balanced",
            Self::CostOptimized => "cost-optimized",
            Self::Fastest => "fastest",
        }
    }

    /// Get a description of what this routing strategy optimizes for.
    pub fn description(&self) -> &str {
        match self {
            Self::BestFit => "Highest quality — assign to the agent with best quality score for the category",
            Self::LoadBalanced => "Balanced load — distribute across agents to avoid overloading",
            Self::CostOptimized => "Lowest cost — assign to cheapest agent that meets quality threshold",
            Self::Fastest => "Fastest result — assign to agent with highest speed score",
        }
    }

    /// Parse a string into a SwarmRouting strategy.
    /// Returns None for unknown strings.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "best-fit" | "bestfit" | "best" => Some(Self::BestFit),
            "load-balanced" | "loadbalanced" | "load" | "balanced" => Some(Self::LoadBalanced),
            "cost-optimized" | "costoptimized" | "cost" | "cheap" => Some(Self::CostOptimized),
            "fastest" | "fast" | "speed" => Some(Self::Fastest),
            _ => None,
        }
    }

    /// All available routing strategies.
    pub fn all() -> Vec<Self> {
        vec![Self::BestFit, Self::LoadBalanced, Self::CostOptimized, Self::Fastest]
    }
}

impl std::fmt::Display for SwarmRouting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

// ─── SwarmConfig ───────────────────────────────────────────────────────────────

/// Swarm configuration — defines a dynamic agent pool.
///
/// A swarm consists of:
/// - A unique identifier
/// - Registered agents with their capability profiles
/// - A routing strategy (how tasks are assigned)
/// - A quality threshold (minimum quality for accepting results)
/// - Retry settings (how many times to retry failed tasks)
/// - A shared budget (total token budget for the swarm)
///
/// **Usage**:
/// ```ignore
/// let config = SwarmConfig::new("analysis_swarm")
///     .with_routing(SwarmRouting::BestFit)
///     .with_agent("coder", AgentCapability::single_category("code", 0.9, 0.7)
///         .with_agent_kind(SubAgentKindProxy::code()))
///     .with_agent("researcher", AgentCapability::single_category("research", 0.85, 0.8)
///         .with_agent_kind(SubAgentKindProxy::explore()))
///     .with_quality_threshold(0.7)
///     .with_max_retries(2)
///     .with_budget(TokenBudgetProxy::new(100_000));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmConfig {
    /// Unique swarm identifier.
    pub id: String,

    /// Registered agents in the swarm (name → SwarmAgentEntry).
    pub agents: Vec<SwarmAgentEntry>,

    /// Task routing strategy for the swarm.
    pub routing: SwarmRouting,

    /// Quality threshold for accepting results (0.0–1.0).
    /// Results below this threshold trigger retry with alternative agent.
    pub quality_threshold: f64,

    /// Maximum retries per subtask before escalating.
    pub max_retries: usize,

    /// Budget for the entire swarm execution.
    #[serde(skip)]
    pub budget: TokenBudgetProxy,

    /// Maximum concurrent tasks (limits parallel execution).
    pub max_concurrent_tasks: usize,
}

impl SwarmConfig {
    /// Create a new swarm config with the given ID and routing strategy.
    pub fn new(id: &str, routing: SwarmRouting) -> Self {
        Self {
            id: id.to_string(),
            agents: Vec::new(),
            routing,
            quality_threshold: 0.7,
            max_retries: 2,
            budget: TokenBudgetProxy::default_budget(),
            max_concurrent_tasks: 4,
        }
    }

    /// Create a best-fit swarm (highest quality routing).
    pub fn best_fit(id: &str) -> Self {
        Self::new(id, SwarmRouting::BestFit)
    }

    /// Create a load-balanced swarm.
    pub fn load_balanced(id: &str) -> Self {
        Self::new(id, SwarmRouting::LoadBalanced)
    }

    /// Create a cost-optimized swarm.
    pub fn cost_optimized(id: &str) -> Self {
        Self::new(id, SwarmRouting::CostOptimized)
    }

    /// Create a fastest swarm.
    pub fn fastest(id: &str) -> Self {
        Self::new(id, SwarmRouting::Fastest)
    }

    /// Add an agent to the swarm.
    pub fn with_agent(mut self, agent: SwarmAgentEntry) -> Self {
        self.agents.push(agent);
        self
    }

    /// Set the routing strategy.
    pub fn with_routing(mut self, routing: SwarmRouting) -> Self {
        self.routing = routing;
        self
    }

    /// Set the quality threshold.
    pub fn with_quality_threshold(mut self, threshold: f64) -> Self {
        self.quality_threshold = threshold;
        self
    }

    /// Set maximum retries per task.
    pub fn with_max_retries(mut self, retries: usize) -> Self {
        self.max_retries = retries;
        self
    }

    /// Set the swarm budget.
    pub fn with_budget(mut self, budget: TokenBudgetProxy) -> Self {
        self.budget = budget;
        self
    }

    /// Set maximum concurrent tasks.
    pub fn with_max_concurrent_tasks(mut self, max: usize) -> Self {
        self.max_concurrent_tasks = max;
        self
    }

    /// Validate the swarm configuration.
    ///
    /// Checks:
    /// - At least 1 agent is registered
    /// - Budget > 0
    /// - Quality threshold is 0.0–1.0
    /// - Agent names are unique
    /// - Each agent's capability is valid
    pub fn validate(&self) -> Result<()> {
        if self.agents.is_empty() {
            return Err(crate::error::OneAIError::Swarm(
                "Swarm must have at least 1 agent".to_string()
            ));
        }

        if self.budget.total_tokens == 0 {
            return Err(crate::error::OneAIError::Swarm(
                "Swarm budget must be > 0".to_string()
            ));
        }

        if self.quality_threshold < 0.0 || self.quality_threshold > 1.0 {
            return Err(crate::error::OneAIError::Swarm(
                format!("Quality threshold must be 0.0–1.0, got {}", self.quality_threshold)
            ));
        }

        // Check unique agent names
        let mut names = HashMap::new();
        for agent in &self.agents {
            if let Some(_prev) = names.insert(&agent.name, agent) {
                return Err(crate::error::OneAIError::Swarm(
                    format!("Duplicate agent name '{}' in swarm '{}'", agent.name, self.id)
                ));
            }
        }

        // Validate each agent's capability
        for agent in &self.agents {
            agent.capability.validate()?;
        }

        Ok(())
    }

    /// Get an agent by name.
    pub fn agent_by_name(&self, name: &str) -> Option<&SwarmAgentEntry> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// Get agent names.
    pub fn agent_names(&self) -> Vec<String> {
        self.agents.iter().map(|a| a.name.clone()).collect()
    }

    /// Get the number of agents.
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Get all categories covered by the swarm's agents.
    pub fn all_categories(&self) -> Vec<String> {
        let mut categories: Vec<String> = self.agents.iter()
            .flat_map(|a| a.capability.categories.clone())
            .collect();
        categories.sort();
        categories.dedup();
        categories
    }
}

// ─── SwarmAgentEntry ───────────────────────────────────────────────────────────

/// An agent entry in the swarm — combines name, capability, and agent kind.
///
/// Each entry maps an agent's capability profile to a SubAgentKind,
/// so the SwarmOrchestrator can both route tasks (using capabilities)
/// and create agents (using the factory + kind).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmAgentEntry {
    /// Agent name (unique within the swarm).
    pub name: String,

    /// The agent's capability profile.
    pub capability: AgentCapability,

    /// The sub-agent kind that implements this agent.
    #[serde(skip)]
    pub agent_kind: SubAgentKindProxy,

    /// System prompt override for this agent (optional).
    pub system_prompt_override: Option<String>,

    /// Current load — number of tasks currently assigned (for LoadBalanced routing).
    /// This is a runtime value, not serialized.
    #[serde(skip)]
    pub current_load: usize,
}

impl SwarmAgentEntry {
    /// Create a new swarm agent entry.
    pub fn new(name: &str, capability: AgentCapability, agent_kind: SubAgentKindProxy) -> Self {
        Self {
            name: name.to_string(),
            capability,
            agent_kind,
            system_prompt_override: None,
            current_load: 0,
        }
    }

    /// Create an agent entry for a coding agent.
    pub fn coder() -> Self {
        Self::new("coder",
            AgentCapability::single_category("code", 0.9, 0.7)
                .with_category("debug", 0.85, 0.75)
                .with_cost_per_1k(0.03),
            SubAgentKindProxy::code(),
        )
    }

    /// Create an agent entry for a research agent.
    pub fn researcher() -> Self {
        Self::new("researcher",
            AgentCapability::single_category("research", 0.85, 0.8)
                .with_category("analysis", 0.8, 0.7)
                .with_cost_per_1k(0.02),
            SubAgentKindProxy::explore(),
        )
    }

    /// Create an agent entry for a review agent.
    pub fn reviewer() -> Self {
        Self::new("reviewer",
            AgentCapability::single_category("review", 0.88, 0.85)
                .with_category("security", 0.82, 0.7)
                .with_cost_per_1k(0.025),
            SubAgentKindProxy::review(),
        )
    }

    /// Create an agent entry for a planning agent.
    pub fn planner() -> Self {
        Self::new("planner",
            AgentCapability::single_category("planning", 0.85, 0.9)
                .with_category("design", 0.8, 0.75)
                .with_cost_per_1k(0.02),
            SubAgentKindProxy::plan(),
        )
    }

    /// Set a system prompt override.
    pub fn with_system_prompt(mut self, prompt: &str) -> Self {
        self.system_prompt_override = Some(prompt.to_string());
        self
    }

    /// Set the current load (for LoadBalanced routing).
    pub fn with_current_load(mut self, load: usize) -> Self {
        self.current_load = load;
        self
    }
}

// ─── SwarmTask ──────────────────────────────────────────────────────────────────

/// Swarm task — a subtask within a swarm execution.
///
/// The SwarmOrchestrator decomposes a complex task into SwarmTasks,
/// each with:
/// - A unique ID
/// - A description (the subtask prompt)
/// - A category (for routing to the right agent)
/// - A priority (higher = more important)
/// - Dependencies (task IDs that must complete first)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmTask {
    /// Unique task ID.
    pub id: String,

    /// The task description (prompt for the agent).
    pub description: String,

    /// The category this task belongs to (e.g., "code", "research", "review").
    pub category: String,

    /// Priority (higher = more important, processed first).
    pub priority: f64,

    /// Dependencies (task IDs that must complete before this task can start).
    pub depends_on: Vec<String>,
}

impl SwarmTask {
    /// Create a new swarm task.
    pub fn new(id: &str, description: &str, category: &str) -> Self {
        Self {
            id: id.to_string(),
            description: description.to_string(),
            category: category.to_string(),
            priority: 1.0,
            depends_on: Vec::new(),
        }
    }

    /// Create a high-priority task.
    pub fn high_priority(id: &str, description: &str, category: &str) -> Self {
        Self {
            id: id.to_string(),
            description: description.to_string(),
            category: category.to_string(),
            priority: 3.0,
            depends_on: Vec::new(),
        }
    }

    /// Set the priority.
    pub fn with_priority(mut self, priority: f64) -> Self {
        self.priority = priority;
        self
    }

    /// Add a dependency.
    pub fn with_dependency(mut self, task_id: &str) -> Self {
        self.depends_on.push(task_id.to_string());
        self
    }

    /// Add multiple dependencies.
    pub fn with_dependencies(mut self, task_ids: &[&str]) -> Self {
        for id in task_ids {
            self.depends_on.push(id.to_string());
        }
        self
    }

    /// Check if this task has any dependencies.
    pub fn has_dependencies(&self) -> bool {
        !self.depends_on.is_empty()
    }

    /// Check if all dependencies are satisfied (all deps in the completed set).
    pub fn dependencies_satisfied(&self, completed_ids: &std::collections::HashSet<String>) -> bool {
        self.depends_on.iter().all(|id| completed_ids.contains(id))
    }
}

// ─── SwarmResult ────────────────────────────────────────────────────────────────

/// Swarm execution result.
///
/// Contains the swarm's final answer (aggregated from all task results),
/// individual task results, token usage, cost, and which agents participated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmResult {
    /// The swarm's final answer.
    pub final_answer: String,

    /// Individual task results.
    pub task_results: Vec<SwarmTaskResult>,

    /// Total tokens used.
    pub total_tokens: u32,

    /// Total cost.
    pub total_cost: f64,

    /// Agents that participated in the swarm execution.
    pub active_agents: Vec<String>,
}

impl SwarmResult {
    /// Create an empty swarm result.
    pub fn empty() -> Self {
        Self {
            final_answer: String::new(),
            task_results: Vec::new(),
            total_tokens: 0,
            total_cost: 0.0,
            active_agents: Vec::new(),
        }
    }

    /// Whether any task completed successfully.
    pub fn has_successful_results(&self) -> bool {
        self.task_results.iter().any(|r| r.completed)
    }

    /// Get successful task results.
    pub fn successful_results(&self) -> Vec<&SwarmTaskResult> {
        self.task_results.iter().filter(|r| r.completed).collect()
    }

    /// Get failed task results.
    pub fn failed_results(&self) -> Vec<&SwarmTaskResult> {
        self.task_results.iter().filter(|r| !r.completed).collect()
    }

    /// Get retried task results.
    pub fn retried_results(&self) -> Vec<&SwarmTaskResult> {
        self.task_results.iter().filter(|r| r.retry_count > 0).collect()
    }
}

// ─── SwarmTaskResult ────────────────────────────────────────────────────────────

/// Result of an individual swarm task.
///
/// Each task result records:
/// - The task ID and description
/// - The agent that handled it
/// - The result text
/// - Whether it completed successfully
/// - The quality score (estimated)
/// - Token usage and cost
/// - How many retries were attempted
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmTaskResult {
    /// The task ID.
    pub task_id: String,

    /// The task description.
    pub task_description: String,

    /// The task category.
    pub category: String,

    /// The agent name that handled this task.
    pub agent_name: String,

    /// The agent kind proxy.
    pub agent_kind: SubAgentKindProxy,

    /// The result text from this task.
    pub result_text: String,

    /// Key findings from this task.
    pub key_findings: Vec<String>,

    /// Whether this task completed successfully.
    pub completed: bool,

    /// Estimated quality score for the result (0.0–1.0).
    pub quality_score: f64,

    /// Tokens used by this task.
    pub tokens_used: u32,

    /// Cost incurred by this task.
    pub cost: f64,

    /// How many retries were attempted for this task.
    pub retry_count: usize,
}

// ─── SwarmCoordinationLog trait ──────────────────────────────────────────────────

/// Log trait for swarm coordination events.
///
/// Records swarm execution events for observability and debugging:
/// - When a swarm execution starts
/// - When each task is routed to an agent
/// - When a task starts/completes
/// - When a result is validated (accepted or rejected)
/// - When a task is retried with an alternative agent
/// - Budget and cost tracking
#[async_trait]
pub trait SwarmCoordinationLog: Send + Sync {
    /// Log a swarm execution start.
    async fn log_swarm_start(&self, swarm_id: &str, routing: SwarmRouting, task: &str);

    /// Log a task routing decision.
    async fn log_task_routed(&self, swarm_id: &str, task_id: &str, agent_name: &str, category: &str);

    /// Log a task start.
    async fn log_task_start(&self, swarm_id: &str, task_id: &str, agent_name: &str);

    /// Log a task completion.
    async fn log_task_complete(
        &self,
        swarm_id: &str,
        task_id: &str,
        agent_name: &str,
        tokens_used: u32,
        cost: f64,
        completed: bool,
        quality_score: f64,
    );

    /// Log a result validation (accepted or rejected).
    async fn log_result_validation(
        &self,
        swarm_id: &str,
        task_id: &str,
        quality_score: f64,
        threshold: f64,
        accepted: bool,
    );

    /// Log a task retry.
    async fn log_task_retry(
        &self,
        swarm_id: &str,
        task_id: &str,
        previous_agent: &str,
        new_agent: &str,
        retry_count: usize,
    );

    /// Log swarm execution completion.
    async fn log_swarm_complete(&self, swarm_id: &str, total_tokens: u32, total_cost: f64);

    /// Get recent swarm coordination events.
    async fn recent_events(&self, limit: usize) -> Vec<SwarmCoordinationEvent>;

    /// Get events for a specific swarm execution.
    async fn events_for_swarm(&self, swarm_id: &str) -> Vec<SwarmCoordinationEvent>;
}

// ─── SwarmCoordinationEvent ──────────────────────────────────────────────────────

/// An event in the swarm coordination log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCoordinationEvent {
    /// The swarm ID this event belongs to.
    pub swarm_id: String,

    /// The event kind.
    pub kind: SwarmCoordinationEventKind,

    /// The task ID involved (for task events).
    pub task_id: Option<String>,

    /// The agent name involved (for task events).
    pub agent_name: Option<String>,

    /// Timestamp of the event.
    pub timestamp: DateTime<Utc>,

    /// Additional details.
    pub details: HashMap<String, String>,
}

// ─── SwarmCoordinationEventKind ──────────────────────────────────────────────────

/// Kind of swarm coordination event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SwarmCoordinationEventKind {
    /// Swarm execution started.
    SwarmStart,
    /// A task was routed to an agent.
    TaskRouted,
    /// A task started executing.
    TaskStart,
    /// A task completed (success or failure).
    TaskComplete,
    /// A result was validated (accepted or rejected).
    ResultValidation,
    /// A task was retried with a different agent.
    TaskRetry,
    /// Swarm execution completed.
    SwarmComplete,
}

// ─── InMemorySwarmCoordinationLog ────────────────────────────────────────────────

/// In-memory implementation of SwarmCoordinationLog.
///
/// Stores events in a Vec protected by a RwLock.
/// Suitable for testing and single-session scenarios.
pub struct InMemorySwarmCoordinationLog {
    events: Arc<tokio::sync::RwLock<Vec<SwarmCoordinationEvent>>>,
}

impl InMemorySwarmCoordinationLog {
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

impl Default for InMemorySwarmCoordinationLog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SwarmCoordinationLog for InMemorySwarmCoordinationLog {
    async fn log_swarm_start(&self, swarm_id: &str, routing: SwarmRouting, task: &str) {
        let mut events = self.events.write().await;
        events.push(SwarmCoordinationEvent {
            swarm_id: swarm_id.to_string(),
            kind: SwarmCoordinationEventKind::SwarmStart,
            task_id: None,
            agent_name: None,
            timestamp: Utc::now(),
            details: HashMap::from([
                ("routing".to_string(), routing.name().to_string()),
                ("task".to_string(), task.to_string()),
            ]),
        });
    }

    async fn log_task_routed(&self, swarm_id: &str, task_id: &str, agent_name: &str, category: &str) {
        let mut events = self.events.write().await;
        events.push(SwarmCoordinationEvent {
            swarm_id: swarm_id.to_string(),
            kind: SwarmCoordinationEventKind::TaskRouted,
            task_id: Some(task_id.to_string()),
            agent_name: Some(agent_name.to_string()),
            timestamp: Utc::now(),
            details: HashMap::from([
                ("category".to_string(), category.to_string()),
            ]),
        });
    }

    async fn log_task_start(&self, swarm_id: &str, task_id: &str, agent_name: &str) {
        let mut events = self.events.write().await;
        events.push(SwarmCoordinationEvent {
            swarm_id: swarm_id.to_string(),
            kind: SwarmCoordinationEventKind::TaskStart,
            task_id: Some(task_id.to_string()),
            agent_name: Some(agent_name.to_string()),
            timestamp: Utc::now(),
            details: HashMap::new(),
        });
    }

    async fn log_task_complete(
        &self,
        swarm_id: &str,
        task_id: &str,
        agent_name: &str,
        tokens_used: u32,
        cost: f64,
        completed: bool,
        quality_score: f64,
    ) {
        let mut events = self.events.write().await;
        events.push(SwarmCoordinationEvent {
            swarm_id: swarm_id.to_string(),
            kind: SwarmCoordinationEventKind::TaskComplete,
            task_id: Some(task_id.to_string()),
            agent_name: Some(agent_name.to_string()),
            timestamp: Utc::now(),
            details: HashMap::from([
                ("tokens_used".to_string(), tokens_used.to_string()),
                ("cost".to_string(), format!("{:.4}", cost)),
                ("completed".to_string(), completed.to_string()),
                ("quality_score".to_string(), format!("{:.2}", quality_score)),
            ]),
        });
    }

    async fn log_result_validation(
        &self,
        swarm_id: &str,
        task_id: &str,
        quality_score: f64,
        threshold: f64,
        accepted: bool,
    ) {
        let mut events = self.events.write().await;
        events.push(SwarmCoordinationEvent {
            swarm_id: swarm_id.to_string(),
            kind: SwarmCoordinationEventKind::ResultValidation,
            task_id: Some(task_id.to_string()),
            agent_name: None,
            timestamp: Utc::now(),
            details: HashMap::from([
                ("quality_score".to_string(), format!("{:.2}", quality_score)),
                ("threshold".to_string(), format!("{:.2}", threshold)),
                ("accepted".to_string(), accepted.to_string()),
            ]),
        });
    }

    async fn log_task_retry(
        &self,
        swarm_id: &str,
        task_id: &str,
        previous_agent: &str,
        new_agent: &str,
        retry_count: usize,
    ) {
        let mut events = self.events.write().await;
        events.push(SwarmCoordinationEvent {
            swarm_id: swarm_id.to_string(),
            kind: SwarmCoordinationEventKind::TaskRetry,
            task_id: Some(task_id.to_string()),
            agent_name: Some(new_agent.to_string()),
            timestamp: Utc::now(),
            details: HashMap::from([
                ("previous_agent".to_string(), previous_agent.to_string()),
                ("retry_count".to_string(), retry_count.to_string()),
            ]),
        });
    }

    async fn log_swarm_complete(&self, swarm_id: &str, total_tokens: u32, total_cost: f64) {
        let mut events = self.events.write().await;
        events.push(SwarmCoordinationEvent {
            swarm_id: swarm_id.to_string(),
            kind: SwarmCoordinationEventKind::SwarmComplete,
            task_id: None,
            agent_name: None,
            timestamp: Utc::now(),
            details: HashMap::from([
                ("total_tokens".to_string(), total_tokens.to_string()),
                ("total_cost".to_string(), format!("{:.4}", total_cost)),
            ]),
        });
    }

    async fn recent_events(&self, limit: usize) -> Vec<SwarmCoordinationEvent> {
        let events = self.events.read().await;
        events.iter().rev().take(limit).cloned().collect()
    }

    async fn events_for_swarm(&self, swarm_id: &str) -> Vec<SwarmCoordinationEvent> {
        let events = self.events.read().await;
        events.iter()
            .filter(|e| e.swarm_id == swarm_id)
            .cloned()
            .collect()
    }
}

// ─── SwarmPresets ────────────────────────────────────────────────────────────────

/// Preset swarm configurations for common use cases.
///
/// These presets provide ready-to-use swarm configurations for
/// typical multi-agent scenarios. Each preset comes with appropriate
/// agents, routing strategy, and budget.
pub struct SwarmPresets;

impl SwarmPresets {
    /// Code analysis swarm — BestFit routing with 4 specialized agents.
    ///
    /// coder, researcher, reviewer, and planner each handle tasks
    /// based on their quality scores. Best routing for code analysis tasks.
    pub fn code_analysis_swarm() -> SwarmConfig {
        SwarmConfig::best_fit("code_analysis")
            .with_agent(SwarmAgentEntry::coder())
            .with_agent(SwarmAgentEntry::researcher())
            .with_agent(SwarmAgentEntry::reviewer())
            .with_agent(SwarmAgentEntry::planner())
            .with_quality_threshold(0.7)
            .with_max_retries(2)
            .with_budget(TokenBudgetProxy::new(120_000))
    }

    /// Fast research swarm — Fastest routing with 3 agents.
    ///
    /// Prioritizes speed over quality for time-sensitive research tasks.
    pub fn fast_research_swarm() -> SwarmConfig {
        SwarmConfig::fastest("fast_research")
            .with_agent(SwarmAgentEntry::researcher())
            .with_agent(SwarmAgentEntry::planner())
            .with_agent(SwarmAgentEntry::reviewer()
                .with_system_prompt("You are a fast review agent. Focus on key points only."))
            .with_quality_threshold(0.6)
            .with_max_retries(1)
            .with_budget(TokenBudgetProxy::new(60_000))
    }

    /// Budget code swarm — CostOptimized routing for cost-sensitive tasks.
    ///
    /// Routes to the cheapest agent that meets quality threshold.
    pub fn budget_code_swarm() -> SwarmConfig {
        SwarmConfig::cost_optimized("budget_code")
            .with_agent(SwarmAgentEntry::planner())  // Cheapest (0.02/1k)
            .with_agent(SwarmAgentEntry::researcher())  // Also cheap (0.02/1k)
            .with_agent(SwarmAgentEntry::reviewer())  // Moderate (0.025/1k)
            .with_agent(SwarmAgentEntry::coder())  // Most expensive (0.03/1k)
            .with_quality_threshold(0.75)
            .with_max_retries(2)
            .with_budget(TokenBudgetProxy::new(80_000))
    }

    /// Balanced dev swarm — LoadBalanced routing for general development.
    ///
    /// Distributes tasks across all agents, avoiding overloading any single agent.
    pub fn balanced_dev_swarm() -> SwarmConfig {
        SwarmConfig::load_balanced("balanced_dev")
            .with_agent(SwarmAgentEntry::coder()
                .with_system_prompt("You are a balanced development agent. Handle coding and debugging tasks."))
            .with_agent(SwarmAgentEntry::researcher()
                .with_system_prompt("You are a balanced research agent. Handle research and analysis tasks."))
            .with_agent(SwarmAgentEntry::reviewer()
                .with_system_prompt("You are a balanced review agent. Handle review and security tasks."))
            .with_agent(SwarmAgentEntry::planner()
                .with_system_prompt("You are a balanced planning agent. Handle planning and design tasks."))
            .with_quality_threshold(0.7)
            .with_max_retries(2)
            .with_budget(TokenBudgetProxy::new(100_000))
            .with_max_concurrent_tasks(4)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_agent_capability_creation() {
        let cap = AgentCapability::new();
        assert!(cap.categories.is_empty());
        assert!(cap.quality_scores.is_empty());
        assert_eq!(cap.max_concurrent, 1);
    }

    #[test]
    fn test_agent_capability_single_category() {
        let cap = AgentCapability::single_category("code", 0.9, 0.7);
        assert_eq!(cap.categories.len(), 1);
        assert_eq!(cap.quality_for("code"), 0.9);
        assert_eq!(cap.speed_for("code"), 0.7);
        assert!(cap.handles_category("code"));
        assert!(!cap.handles_category("research"));
    }

    #[test]
    fn test_agent_capability_with_category() {
        let cap = AgentCapability::single_category("code", 0.9, 0.7)
            .with_category("debug", 0.85, 0.75);
        assert_eq!(cap.categories.len(), 2);
        assert_eq!(cap.quality_for("code"), 0.9);
        assert_eq!(cap.quality_for("debug"), 0.85);
    }

    #[test]
    fn test_agent_capability_validate_empty() {
        let cap = AgentCapability::new();
        let result = cap.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least 1 category"));
    }

    #[test]
    fn test_agent_capability_validate_score_range() {
        let cap = AgentCapability::single_category("code", 1.5, 0.7);
        let result = cap.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("0.0–1.0"));
    }

    #[test]
    fn test_agent_capability_validate_valid() {
        let cap = AgentCapability::single_category("code", 0.9, 0.7)
            .with_max_concurrent(2);
        assert!(cap.validate().is_ok());
    }

    #[test]
    fn test_agent_capability_best_match_for_task() {
        let cap = AgentCapability::single_category("code", 0.9, 0.7)
            .with_category("research", 0.85, 0.8);
        // Task mentions "code" keyword → should match "code"
        assert_eq!(cap.best_match_for_task("implement a new code feature"), Some("code".to_string()));
        // Task mentions both "research" and "code" keywords → should match "code" (highest quality)
        assert_eq!(cap.best_match_for_task("research the codebase"), Some("code".to_string()));
        // Task mentions "research" keyword but NOT "code" → should match "research"
        assert_eq!(cap.best_match_for_task("investigate and research this topic"), Some("research".to_string()));
        // No keyword match → fallback to highest quality category ("code" at 0.9)
        assert_eq!(cap.best_match_for_task("do something general"), Some("code".to_string()));
    }

    #[test]
    fn test_swarm_routing_names() {
        assert_eq!(SwarmRouting::BestFit.name(), "best-fit");
        assert_eq!(SwarmRouting::LoadBalanced.name(), "load-balanced");
        assert_eq!(SwarmRouting::CostOptimized.name(), "cost-optimized");
        assert_eq!(SwarmRouting::Fastest.name(), "fastest");
    }

    #[test]
    fn test_swarm_routing_from_str() {
        assert_eq!(SwarmRouting::from_str_opt("best-fit"), Some(SwarmRouting::BestFit));
        assert_eq!(SwarmRouting::from_str_opt("best"), Some(SwarmRouting::BestFit));
        assert_eq!(SwarmRouting::from_str_opt("load"), Some(SwarmRouting::LoadBalanced));
        assert_eq!(SwarmRouting::from_str_opt("cost"), Some(SwarmRouting::CostOptimized));
        assert_eq!(SwarmRouting::from_str_opt("fast"), Some(SwarmRouting::Fastest));
        assert_eq!(SwarmRouting::from_str_opt("unknown"), None);
    }

    #[test]
    fn test_swarm_routing_all() {
        assert_eq!(SwarmRouting::all().len(), 4);
    }

    #[test]
    fn test_swarm_routing_display() {
        assert_eq!(format!("{}", SwarmRouting::BestFit), "best-fit");
    }

    #[test]
    fn test_swarm_routing_serialization() {
        for routing in SwarmRouting::all() {
            let json = serde_json::to_string(&routing).unwrap();
            let parsed: SwarmRouting = serde_json::from_str(&json).unwrap();
            assert_eq!(routing, parsed);
        }
    }

    #[test]
    fn test_swarm_config_creation() {
        let config = SwarmConfig::new("test_swarm", SwarmRouting::BestFit);
        assert_eq!(config.id, "test_swarm");
        assert!(config.agents.is_empty());
        assert_eq!(config.routing, SwarmRouting::BestFit);
        assert_eq!(config.quality_threshold, 0.7);
        assert_eq!(config.max_retries, 2);
    }

    #[test]
    fn test_swarm_config_with_agent() {
        let config = SwarmConfig::best_fit("test")
            .with_agent(SwarmAgentEntry::coder());
        assert_eq!(config.agent_count(), 1);
        assert_eq!(config.agent_names(), vec!["coder"]);
    }

    #[test]
    fn test_swarm_config_validate_empty_agents() {
        let config = SwarmConfig::best_fit("bad_swarm");
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("at least 1 agent"));
    }

    #[test]
    fn test_swarm_config_validate_zero_budget() {
        let config = SwarmConfig::best_fit("bad_swarm")
            .with_agent(SwarmAgentEntry::coder())
            .with_budget(TokenBudgetProxy::new(0));
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("budget must be > 0"));
    }

    #[test]
    fn test_swarm_config_validate_duplicate_names() {
        let config = SwarmConfig::best_fit("dup_swarm")
            .with_agent(SwarmAgentEntry::coder())
            .with_agent(SwarmAgentEntry::coder()); // Same name
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Duplicate agent name"));
    }

    #[test]
    fn test_swarm_config_validate_valid() {
        let config = SwarmPresets::code_analysis_swarm();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_swarm_config_all_categories() {
        let config = SwarmPresets::code_analysis_swarm();
        let categories = config.all_categories();
        assert!(categories.contains(&"code".to_string()));
        assert!(categories.contains(&"research".to_string()));
    }

    #[test]
    fn test_swarm_agent_entry_builtins() {
        let coder = SwarmAgentEntry::coder();
        assert_eq!(coder.name, "coder");
        assert!(coder.capability.handles_category("code"));

        let researcher = SwarmAgentEntry::researcher();
        assert_eq!(researcher.name, "researcher");
        assert!(researcher.capability.handles_category("research"));
    }

    #[test]
    fn test_swarm_task_creation() {
        let task = SwarmTask::new("t1", "Implement feature X", "code");
        assert_eq!(task.id, "t1");
        assert_eq!(task.category, "code");
        assert_eq!(task.priority, 1.0);
        assert!(!task.has_dependencies());
    }

    #[test]
    fn test_swarm_task_with_dependencies() {
        let task = SwarmTask::new("t2", "Review code", "review")
            .with_dependency("t1");
        assert!(task.has_dependencies());
        assert_eq!(task.depends_on.len(), 1);

        let completed: HashSet<String> = HashSet::from(["t1".to_string()]);
        assert!(task.dependencies_satisfied(&completed));

        let not_completed: HashSet<String> = HashSet::new();
        assert!(!task.dependencies_satisfied(&not_completed));
    }

    #[test]
    fn test_swarm_result_empty() {
        let result = SwarmResult::empty();
        assert!(result.final_answer.is_empty());
        assert!(result.task_results.is_empty());
        assert!(!result.has_successful_results());
    }

    #[test]
    fn test_swarm_result_serialization() {
        let result = SwarmResult {
            final_answer: "Analysis complete".into(),
            task_results: vec![SwarmTaskResult {
                task_id: "t1".into(),
                task_description: "Code review".into(),
                category: "code".into(),
                agent_name: "coder".into(),
                agent_kind: SubAgentKindProxy::code(),
                result_text: "Found 3 issues".into(),
                key_findings: vec!["Issue A".into()],
                completed: true,
                quality_score: 0.85,
                tokens_used: 3000,
                cost: 0.03,
                retry_count: 0,
            }],
            total_tokens: 3000,
            total_cost: 0.03,
            active_agents: vec!["coder".to_string()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SwarmResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.final_answer, parsed.final_answer);
        assert_eq!(result.total_tokens, parsed.total_tokens);
    }

    #[tokio::test]
    async fn test_in_memory_swarm_coordination_log() {
        let log = InMemorySwarmCoordinationLog::new();

        log.log_swarm_start("test_swarm", SwarmRouting::BestFit, "Analyze codebase").await;
        log.log_task_routed("test_swarm", "t1", "coder", "code").await;
        log.log_task_start("test_swarm", "t1", "coder").await;
        log.log_task_complete("test_swarm", "t1", "coder", 3000, 0.03, true, 0.85).await;
        log.log_result_validation("test_swarm", "t1", 0.85, 0.7, true).await;
        log.log_swarm_complete("test_swarm", 3000, 0.03).await;

        assert_eq!(log.event_count().await, 6);

        let events = log.events_for_swarm("test_swarm").await;
        assert_eq!(events.len(), 6);

        let recent = log.recent_events(3).await;
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn test_swarm_coordination_event_kind() {
        assert_eq!(SwarmCoordinationEventKind::SwarmStart, SwarmCoordinationEventKind::SwarmStart);
        assert_ne!(SwarmCoordinationEventKind::TaskRouted, SwarmCoordinationEventKind::TaskComplete);
    }

    #[test]
    fn test_presets_code_analysis() {
        let config = SwarmPresets::code_analysis_swarm();
        assert_eq!(config.id, "code_analysis");
        assert_eq!(config.routing, SwarmRouting::BestFit);
        assert_eq!(config.agent_count(), 4);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_presets_fast_research() {
        let config = SwarmPresets::fast_research_swarm();
        assert_eq!(config.id, "fast_research");
        assert_eq!(config.routing, SwarmRouting::Fastest);
        assert_eq!(config.agent_count(), 3);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_presets_budget_code() {
        let config = SwarmPresets::budget_code_swarm();
        assert_eq!(config.id, "budget_code");
        assert_eq!(config.routing, SwarmRouting::CostOptimized);
        assert_eq!(config.agent_count(), 4);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_presets_balanced_dev() {
        let config = SwarmPresets::balanced_dev_swarm();
        assert_eq!(config.id, "balanced_dev");
        assert_eq!(config.routing, SwarmRouting::LoadBalanced);
        assert_eq!(config.agent_count(), 4);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_swarm_config_quality_threshold_range() {
        let config = SwarmConfig::best_fit("bad_threshold")
            .with_agent(SwarmAgentEntry::coder())
            .with_quality_threshold(1.5);
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("0.0–1.0"));
    }

    #[test]
    fn test_swarm_task_result_serialization() {
        let result = SwarmTaskResult {
            task_id: "t1".into(),
            task_description: "Review".into(),
            category: "code".into(),
            agent_name: "coder".into(),
            agent_kind: SubAgentKindProxy::code(),
            result_text: "Found issues".into(),
            key_findings: vec!["Issue 1".into()],
            completed: true,
            quality_score: 0.85,
            tokens_used: 3000,
            cost: 0.03,
            retry_count: 0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SwarmTaskResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.task_id, parsed.task_id);
        assert_eq!(result.quality_score, parsed.quality_score);
    }

    #[test]
    fn test_swarm_result_successful_failed() {
        let result = SwarmResult {
            final_answer: "Test".into(),
            task_results: vec![
                SwarmTaskResult {
                    task_id: "t1".into(),
                    task_description: "Task 1".into(),
                    category: "code".into(),
                    agent_name: "coder".into(),
                    agent_kind: SubAgentKindProxy::code(),
                    result_text: "Success".into(),
                    key_findings: vec![],
                    completed: true,
                    quality_score: 0.85,
                    tokens_used: 1000,
                    cost: 0.01,
                    retry_count: 0,
                },
                SwarmTaskResult {
                    task_id: "t2".into(),
                    task_description: "Task 2".into(),
                    category: "research".into(),
                    agent_name: "researcher".into(),
                    agent_kind: SubAgentKindProxy::explore(),
                    result_text: "Failed".into(),
                    key_findings: vec![],
                    completed: false,
                    quality_score: 0.3,
                    tokens_used: 500,
                    cost: 0.005,
                    retry_count: 1,
                },
            ],
            total_tokens: 1500,
            total_cost: 0.015,
            active_agents: vec!["coder".to_string(), "researcher".to_string()],
        };
        assert!(result.has_successful_results());
        assert_eq!(result.successful_results().len(), 1);
        assert_eq!(result.failed_results().len(), 1);
        assert_eq!(result.retried_results().len(), 1);
    }

    #[test]
    fn test_agent_capability_zero_max_concurrent() {
        let cap = AgentCapability::single_category("code", 0.9, 0.7)
            .with_max_concurrent(0);
        let result = cap.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max_concurrent"));
    }
}
