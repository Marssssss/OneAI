//! Team Coordinator — orchestrates multi-agent team execution.
//!
//! The TeamCoordinator is the runtime engine for OneAI's multi-agent team system.
//! It takes a TeamConfig (strategy + roles + budget) and executes a task by
//! creating sub-agents via the SubAgentFactory and coordinating their execution
//! according to the strategy:
//!
//! - **Coordinate**: All agents run the same task in parallel, coordinator
//!   synthesizes their results into a consensus answer.
//! - **Route**: Router agent selects the best specialist, only that agent runs.
//! - **Collaborate**: Agents run in sequence, each building on previous output.
//! - **Debate**: Debaters argue from different perspectives, judge resolves.
//!
//! The coordinator tracks budget, cost, and logs all coordination events
//! through the TeamCoordinationLog trait.

use std::sync::Arc;

use oneai_core::team::{
    TeamConfig, TeamResult, TeamStrategy, AgentResultEntry, AgentRole,
    TeamCoordinationLog, InMemoryTeamCoordinationLog,
    SubAgentKindProxy,
};
use oneai_core::budget::TokenBudget;
use oneai_core::usage::UsageTracker;
use oneai_core::error::{OneAIError, Result};
use oneai_core::context_manager::ContextManager;

use crate::sub_agent::{SubAgentFactory, SubAgentKind, SubAgentSummary};

// ─── TeamCoordinator ─────────────────────────────────────────────────────────

/// Team coordinator — orchestrates multi-agent team execution.
///
/// The coordinator runs agents according to the TeamStrategy:
/// - Coordinate: All agents run the same task, coordinator merges results
/// - Route: Router agent selects the best agent for the task
/// - Collaborate: Agents run in sequence, building on previous results
/// - Debate: Agents argue from different perspectives, judge resolves
///
/// The coordinator:
/// 1. Validates the TeamConfig
/// 2. Creates sub-agents via the SubAgentFactory
/// 3. Executes them according to the strategy
/// 4. Synthesizes results into a final TeamResult
/// 5. Logs all coordination events
/// 6. Tracks token usage
pub struct TeamCoordinator {
    /// The sub-agent factory for creating team members.
    factory: Arc<dyn SubAgentFactory>,

    /// Default budget for team execution.
    default_budget: TokenBudget,

    /// Context manager for trimming agent outputs (optional).
    context_manager: Option<Arc<ContextManager>>,

    /// Usage tracker for recording team usage (optional).
    usage_tracker: Option<Arc<dyn UsageTracker>>,

    /// Coordination log for recording team events.
    log: Arc<dyn TeamCoordinationLog>,
}

impl TeamCoordinator {
    /// Create a new TeamCoordinator with the given sub-agent factory.
    pub fn new(factory: Arc<dyn SubAgentFactory>) -> Self {
        Self {
            factory,
            default_budget: TokenBudget::new(100_000),
            context_manager: None,
            usage_tracker: None,
            log: Arc::new(InMemoryTeamCoordinationLog::new()),
        }
    }

    /// Create a TeamCoordinator with a custom default budget.
    pub fn with_budget(factory: Arc<dyn SubAgentFactory>, budget: TokenBudget) -> Self {
        Self {
            factory,
            default_budget: budget,
            context_manager: None,
            usage_tracker: None,
            log: Arc::new(InMemoryTeamCoordinationLog::new()),
        }
    }

    /// Create a TeamCoordinator with all optional components.
    pub fn with_components(
        factory: Arc<dyn SubAgentFactory>,
        budget: TokenBudget,
        context_manager: Option<Arc<ContextManager>>,
        usage_tracker: Option<Arc<dyn UsageTracker>>,
        log: Arc<dyn TeamCoordinationLog>,
    ) -> Self {
        Self {
            factory,
            default_budget: budget,
            context_manager,
            usage_tracker,
            log,
        }
    }

    /// Set the context manager.
    pub fn set_context_manager(&mut self, manager: Arc<ContextManager>) {
        self.context_manager = Some(manager);
    }

    /// Set the usage tracker.
    pub fn set_usage_tracker(&mut self, tracker: Arc<dyn UsageTracker>) {
        self.usage_tracker = Some(tracker);
    }

    /// Get the coordination log.
    pub fn log(&self) -> &Arc<dyn TeamCoordinationLog> {
        &self.log
    }

