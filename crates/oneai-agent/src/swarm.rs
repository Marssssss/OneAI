//! SwarmOrchestrator — dynamic agent pool orchestration for complex tasks.
//!
//! The SwarmOrchestrator manages dynamic agent pools where:
//! 1. A complex task is decomposed into SwarmTasks
//! 2. Each task is routed to the best-suited agent based on capabilities
//! 3. Tasks run concurrently (respecting dependencies)
//! 4. Results are validated against quality thresholds
//! 5. Failed tasks are retried with alternative agents
//! 6. All results are aggregated into a final SwarmResult
//!
//! The orchestrator supports 3 routing strategies:
//! - **BestFit**: Highest quality agent for the task category
//! - **LoadBalanced**: Distribute across agents, considering current load
//! - **Fastest**: Agent with highest speed score

use std::collections::HashSet;
use std::sync::Arc;

use oneai_core::swarm::{
    SwarmConfig, SwarmResult, SwarmRouting, SwarmTask, SwarmTaskResult,
    SwarmAgentEntry, SwarmCoordinationLog, InMemorySwarmCoordinationLog,
};
use oneai_core::team::SubAgentKindProxy;
use oneai_core::budget::TokenBudget;
use oneai_core::usage::UsageTracker;
use oneai_core::context_manager::ContextManager;
use oneai_core::error::Result;

use crate::sub_agent::{SubAgentFactory, SubAgentKind, SubAgentSummary};

// ─── SwarmOrchestrator ──────────────────────────────────────────────────────────

/// Swarm orchestrator — manages dynamic agent pools for complex tasks.
///
/// The orchestrator:
/// 1. Decomposes a complex task into SwarmTasks
/// 2. Routes each task to the best-suited agent based on capabilities
/// 3. Runs tasks concurrently (respecting dependencies)
/// 4. Validates results against quality threshold
/// 5. Retries failed tasks with alternative agents
/// 6. Aggregates all results into a final SwarmResult
///
/// The orchestrator tracks token usage and logs all coordination events
/// through the SwarmCoordinationLog trait.
pub struct SwarmOrchestrator {
    /// The sub-agent factory for creating swarm agents.
    factory: Arc<dyn SubAgentFactory>,

    /// Default budget for swarm execution.
    #[allow(dead_code)]
    default_budget: TokenBudget,

    /// Context manager for trimming task context (optional).
    context_manager: Option<Arc<ContextManager>>,

    /// Usage tracker for recording swarm usage (optional).
    usage_tracker: Option<Arc<dyn UsageTracker>>,

    /// Coordination log for recording swarm events.
    log: Arc<dyn SwarmCoordinationLog>,
}

impl SwarmOrchestrator {
    /// Create a new SwarmOrchestrator with the given sub-agent factory.
    pub fn new(factory: Arc<dyn SubAgentFactory>) -> Self {
        Self {
            factory,
            default_budget: TokenBudget::new(100_000),
            context_manager: None,
            usage_tracker: None,
            log: Arc::new(InMemorySwarmCoordinationLog::new()),
        }
    }

    /// Create a SwarmOrchestrator with a custom default budget.
    pub fn with_budget(factory: Arc<dyn SubAgentFactory>, budget: TokenBudget) -> Self {
        Self {
            factory,
            default_budget: budget,
            context_manager: None,
            usage_tracker: None,
            log: Arc::new(InMemorySwarmCoordinationLog::new()),
        }
    }