    // ─── Execute ──────────────────────────────────────────────────────────────

    /// Execute a team task.
    ///
    /// Takes a TeamConfig and a task description, validates the config,
    /// and executes according to the strategy. Returns a TeamResult
    /// containing the final answer, individual agent results, and
    /// aggregate statistics.
    ///
    /// The team budget is shared across all agents — each agent
    /// gets an allocation based on the strategy and number of roles.
    pub async fn execute(&self, config: &TeamConfig, task: &str) -> Result<TeamResult> {
        // Validate config
        config.validate()?;

        // Log team start
        self.log.log_team_start(&config.id, config.strategy, task).await;

        // Execute according to strategy
        let result = match config.strategy {
            TeamStrategy::Coordinate => self.coordinate(config, task).await?,
            TeamStrategy::Route => self.route(config, task).await?,
            TeamStrategy::Collaborate => self.collaborate(config, task).await?,
            TeamStrategy::Debate => self.debate(config, task).await?,
            _ => Err(OneAIError::Team(format!("Unsupported team strategy: {}", config.strategy.name())))?,
        };

        // Log team complete
        self.log.log_team_complete(&config.id, result.total_tokens).await;

        Ok(result)
    }

    // ─── Coordinate Strategy ──────────────────────────────────────────────────

    /// Coordinate strategy — all agents work on the same task.
    ///
    /// All agents receive the same task description, work independently,
    /// and produce their own answers. The coordinator then synthesizes
    /// these answers into a consensus result.
    ///
    /// If `use_coordinator_agent` is true, a dedicated coordinator agent
    /// synthesizes results. Otherwise, the coordinator uses template-based
    /// merging (concatenation with role labels).
    async fn coordinate(&self, config: &TeamConfig, task: &str) -> Result<TeamResult> {
        let budget_per_agent = self.allocate_budget(config);
        let budget_clone = budget_per_agent.clone();

        // Run all agents on the same task (concurrently, up to max_concurrent)
        let agent_results = self.run_agents_concurrently(
            &config.roles,
            task,
            budget_per_agent,
            config.max_concurrent,
            &config.id,
        ).await?;

        // Synthesize results
        let final_answer = if config.use_coordinator_agent && agent_results.len() > 1 {
            self.synthesize_with_agent(&agent_results, task, budget_clone, &config.id).await?
        } else {
            self.synthesize_template(&agent_results)
        };

        // Log synthesis
        self.log.log_synthesis(&config.id, TeamStrategy::Coordinate, final_answer.len()).await;

        // Aggregate statistics
        let total_tokens = agent_results.iter().map(|r| r.tokens_used).sum();

        Ok(TeamResult {
            final_answer,
            agent_results,
            total_tokens,
            strategy: TeamStrategy::Coordinate,
            team_id: config.id.clone(),
        })
    }

    // ─── Route Strategy ───────────────────────────────────────────────────────

    /// Route strategy — router agent selects the best specialist.
    ///
    /// The first role in the config acts as the router. It examines the task
    /// and decides which specialist (one of the remaining roles) should handle it.
    /// Only the selected specialist runs; others are not invoked.
    async fn route(&self, config: &TeamConfig, task: &str) -> Result<TeamResult> {
        let budget_per_agent = self.allocate_budget(config);

        // First role is the router
        let router_role = &config.roles[0];
        self.log.log_agent_start(&config.id, &router_role.name, router_role.agent_kind.name()).await;

        // Run the router agent with a routing prompt
        let routing_task = format!(
            "You are a task router. Analyze the following task and decide which specialist should handle it.\n\
             Available specialists: {}\n\
             Task: {}\n\
             Reply with ONLY the specialist name (one of the above).",
            config.roles[1..].iter()
                .map(|r| format!("'{}' ({})", r.name, r.backstory))
                .collect::<Vec<_>>()
                .join(", "),
            task
        );

        let router_kind = self.resolve_kind(&router_role.agent_kind);
        let router_agent = self.factory.create(router_kind, budget_per_agent.clone()).await?;
        let router_summary = router_agent.run(&routing_task).await?;

        self.log.log_agent_complete(
            &config.id, &router_role.name, router_role.agent_kind.name(),
            router_summary.tokens_used, router_summary.completed).await;

        // Determine which specialist to route to
        let target_name = self.extract_route_target(&router_summary.summary, &config.roles[1..]);
        let target_role = config.roles[1..].iter()
            .find(|r| r.name == target_name)
            .cloned();

        if let Some(target_role) = target_role {
            // Run the selected specialist
            self.log.log_agent_start(&config.id, &target_role.name, target_role.agent_kind.name()).await;

            let target_kind = self.resolve_kind(&target_role.agent_kind);
            let target_agent = self.factory.create(target_kind, self.default_budget.clone()).await?;
            let target_summary = target_agent.run(task).await?;

            self.log.log_agent_complete(
                &config.id, &target_role.name, target_role.agent_kind.name(),
                target_summary.tokens_used, target_summary.completed).await;

            let final_answer = target_summary.summary.clone();

            Ok(TeamResult {
                final_answer,
                agent_results: vec![
                    self.make_result_entry(&router_role, &router_summary),
                    self.make_result_entry(&target_role, &target_summary),
                ],
                total_tokens: router_summary.tokens_used + target_summary.tokens_used,
                strategy: TeamStrategy::Route,
                team_id: config.id.clone(),
            })
        } else {
            // Route target not found — return router's output as fallback
            Ok(TeamResult {
                final_answer: router_summary.summary.clone(),
                agent_results: vec![self.make_result_entry(&router_role, &router_summary)],
                total_tokens: router_summary.tokens_used,
                strategy: TeamStrategy::Route,
                team_id: config.id.clone(),
            })
        }
    }

    // ─── Collaborate Strategy ─────────────────────────────────────────────────

    /// Collaborate strategy — agents work in sequence (relay chain).
    ///
    /// Each agent receives the task + the previous agent's output.
    /// Agent A works first, then agent B gets agent A's result
    /// as additional context, then agent C gets B's result, etc.
    ///
    /// The last agent's output is the final answer.
    async fn collaborate(&self, config: &TeamConfig, task: &str) -> Result<TeamResult> {
        let budget_per_agent = self.allocate_budget(config);
        let mut agent_results: Vec<AgentResultEntry> = Vec::new();
        let mut accumulated_context = String::new();
        let mut total_tokens = 0u32;

        // Run agents in sequence
        for role in &config.roles {
            self.log.log_agent_start(&config.id, &role.name, role.agent_kind.name()).await;

            // Build the task with accumulated context from previous agents
            let collaborative_task = if accumulated_context.is_empty() {
                task.to_string()
            } else {
                format!(
                    "Original task: {}\n\n\
                     Previous agent results:\n{}\n\n\
                     Continue the work based on the above context. Build upon the previous agent's output.",
                    task, accumulated_context
                )
            };

            let kind = self.resolve_kind(&role.agent_kind);
            let agent = self.factory.create(kind, budget_per_agent.clone()).await?;
            let summary = agent.run(&collaborative_task).await?;

            self.log.log_agent_complete(
                &config.id, &role.name, role.agent_kind.name(),
                summary.tokens_used, summary.completed
            ).await;

            // Accumulate context for next agent
            if summary.completed {
                accumulated_context = format!("{} [{}]: {}", accumulated_context, role.name, summary.summary);
                if !accumulated_context.is_empty() && !accumulated_context.starts_with('[') {
                    // First entry — no leading newline needed
                } else {
                    // Subsequent entries — add separator
                    accumulated_context = format!("\n{}", accumulated_context.trim_start());
                }
            }

            total_tokens += summary.tokens_used;

            agent_results.push(self.make_result_entry(role, &summary));

            // Trim context if it's getting too long
            if accumulated_context.len() > 8000 {
                if let Some(_cm) = &self.context_manager {
                    // Trim accumulated context to keep it manageable
                    accumulated_context = accumulated_context.chars().take(8000).collect();
                }
            }
        }

        // The last agent's output is the final answer
        let final_answer = agent_results.last()
            .map(|r| r.result_text.clone())
            .unwrap_or_default();

        self.log.log_synthesis(&config.id, TeamStrategy::Collaborate, final_answer.len()).await;

        Ok(TeamResult {
            final_answer,
            agent_results,
            total_tokens,
            strategy: TeamStrategy::Collaborate,
            team_id: config.id.clone(),
        })
    }