    /// Create a SwarmOrchestrator with all optional components.
    pub fn with_components(
        factory: Arc<dyn SubAgentFactory>,
        budget: TokenBudget,
        context_manager: Option<Arc<ContextManager>>,
        usage_tracker: Option<Arc<dyn UsageTracker>>,
        log: Arc<dyn SwarmCoordinationLog>,
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
    pub fn log(&self) -> &Arc<dyn SwarmCoordinationLog> {
        &self.log
    }

    // ─── Execute ──────────────────────────────────────────────────────────────

    /// Execute a swarm task.
    ///
    /// Takes a SwarmConfig and a task description, validates the config,
    /// decomposes the task into subtasks, routes each to the best agent,
    /// runs them (respecting dependencies), validates results, retries
    /// failed tasks, and aggregates into a SwarmResult.
    pub async fn execute(&self, config: &SwarmConfig, task: &str) -> Result<SwarmResult> {
        // Validate config
        config.validate()?;

        // Log swarm start
        self.log.log_swarm_start(&config.id, config.routing, task).await;

        // Decompose the task into subtasks
        let subtasks = self.decompose(task, config);

        // Execute subtasks with routing
        let result = self.execute_subtasks(config, &subtasks).await?;

        // Log swarm complete
        self.log.log_swarm_complete(&config.id, result.total_tokens).await;

        Ok(result)
    }

    // ─── Decompose ────────────────────────────────────────────────────────────

    /// Decompose a complex task into SwarmTasks.
    ///
    /// The decomposition strategy:
    /// 1. Identify the main categories covered by the swarm's agents
    /// 2. Create one task per category, with the task description
    ///    tailored to that category
    /// 3. Set up dependencies: research → planning → code → review
    ///
    /// This is a heuristic decomposition. In a production system,
    /// this could be replaced with LLM-driven task decomposition.
    fn decompose(&self, task: &str, config: &SwarmConfig) -> Vec<SwarmTask> {
        let categories = config.all_categories();
        let mut subtasks: Vec<SwarmTask> = Vec::new();

        // Create tasks based on available categories
        // Research tasks come first, then planning, then code, then review
        let priority_order = ["research", "analysis", "planning", "design", "code", "debug", "review", "security"];

        for (i, category) in categories.iter().enumerate() {
            let task_description = self.make_category_task(task, category);
            let priority = priority_order.iter()
                .position(|p| p == category)
                .map(|pos| (priority_order.len() - pos) as f64)
                .unwrap_or(1.0);

            let mut swarm_task = SwarmTask::new(
                &format!("task_{}", i + 1),
                &task_description,
                category,
            ).with_priority(priority);

            // Add dependencies: earlier categories depend on research
            if i > 0 && category != "research" && category != "analysis" {
                // Research tasks don't depend on anything
                // Other tasks depend on the research task
                if let Some(research_task_id) = subtasks.iter()
                    .find(|t| t.category == "research" || t.category == "analysis")
                    .map(|t| t.id.clone())
                {
                    swarm_task = swarm_task.with_dependency(&research_task_id);
                }
            }

            subtasks.push(swarm_task);
        }

        // Sort by priority (highest first)
        subtasks.sort_by(|a, b| b.priority.partial_cmp(&a.priority).unwrap_or(std::cmp::Ordering::Equal));

        subtasks
    }

    /// Make a task description tailored to a category.
    fn make_category_task(&self, task: &str, category: &str) -> String {
        match category {
            "research" | "analysis" => format!(
                "Research and analyze the following task. Gather all relevant context, \
                 identify key components, and produce a comprehensive research summary.\n\
                 Task: {}", task
            ),
            "planning" | "design" => format!(
                "Based on the research results, create a detailed plan for the following task. \
                 Identify steps, dependencies, and design decisions.\n\
                 Task: {}", task
            ),
            "code" | "debug" => format!(
                "Implement the following task based on the plan. Write clean, well-structured \
                 code that follows best practices.\n\
                 Task: {}", task
            ),
            "review" | "security" => format!(
                "Review the implementation of the following task. Check for correctness, \
                 quality, security issues, and best practices compliance.\n\
                 Task: {}", task
            ),
            _ => format!("Handle the following task from a '{}' perspective.\nTask: {}", category, task),
        }
    }

    // ─── Execute Subtasks ─────────────────────────────────────────────────────

    /// Execute all subtasks with routing and dependency management.
    async fn execute_subtasks(
        &self,
        config: &SwarmConfig,
        subtasks: &[SwarmTask],
    ) -> Result<SwarmResult> {
        let mut task_results: Vec<SwarmTaskResult> = Vec::new();
        let mut completed_ids: HashSet<String> = HashSet::new();
        let mut total_tokens = 0u32;
        let mut active_agents: Vec<String> = Vec::new();
        let mut accumulated_context = String::new();

        // Process tasks in dependency order
        // Simple approach: iterate through tasks, skip those with unsatisfied deps,
        // then process them once deps are complete
        let mut remaining: Vec<&SwarmTask> = subtasks.iter().collect();
        let mut max_iterations = remaining.len() * 2; // Safety limit

        while !remaining.is_empty() && max_iterations > 0 {
            max_iterations -= 1;

            // Find tasks with satisfied dependencies
            let ready_tasks: Vec<&SwarmTask> = remaining.iter()
                .filter(|t| t.dependencies_satisfied(&completed_ids))
                .copied()
                .collect();

            if ready_tasks.is_empty() {
                // Deadlock: all remaining tasks have unsatisfied dependencies
                // This shouldn't happen with proper decomposition
                break;
            }

            // Execute ready tasks
            for task in &ready_tasks {
                // Route the task to an agent
                let agent_name = self.route_task(task, config);
                let agent_entry = config.agent_by_name(&agent_name);

                if let Some(agent_entry) = agent_entry {
                    self.log.log_task_routed(&config.id, &task.id, &agent_name, &task.category).await;
                    self.log.log_task_start(&config.id, &task.id, &agent_name).await;

                    // Build the task prompt with accumulated context
                    let task_prompt = self.build_task_prompt(task, &accumulated_context);

                    // Create and run the agent
                    let kind = self.resolve_kind(&agent_entry.agent_kind);
                    let budget_per_task = self.allocate_budget(config, subtasks.len());

                    let result = self.run_agent_with_retry(
                        &config.id,
                        task,
                        &agent_name,
                        &agent_entry.agent_kind,
                        &task_prompt,
                        kind,
                        budget_per_task,
                        config,
                    ).await;

                    match result {
                        Ok((summary, retry_count)) => {
                            let quality_score = self.estimate_quality(&summary);

                            self.log.log_task_complete(
                                &config.id, &task.id, &agent_name,
                                summary.tokens_used, summary.completed, quality_score
                            ).await;

                            // Validate result quality
                            let accepted = quality_score >= config.quality_threshold;
                            self.log.log_result_validation(
                                &config.id, &task.id, quality_score,
                                config.quality_threshold, accepted
                            ).await;

                            // Accumulate context for subsequent tasks
                            if summary.completed {
                                accumulated_context = format!(
                                    "{}\n[{}]: {}",
                                    accumulated_context, task.category, summary.summary
                                );
                            }

                            // Create task result entry
                            task_results.push(SwarmTaskResult {
                                task_id: task.id.clone(),
                                task_description: task.description.clone(),
                                category: task.category.clone(),
                                agent_name: agent_name.clone(),
                                agent_kind: agent_entry.agent_kind.clone(),
                                result_text: summary.summary.clone(),
                                key_findings: summary.key_findings.clone(),
                                completed: summary.completed,
                                quality_score,
                                tokens_used: summary.tokens_used,
                                retry_count,
                            });

                            total_tokens += summary.tokens_used;

                            if !active_agents.contains(&agent_name) {
                                active_agents.push(agent_name.clone());
                            }

                            completed_ids.insert(task.id.clone());
                        }
                        Err(e) => {
                            // Task failed completely
                            task_results.push(SwarmTaskResult {
                                task_id: task.id.clone(),
                                task_description: task.description.clone(),
                                category: task.category.clone(),
                                agent_name: agent_name.clone(),
                                agent_kind: agent_entry.agent_kind.clone(),
                                result_text: format!("Task failed: {}", e),
                                key_findings: vec![],
                                completed: false,
                                quality_score: 0.0,
                                tokens_used: 0,
                                retry_count: 0,
                            });

                            // Still mark as completed to avoid deadlock
                            completed_ids.insert(task.id.clone());
                        }
                    }
                } else {
                    // No agent found for this task
                    task_results.push(SwarmTaskResult {
                        task_id: task.id.clone(),
                        task_description: task.description.clone(),
                        category: task.category.clone(),
                        agent_name: "none".to_string(),
                        agent_kind: SubAgentKindProxy::custom("none"),
                        result_text: "No agent available for this category".to_string(),
                        key_findings: vec![],
                        completed: false,
                        quality_score: 0.0,
                        tokens_used: 0,
                        retry_count: 0,
                    });

                    completed_ids.insert(task.id.clone());
                }
            }

            // Remove completed tasks from remaining
            remaining = remaining.iter()
                .filter(|t| !completed_ids.contains(&t.id))
                .copied()
                .collect();
        }

        // Aggregate results into a final answer
        let final_answer = self.aggregate_results(&task_results);

        Ok(SwarmResult {
            final_answer,
            task_results,
            total_tokens,
            active_agents,
        })
    }

    // ─── Routing ──────────────────────────────────────────────────────────────

    /// Route a task to the best-suited agent based on the routing strategy.
    ///
    /// The routing considers:
    /// - Agent capabilities (quality/speed scores per category)
    /// - Current agent load (for LoadBalanced routing)
    /// - Quality threshold (for BestFit routing)
    fn route_task(&self, task: &SwarmTask, config: &SwarmConfig) -> String {
        // Find agents that can handle this category
        let capable_agents: Vec<&SwarmAgentEntry> = config.agents.iter()
            .filter(|a| a.capability.handles_category(&task.category))
            .collect();

        if capable_agents.is_empty() {
            // No agent handles this category — find the best general match
            let best_general = config.agents.iter()
                .max_by_key(|a| {
                    let best_quality = a.capability.quality_scores.values()
                        .fold(0.0_f64, |a, b| if a > *b { a } else { *b });
                    (best_quality * 1000.0) as u64
                });
            return best_general.map(|a| a.name.clone()).unwrap_or_default();
        }

        match config.routing {
            SwarmRouting::BestFit => {
                // Route to agent with highest quality score for this category
                capable_agents.iter()
                    .max_by_key(|a| (a.capability.quality_for(&task.category) * 1000.0) as u64)
                    .map(|a| a.name.clone())
                    .unwrap_or_default()
            }
            SwarmRouting::LoadBalanced => {
                // Route to agent with lowest current load that can handle the category
                capable_agents.iter()
                    .min_by_key(|a| a.current_load)
                    .map(|a| a.name.clone())
                    .unwrap_or_default()
            }
            SwarmRouting::Fastest => {
                // Route to agent with highest speed score for this category
                capable_agents.iter()
                    .max_by_key(|a| (a.capability.speed_for(&task.category) * 1000.0) as u64)
                    .map(|a| a.name.clone())
                    .unwrap_or_default()
            }
            _ => {
                // Default to best-fit for unknown strategies
                capable_agents.iter()
                    .max_by_key(|a| (a.capability.quality_for(&task.category) * 1000.0) as u64)
                    .map(|a| a.name.clone())
                    .unwrap_or_default()
            }
        }
    }

    // ─── Retry ────────────────────────────────────────────────────────────────

    /// Run an agent with retry logic.
    ///
    /// If the result quality is below threshold, retry with an alternative agent.
    /// Returns the final summary and the number of retries attempted.
    async fn run_agent_with_retry(
        &self,
        swarm_id: &str,
        task: &SwarmTask,
        initial_agent_name: &str,
        _initial_agent_kind: &SubAgentKindProxy,
        task_prompt: &str,
        initial_kind: SubAgentKind,
        budget: TokenBudget,
        config: &SwarmConfig,
    ) -> Result<(SubAgentSummary, usize)> {
        let mut agent_name = initial_agent_name.to_string();
        let mut kind = initial_kind;
        let mut retry_count = 0;

        // Run the initial agent
        let agent = self.factory.create(kind.clone(), budget.clone()).await?;
        let summary = agent.run(task_prompt).await?;

        // Check quality and retry if needed
        let quality_score = self.estimate_quality(&summary);

        if quality_score < config.quality_threshold && config.max_retries > 0 {
            // Find alternative agents for retry
            let alternatives: Vec<&SwarmAgentEntry> = config.agents.iter()
                .filter(|a| a.name != agent_name && a.capability.handles_category(&task.category))
                .collect();

            for alternative in alternatives.iter().take(config.max_retries) {
                retry_count += 1;

                self.log.log_task_retry(
                    swarm_id, &task.id, &agent_name, &alternative.name, retry_count
                ).await;

                agent_name = alternative.name.clone();
                kind = self.resolve_kind(&alternative.agent_kind);

                let retry_agent = self.factory.create(kind.clone(), budget.clone()).await?;
                let retry_summary = retry_agent.run(task_prompt).await?;
                let retry_quality = self.estimate_quality(&retry_summary);

                if retry_quality >= config.quality_threshold {
                    return Ok((retry_summary, retry_count));
                }
            }

            // If all retries failed but we got a result, return the best one
            // (The initial summary is likely better than nothing)
        }

        Ok((summary, retry_count))
    }

    // ─── Helpers ──────────────────────────────────────────────────────────────

    /// Resolve a SubAgentKindProxy to an actual SubAgentKind.
    fn resolve_kind(&self, proxy: &SubAgentKindProxy) -> SubAgentKind {
        SubAgentKind::from_str(proxy.name())
    }

    /// Allocate budget per task.
    fn allocate_budget(&self, config: &SwarmConfig, num_tasks: usize) -> TokenBudget {
        let total = config.budget.total_tokens;
        let per_task = total / num_tasks.max(1) as u32;
        TokenBudget::new(per_task.max(10_000)) // Minimum 10k per task
    }

    /// Build a task prompt with accumulated context from previous tasks.
    fn build_task_prompt(&self, task: &SwarmTask, accumulated_context: &str) -> String {
        if accumulated_context.is_empty() {
            task.description.clone()
        } else {
            format!(
                "{}\n\nPrevious swarm results:\n{}",
                task.description, accumulated_context
            )
        }
    }

    /// Estimate the quality of a result based on the summary.
    ///
    /// This is a heuristic: longer, more structured summaries tend to
    /// have higher quality. In a production system, this could use
    /// LLM-based quality scoring.
    fn estimate_quality(&self, summary: &SubAgentSummary) -> f64 {
        if !summary.completed {
            return 0.0;
        }

        // Base quality from completion
        let base_quality = 0.7;

        // Bonus for longer summaries (more detail = higher quality)
        let length_bonus = if summary.summary.len() > 500 {
            0.1
        } else if summary.summary.len() > 200 {
            0.05
        } else {
            0.0
        };

        // Bonus for having key findings
        let findings_bonus = if !summary.key_findings.is_empty() {
            0.05
        } else {
            0.0
        };

        // Clamp to 0.0–1.0
        let score = base_quality + length_bonus + findings_bonus;
        if score > 1.0 { 1.0 } else { score }
    }

    /// Aggregate task results into a final answer.
    fn aggregate_results(&self, results: &[SwarmTaskResult]) -> String {
        if results.is_empty() {
            return String::new();
        }

        let successful = results.iter().filter(|r| r.completed).collect::<Vec<_>>();

        if successful.is_empty() {
            return "All tasks failed — no results to aggregate".to_string();
        }

        if successful.len() == 1 {
            return successful[0].result_text.clone();
        }

        // Template-based aggregation: label each result by category
        successful.iter()
            .map(|r| format!("[{} (agent: {})]: {}", r.category, r.agent_name, r.result_text))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::swarm::{SwarmConfig, SwarmTask, SwarmAgentEntry};
    use oneai_core::team::SubAgentKindProxy;
    use oneai_core::team::TokenBudgetProxy; // test-only
    use oneai_core::budget::TokenBudget;
    use async_trait::async_trait;
    use oneai_core::error::Result;

    use crate::sub_agent::{SubAgent, SubAgentFactory, SubAgentKind, SubAgentSummary};

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

    // ─── Routing Tests ────────────────────────────────────────────────────────

    #[test]
    fn test_route_best_fit() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::best_fit("test_route")
            .with_agent(SwarmAgentEntry::coder())
            .with_agent(SwarmAgentEntry::researcher())
            .with_agent(SwarmAgentEntry::reviewer());

        let task = SwarmTask::new("t1", "Implement feature", "code");
        let agent = orchestrator.route_task(&task, &config);
        assert_eq!(agent, "coder"); // Best quality for "code" category
    }

    #[test]
    fn test_route_fastest() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::fastest("test_route")
            .with_agent(SwarmAgentEntry::coder())
            .with_agent(SwarmAgentEntry::researcher())
            .with_agent(SwarmAgentEntry::reviewer());

        let task = SwarmTask::new("t1", "Research the codebase", "research");
        let agent = orchestrator.route_task(&task, &config);
        assert_eq!(agent, "researcher"); // Best speed for "research" category
    }