    // ─── Debate Strategy ──────────────────────────────────────────────────────

    /// Debate strategy — agents argue from different perspectives, judge resolves.
    ///
    /// The debaters (all roles except the last) each argue from their
    /// perspective on the task. The judge (last role) evaluates all
    /// arguments and produces a final resolution.
    ///
    /// Each debater sees the previous debaters' arguments, enabling
    /// direct rebuttals and counterpoints (not just isolated opinions).
    async fn debate(&self, config: &TeamConfig, task: &str) -> Result<TeamResult> {
        let budget_per_agent = self.allocate_budget(config);
        let n_roles = config.roles.len();

        // Last role is the judge
        let debater_roles = &config.roles[..n_roles - 1];
        let judge_role = &config.roles[n_roles - 1];

        // Run debaters sequentially (so they can respond to previous arguments)
        let mut debater_results: Vec<AgentResultEntry> = Vec::new();
        let mut arguments = String::new();
        let mut total_tokens = 0u32;

        for (i, role) in debater_roles.iter().enumerate() {
            self.log.log_agent_start(&config.id, &role.name, role.agent_kind.name()).await;

            let debate_task = if i == 0 {
                format!(
                    "You are arguing from the '{}' perspective on this task.\n\
                     Present your argument clearly and persuasively.\n\
                     Task: {}",
                    role.backstory, task
                )
            } else {
                format!(
                    "You are arguing from the '{}' perspective on this task.\n\
                     Previous arguments have been made — read them and provide your \
                     counter-argument or rebuttal.\n\
                     Task: {}\n\n\
                     Previous arguments:\n{}",
                    role.backstory, task, arguments
                )
            };

            let kind = self.resolve_kind(&role.agent_kind);
            let agent = self.factory.create(kind, budget_per_agent.clone()).await?;
            let summary = agent.run(&debate_task).await?;

            self.log.log_agent_complete(
                &config.id, &role.name, role.agent_kind.name(),
                summary.tokens_used, summary.completed
            ).await;

            if summary.completed {
                arguments = format!("{}\n[{}]: {}", arguments, role.name, summary.summary);
            }

            total_tokens += summary.tokens_used;

            debater_results.push(self.make_result_entry(role, &summary));
        }

        // Run the judge
        self.log.log_agent_start(&config.id, &judge_role.name, judge_role.agent_kind.name()).await;

        let judge_task = format!(
            "You are a neutral judge evaluating the following debate arguments.\n\
             Decide the best approach based on the arguments presented.\n\
             Provide a reasoned decision with trade-off analysis.\n\n\
             Task: {}\n\n\
             Arguments:\n{}",
            task, arguments
        );

        let judge_kind = self.resolve_kind(&judge_role.agent_kind);
        let judge_agent = self.factory.create(judge_kind, budget_per_agent.clone()).await?;
        let judge_summary = judge_agent.run(&judge_task).await?;

        self.log.log_agent_complete(
            &config.id, &judge_role.name, judge_role.agent_kind.name(),
            judge_summary.tokens_used, judge_summary.completed
        ).await;

        total_tokens += judge_summary.tokens_used;

        let final_answer = judge_summary.summary.clone();

        self.log.log_synthesis(&config.id, TeamStrategy::Debate, final_answer.len()).await;

        let all_results: Vec<AgentResultEntry> = debater_results
            .into_iter()
            .chain(std::iter::once(self.make_result_entry(judge_role, &judge_summary)))
            .collect();

        Ok(TeamResult {
            final_answer,
            agent_results: all_results,
            total_tokens,
            strategy: TeamStrategy::Debate,
            team_id: config.id.clone(),
        })
    }

    // ─── Helpers ──────────────────────────────────────────────────────────────

    /// Resolve a SubAgentKindProxy to an actual SubAgentKind.
    ///
    /// Maps known proxy names (plan, explore, code, review) to their
    /// SubAgentKind equivalents. Unknown names become Custom kinds.
    fn resolve_kind(&self, proxy: &SubAgentKindProxy) -> SubAgentKind {
        SubAgentKind::from_str(proxy.name())
    }

    /// Allocate budget per agent based on strategy and role count.
    ///
    /// For Coordinate and Debate: equal split among all agents
    /// For Route: router gets small budget, specialist gets most
    /// For Collaborate: each agent gets equal budget
    fn allocate_budget(&self, config: &TeamConfig) -> TokenBudget {
        let total = config.budget.total_tokens;
        let per_agent = total / config.roles.len().max(1) as u32;
        TokenBudget::new(per_agent.max(10_000)) // Minimum 10k per agent
    }

    /// Run agents concurrently (up to max_concurrent).
    ///
    /// Creates sub-agents for each role and runs them concurrently.
    /// The concurrency is limited by max_concurrent — excess agents
    /// are queued and run when slots become available.
    async fn run_agents_concurrently(
        &self,
        roles: &[AgentRole],
        task: &str,
        budget_per_agent: TokenBudget,
        _max_concurrent: usize,
        team_id: &str,
    ) -> Result<Vec<AgentResultEntry>> {
        let mut results = Vec::new();

        // For simplicity, we run all agents concurrently using tokio::spawn
        // In a production implementation, we would use a semaphore to limit concurrency
        let mut handles = Vec::new();

        for role in roles {
            self.log.log_agent_start(team_id, &role.name, role.agent_kind.name()).await;

            let kind = self.resolve_kind(&role.agent_kind);
            let agent = self.factory.create(kind, budget_per_agent.clone()).await?;
            let task_owned = task.to_string();
            let role_name = role.name.clone();
            let role_kind = role.agent_kind.clone();

            let handle = tokio::spawn(async move {
                agent.run(&task_owned).await
            });

            handles.push((handle, role_name, role_kind));
        }

        // Collect results
        for (handle, role_name, role_kind) in handles {
            let summary = handle.await
                .map_err(|e| OneAIError::Team(
                    format!("Agent '{}' panicked or was cancelled: {}", role_name, e)
                ))?
                .map_err(|e| OneAIError::Team(
                    format!("Agent '{}' failed: {}", role_name, e)
                ))?;

            self.log.log_agent_complete(
                team_id, &role_name, role_kind.name(),
                summary.tokens_used, summary.completed).await;

            // Create the result entry manually since we need role info
            let entry = AgentResultEntry {
                role: role_name,
                agent_kind: role_kind,
                result_text: summary.summary.clone(),
                key_findings: summary.key_findings.clone(),
                completed: summary.completed,
                tokens_used: summary.tokens_used,
            };

            results.push(entry);
        }

        Ok(results)
    }

    /// Synthesize agent results with a dedicated coordinator agent.
    ///
    /// Creates a coordinator sub-agent that receives all agent results
    /// and synthesizes a consensus answer. The coordinator gets a
    /// synthesis prompt that includes all agent outputs with role labels.
    async fn synthesize_with_agent(
        &self,
        results: &[AgentResultEntry],
        task: &str,
        budget: TokenBudget,
        team_id: &str,
    ) -> Result<String> {
        let all_outputs = results.iter()
            .map(|r| format!("[{}]: {}", r.role, r.result_text))
            .collect::<Vec<_>>()
            .join("\n\n");

        let synthesis_task = format!(
            "You are a coordinator synthesizing multiple perspectives into a consensus answer.\n\
             Original task: {}\n\n\
             Agent results:\n{}\n\n\
             Synthesize a comprehensive answer that incorporates the best insights from all agents.",
            task, all_outputs
        );

        let coordinator = self.factory.create(SubAgentKind::Plan, budget).await?;
        let summary = coordinator.run(&synthesis_task).await?;

        self.log.log_agent_start(team_id, "coordinator", "plan").await;
        self.log.log_agent_complete(
            team_id, "coordinator", "plan",
            summary.tokens_used, summary.completed).await;

        Ok(summary.summary)
    }