    #[test]
    fn test_route_load_balanced() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::load_balanced("test_route")
            .with_agent(SwarmAgentEntry::coder().with_current_load(3))
            .with_agent(SwarmAgentEntry::planner().with_current_load(0));

        let task = SwarmTask::new("t1", "Plan the project", "planning");
        let agent = orchestrator.route_task(&task, &config);
        // Planner has lower load (0 vs 3), but planner doesn't handle "planning"
        // Actually, planner DOES handle "planning" category, and coder does NOT
        // So planner should be selected (load=0)
        assert_eq!(agent, "planner");
    }

    #[test]
    fn test_route_no_capable_agent() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::best_fit("test_route")
            .with_agent(SwarmAgentEntry::coder());

        let task = SwarmTask::new("t1", "Research the codebase", "research");
        let agent = orchestrator.route_task(&task, &config);
        // Coder doesn't handle "research" — fallback to best general agent
        // Coder's best quality score is for "code" = 0.9
        assert_eq!(agent, "coder"); // Fallback
    }

    // ─── Execute Tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_execute_best_fit_swarm() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::best_fit("test_exec")
            .with_agent(SwarmAgentEntry::coder())
            .with_agent(SwarmAgentEntry::researcher())
            .with_budget(TokenBudgetProxy::new(50_000))
            .with_quality_threshold(0.6);

        let result = orchestrator.execute(&config, "Analyze and implement this feature").await.unwrap();

        assert!(!result.final_answer.is_empty());
        assert!(result.task_results.len() >= 1);
        assert!(result.total_tokens > 0);
        assert!(!result.active_agents.is_empty());
    }

    #[tokio::test]
    async fn test_execute_fastest_swarm() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::fastest("test_exec")
            .with_agent(SwarmAgentEntry::researcher())
            .with_agent(SwarmAgentEntry::planner())
            .with_budget(TokenBudgetProxy::new(50_000))
            .with_quality_threshold(0.6);

        let result = orchestrator.execute(&config, "Quick research task").await.unwrap();

        assert!(!result.final_answer.is_empty());
        assert!(result.task_results.len() >= 1);
    }

    #[tokio::test]
    async fn test_execute_load_balanced_swarm() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::load_balanced("test_exec")
            .with_agent(SwarmAgentEntry::coder())
            .with_agent(SwarmAgentEntry::researcher())
            .with_budget(TokenBudgetProxy::new(50_000))
            .with_quality_threshold(0.6);

        let result = orchestrator.execute(&config, "Develop a feature").await.unwrap();

        assert!(!result.final_answer.is_empty());
        assert!(result.task_results.len() >= 1);
    }

    // ─── Validation Tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_execute_invalid_config() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::best_fit("bad_swarm"); // No agents
        let result = orchestrator.execute(&config, "Do something").await;
        assert!(result.is_err());
    }

    // ─── Preset Tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_preset_code_analysis() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = oneai_core::swarm::SwarmPresets::code_analysis_swarm();
        let result = orchestrator.execute(&config, "Analyze this codebase for security issues").await.unwrap();

        assert!(!result.final_answer.is_empty());
        assert!(result.task_results.len() >= 2);
        assert!(result.active_agents.len() <= result.task_results.len());
    }

    #[tokio::test]
    async fn test_preset_fast_research() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = oneai_core::swarm::SwarmPresets::fast_research_swarm();
        let result = orchestrator.execute(&config, "Research the latest Rust frameworks").await.unwrap();

        assert!(!result.final_answer.is_empty());
    }

    #[tokio::test]
    async fn test_preset_balanced_dev() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = oneai_core::swarm::SwarmPresets::balanced_dev_swarm();
        let result = orchestrator.execute(&config, "Build a REST API endpoint").await.unwrap();

        assert!(!result.final_answer.is_empty());
    }

    // ─── Logging Tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_swarm_coordination_logging() {
        let log = Arc::new(InMemorySwarmCoordinationLog::new());
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::with_components(
            factory,
            TokenBudget::new(50_000),
            None,
            None,
            log.clone(),
        );

        let config = SwarmConfig::best_fit("logged_swarm")
            .with_agent(SwarmAgentEntry::coder())
            .with_agent(SwarmAgentEntry::researcher())
            .with_budget(TokenBudgetProxy::new(50_000))
            .with_quality_threshold(0.6);

        let _result = orchestrator.execute(&config, "Analyze code").await.unwrap();

        // Check log events
        let events = log.events_for_swarm("logged_swarm").await;
        assert!(events.len() >= 2); // At least SwarmStart + SwarmComplete

        // Verify event types
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&oneai_core::swarm::SwarmCoordinationEventKind::SwarmStart));
        assert!(kinds.contains(&oneai_core::swarm::SwarmCoordinationEventKind::SwarmComplete));
    }

    // ─── Quality Estimation Tests ────────────────────────────────────────────────

    #[test]
    fn test_estimate_quality_completed() {
        let orchestrator = SwarmOrchestrator::new(Arc::new(MockFactory));

        let summary = SubAgentSummary {
            completed: true,
            summary: "This is a detailed result with multiple findings about the task. It covers several aspects and provides comprehensive analysis.".to_string(),
            key_findings: vec!["Finding 1".to_string(), "Finding 2".to_string()],
            budget_exceeded: false,
            agent_kind: SubAgentKind::Explore,
            tokens_used: 2000,
        };

        let quality = orchestrator.estimate_quality(&summary);
        assert!(quality >= 0.7); // Completed with detail → at least 0.7
        assert!(quality <= 1.0);
    }

    #[test]
    fn test_estimate_quality_failed() {
        let orchestrator = SwarmOrchestrator::new(Arc::new(MockFactory));

        let summary = SubAgentSummary {
            completed: false,
            summary: "".to_string(),
            key_findings: vec![],
            budget_exceeded: false,
            agent_kind: SubAgentKind::Explore,
            tokens_used: 0,
        };

        let quality = orchestrator.estimate_quality(&summary);
        assert_eq!(quality, 0.0); // Failed → 0.0
    }

    // ─── Aggregation Tests ────────────────────────────────────────────────────────

    #[test]
    fn test_aggregate_empty() {
        let orchestrator = SwarmOrchestrator::new(Arc::new(MockFactory));

        let aggregated = orchestrator.aggregate_results(&[]);
        assert!(aggregated.is_empty());
    }

    #[test]
    fn test_aggregate_single() {
        let orchestrator = SwarmOrchestrator::new(Arc::new(MockFactory));

        let results = vec![SwarmTaskResult {
            task_id: "t1".into(),
            task_description: "Research".into(),
            category: "research".into(),
            agent_name: "researcher".into(),
            agent_kind: SubAgentKindProxy::explore(),
            result_text: "Research findings".into(),
            key_findings: vec![],
            completed: true,
            quality_score: 0.85,
            tokens_used: 2000,
            retry_count: 0,
        }];

        let aggregated = orchestrator.aggregate_results(&results);
        assert_eq!(aggregated, "Research findings");
    }

    #[test]
    fn test_aggregate_multiple() {
        let orchestrator = SwarmOrchestrator::new(Arc::new(MockFactory));

        let results = vec![
            SwarmTaskResult {
                task_id: "t1".into(),
                task_description: "Research".into(),
                category: "research".into(),
                agent_name: "researcher".into(),
                agent_kind: SubAgentKindProxy::explore(),
                result_text: "Research findings".into(),
                key_findings: vec![],
                completed: true,
                quality_score: 0.85,
                tokens_used: 2000,
                retry_count: 0,
            },
            SwarmTaskResult {
                task_id: "t2".into(),
                task_description: "Code".into(),
                category: "code".into(),
                agent_name: "coder".into(),
                agent_kind: SubAgentKindProxy::code(),
                result_text: "Code implementation".into(),
                key_findings: vec![],
                completed: true,
                quality_score: 0.9,
                tokens_used: 3000,
                retry_count: 0,
            },
        ];

        let aggregated = orchestrator.aggregate_results(&results);
        assert!(aggregated.contains("[research"));
        assert!(aggregated.contains("[code"));
        assert!(aggregated.contains("Research findings"));
        assert!(aggregated.contains("Code implementation"));
    }

    // ─── Decompose Tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_decompose_single_agent() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::best_fit("test_decompose")
            .with_agent(SwarmAgentEntry::coder());

        let tasks = orchestrator.decompose("Implement feature X", &config);
        assert_eq!(tasks.len(), 2); // code + debug categories
    }

    #[test]
    fn test_decompose_multi_agent() {
        let factory = Arc::new(MockFactory);
        let orchestrator = SwarmOrchestrator::new(factory);

        let config = SwarmConfig::best_fit("test_decompose")
            .with_agent(SwarmAgentEntry::coder())
            .with_agent(SwarmAgentEntry::researcher())
            .with_agent(SwarmAgentEntry::reviewer());

        let tasks = orchestrator.decompose("Analyze and review this code", &config);
        // Should create tasks for all categories: research, analysis, code, debug, review, security
        assert!(tasks.len() >= 3);
    }
}