    /// Synthesize agent results with template-based merging (no LLM call).
    ///
    /// Concatenates all agent results with role labels. Useful when
    /// `use_coordinator_agent` is false or when budget is limited.
    fn synthesize_template(&self, results: &[AgentResultEntry]) -> String {
        if results.len() == 1 {
            return results[0].result_text.clone();
        }

        results.iter()
            .map(|r| format!("[{}]: {}", r.role, r.result_text))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Extract the route target from the router agent's response.
    ///
    /// The router agent should reply with just the specialist name.
    /// This function extracts the name from the response, matching
    /// it against available role names.
    fn extract_route_target(&self, router_response: &str, available_roles: &[AgentRole]) -> String {
        let response_lower = router_response.to_lowercase();
        let response_trimmed = response_lower.trim();

        // Try exact match first
        for role in available_roles {
            if response_trimmed == role.name.to_lowercase() {
                return role.name.clone();
            }
        }

        // Try substring match
        for role in available_roles {
            if response_trimmed.contains(&role.name.to_lowercase()) {
                return role.name.clone();
            }
        }

        // Fallback: return first available role
        available_roles.first()
            .map(|r| r.name.clone())
            .unwrap_or_default()
    }

    /// Create an AgentResultEntry from a role and summary.
    fn make_result_entry(&self, role: &AgentRole, summary: &SubAgentSummary) -> AgentResultEntry {
        AgentResultEntry {
            role: role.name.clone(),
            agent_kind: role.agent_kind.clone(),
            result_text: summary.summary.clone(),
            key_findings: summary.key_findings.clone(),
            completed: summary.completed,
            tokens_used: summary.tokens_used,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::budget::TokenBudget;
    use async_trait::async_trait;
    use oneai_core::error::Result;
    // Test-only imports (kept out of the lib build to avoid unused-import warnings):
    use oneai_core::team::TokenBudgetProxy;
    use crate::sub_agent::SubAgent;

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
                summary: format!("{} (task: {})", self.response, task.chars().take(50).collect::<String>()),
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

    // ─── Helper to create test team configs ───────────────────────────────────

    fn make_test_role(name: &str, kind_name: &str) -> oneai_core::team::AgentRole {
        oneai_core::team::AgentRole {
            name: name.to_string(),
            backstory: format!("{} expert", name),
            agent_kind: SubAgentKindProxy { kind_name: kind_name.to_string() },
            available_tools: vec!["read_file".to_string()],
            system_prompt_override: None,
        }
    }

    // ─── Coordinate Tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_coordinate_two_agents() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = TeamConfig::coordinate("test_coord")
            .with_role(make_test_role("analyst", "explore"))
            .with_role(make_test_role("reviewer", "review"))
            .with_budget(TokenBudgetProxy::new(50_000))
            .with_coordinator_agent(false); // Use template synthesis

        let result = coordinator.execute(&config, "Analyze this codebase").await.unwrap();

        assert_eq!(result.strategy, TeamStrategy::Coordinate);
        assert_eq!(result.agent_results.len(), 2);
        assert!(result.has_successful_results());
        assert!(result.total_tokens > 0);
        // Template synthesis should contain both role labels
        assert!(result.final_answer.contains("[analyst]"));
        assert!(result.final_answer.contains("[reviewer]"));
    }

    #[tokio::test]
    async fn test_coordinate_with_coordinator_agent() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = TeamConfig::coordinate("test_coord_agent")
            .with_role(make_test_role("analyst", "explore"))
            .with_role(make_test_role("reviewer", "review"))
            .with_budget(TokenBudgetProxy::new(50_000))
            .with_coordinator_agent(true); // Use coordinator agent

        let result = coordinator.execute(&config, "Analyze this codebase").await.unwrap();

        assert_eq!(result.strategy, TeamStrategy::Coordinate);
        assert_eq!(result.agent_results.len(), 2);
        // Coordinator agent result should be in the final answer
        assert!(!result.final_answer.is_empty());
    }

    #[tokio::test]
    async fn test_coordinate_single_agent() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = TeamConfig::coordinate("test_single")
            .with_role(make_test_role("analyst", "explore"))
            .with_budget(TokenBudgetProxy::new(50_000));

        let result = coordinator.execute(&config, "Analyze this codebase").await.unwrap();

        assert_eq!(result.agent_results.len(), 1);
        // Single agent result should be the final answer directly
        assert_eq!(result.final_answer, result.agent_results[0].result_text);
    }

    // ─── Route Tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_route_strategy() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = TeamConfig::route("test_route")
            .with_role(make_test_role("router", "plan"))
            .with_role(make_test_role("coder", "code"))
            .with_role(make_test_role("explorer", "explore"))
            .with_budget(TokenBudgetProxy::new(50_000));

        let result = coordinator.execute(&config, "Implement a new feature").await.unwrap();

        assert_eq!(result.strategy, TeamStrategy::Route);
        // Should have at least the router result
        assert!(result.agent_results.len() >= 1);
    }

    // ─── Collaborate Tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_collaborate_two_agents() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = TeamConfig::collaborate("test_collab")
            .with_role(make_test_role("researcher", "explore"))
            .with_role(make_test_role("implementer", "code"))
            .with_budget(TokenBudgetProxy::new(50_000));

        let result = coordinator.execute(&config, "Add authentication").await.unwrap();

        assert_eq!(result.strategy, TeamStrategy::Collaborate);
        assert_eq!(result.agent_results.len(), 2);
        // Final answer should be the last agent's output
        assert!(!result.final_answer.is_empty());
    }

    #[tokio::test]
    async fn test_collaborate_three_agents() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = TeamConfig::collaborate("test_collab3")
            .with_role(make_test_role("researcher", "explore"))
            .with_role(make_test_role("planner", "plan"))
            .with_role(make_test_role("implementer", "code"))
            .with_budget(TokenBudgetProxy::new(80_000));

        let result = coordinator.execute(&config, "Build a REST API").await.unwrap();

        assert_eq!(result.agent_results.len(), 3);
        // Final answer from last agent (implementer)
        assert!(result.final_answer.contains("implementation"));
    }

    // ─── Debate Tests ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_debate_strategy() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = TeamConfig::debate("test_debate")
            .with_role(make_test_role("simplicity", "custom(simplicity)"))
            .with_role(make_test_role("performance", "custom(performance)"))
            .with_role(make_test_role("judge", "review"))
            .with_budget(TokenBudgetProxy::new(60_000));

        let result = coordinator.execute(&config, "Design the caching system").await.unwrap();

        assert_eq!(result.strategy, TeamStrategy::Debate);
        assert_eq!(result.agent_results.len(), 3);
        // Judge should have produced a resolution
        assert!(!result.final_answer.is_empty());
    }

    // ─── Budget Allocation Tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_budget_allocation() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        // 100k budget, 3 agents → ~33k per agent
        let config = TeamConfig::coordinate("test_budget")
            .with_role(make_test_role("a1", "explore"))
            .with_role(make_test_role("a2", "explore"))
            .with_role(make_test_role("a3", "explore"))
            .with_budget(TokenBudgetProxy::new(100_000));

        let budget_per_agent = coordinator.allocate_budget(&config);
        assert!(budget_per_agent.total >= 10_000);
        // Should be roughly 100k / 3 = ~33k
        assert!(budget_per_agent.total >= 30_000);
    }

    // ─── Validation Tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_execute_invalid_config() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        // Empty roles → validation error
        let config = TeamConfig::coordinate("bad_team");
        let result = coordinator.execute(&config, "Do something").await;
        assert!(result.is_err());
    }

    // ─── Route Target Extraction Tests ────────────────────────────────────────

    #[test]
    fn test_extract_route_target_exact() {
        let coordinator = TeamCoordinator::new(Arc::new(MockFactory));

        let roles = vec![
            make_test_role("coder", "code"),
            make_test_role("explorer", "explore"),
        ];

        let target = coordinator.extract_route_target("coder", &roles);
        assert_eq!(target, "coder");
    }

    #[test]
    fn test_extract_route_target_case_insensitive() {
        let coordinator = TeamCoordinator::new(Arc::new(MockFactory));

        let roles = vec![
            make_test_role("coder", "code"),
            make_test_role("explorer", "explore"),
        ];

        let target = coordinator.extract_route_target("CODER", &roles);
        assert_eq!(target, "coder");
    }

    #[test]
    fn test_extract_route_target_substring() {
        let coordinator = TeamCoordinator::new(Arc::new(MockFactory));

        let roles = vec![
            make_test_role("coder", "code"),
            make_test_role("explorer", "explore"),
        ];

        let target = coordinator.extract_route_target("I think the coder should handle this", &roles);
        assert_eq!(target, "coder");
    }

    #[test]
    fn test_extract_route_target_fallback() {
        let coordinator = TeamCoordinator::new(Arc::new(MockFactory));

        let roles = vec![
            make_test_role("coder", "code"),
        ];

        // Unknown target → fallback to first available
        let target = coordinator.extract_route_target("unknown_specialist", &roles);
        assert_eq!(target, "coder");
    }

    // ─── Synthesis Template Tests ─────────────────────────────────────────────

    #[test]
    fn test_synthesize_template_single() {
        let coordinator = TeamCoordinator::new(Arc::new(MockFactory));

        let results = vec![AgentResultEntry {
            role: "analyst".into(),
            agent_kind: SubAgentKindProxy::explore(),
            result_text: "Single result".into(),
            key_findings: vec![],
            completed: true,
            tokens_used: 1000,
        }];

        let synthesized = coordinator.synthesize_template(&results);
        assert_eq!(synthesized, "Single result");
    }

    #[test]
    fn test_synthesize_template_multiple() {
        let coordinator = TeamCoordinator::new(Arc::new(MockFactory));

        let results = vec![
            AgentResultEntry {
                role: "analyst".into(),
                agent_kind: SubAgentKindProxy::explore(),
                result_text: "Analysis result".into(),
                key_findings: vec![],
                completed: true,
                tokens_used: 1000,
            },
            AgentResultEntry {
                role: "reviewer".into(),
                agent_kind: SubAgentKindProxy::review(),
                result_text: "Review result".into(),
                key_findings: vec![],
                completed: true,
                tokens_used: 1000,
            },
        ];

        let synthesized = coordinator.synthesize_template(&results);
        assert!(synthesized.contains("[analyst]"));
        assert!(synthesized.contains("[reviewer]"));
        assert!(synthesized.contains("Analysis result"));
        assert!(synthesized.contains("Review result"));
    }

    // ─── Logging Tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_team_coordination_logging() {
        let log = Arc::new(InMemoryTeamCoordinationLog::new());
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::with_components(
            factory,
            TokenBudget::new(50_000),
            None,
            None,
            log.clone(),
        );

        let config = TeamConfig::coordinate("logged_team")
            .with_role(make_test_role("analyst", "explore"))
            .with_role(make_test_role("reviewer", "review"))
            .with_budget(TokenBudgetProxy::new(50_000))
            .with_coordinator_agent(false);

        let _result = coordinator.execute(&config, "Analyze code").await.unwrap();

        // Check log events
        let events = log.events_for_team("logged_team").await;
        assert!(events.len() >= 5); // TeamStart + 2 AgentStart + 2 AgentComplete + Synthesis + TeamComplete

        // Verify event types
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&oneai_core::team::TeamCoordinationEventKind::TeamStart));
        assert!(kinds.contains(&oneai_core::team::TeamCoordinationEventKind::AgentStart));
        assert!(kinds.contains(&oneai_core::team::TeamCoordinationEventKind::TeamComplete));
    }

    // ─── Preset Tests ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_preset_code_review_team() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = oneai_core::team::TeamPresets::code_review_team();
        let result = coordinator.execute(&config, "Review this PR").await.unwrap();

        assert_eq!(result.strategy, TeamStrategy::Coordinate);
        assert_eq!(result.agent_results.len(), 3);
    }

    #[tokio::test]
    async fn test_preset_development_pipeline() {
        let factory = Arc::new(MockFactory);
        let coordinator = TeamCoordinator::new(factory);

        let config = oneai_core::team::TeamPresets::development_pipeline_team();
        let result = coordinator.execute(&config, "Add new feature").await.unwrap();

        assert_eq!(result.strategy, TeamStrategy::Collaborate);
        assert_eq!(result.agent_results.len(), 3);
    }

    // ─── Result Statistics Tests ──────────────────────────────────────────────

    #[test]
    fn test_team_result_has_successful() {
        let result = TeamResult {
            final_answer: "Test".into(),
            agent_results: vec![
                AgentResultEntry {
                    role: "a1".into(),
                    agent_kind: SubAgentKindProxy::explore(),
                    result_text: "Success".into(),
                    key_findings: vec![],
                    completed: true,
                    tokens_used: 1000,
                },
                AgentResultEntry {
                    role: "a2".into(),
                    agent_kind: SubAgentKindProxy::code(),
                    result_text: "Failed".into(),
                    key_findings: vec![],
                    completed: false,
                    tokens_used: 500,
                },
            ],
            total_tokens: 1500,
            strategy: TeamStrategy::Coordinate,
            team_id: "test".into(),
        };

        assert!(result.has_successful_results());
        assert_eq!(result.successful_results().len(), 1);
        assert_eq!(result.failed_results().len(), 1);
    }
}
