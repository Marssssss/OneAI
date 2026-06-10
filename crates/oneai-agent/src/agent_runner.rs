//! Agent Runner — orchestrates paradigm fusion (Plan → Parallel → ReAct → Reflection).
//!
//! The AgentRunner is the top-level entry point for complex task execution.
//! It implements the full paradigm fusion:
//! 1. PlanAgent: decomposes complex tasks into ordered steps
//! 2. ParallelExecutor: runs non-coupled steps concurrently with isolated ScopeState
//! 3. ReActAgent: executes coupled steps in a pipeline with tool calling
//! 4. ReflectionAgent: verifies results and suggests corrections
//!
//! The key challenge is context flow: when tasks flow from Planner to
//! parallel sub-agents and then to ReAct pipeline, the ScopeState isolation
//! ensures no state pollution between concurrent agents.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use oneai_core::{Conversation, GlobalState, Message, Role};
use oneai_core::error::Result;
use oneai_core::traits::{ApprovalGate, LlmProvider, OutputParser, Tool, StateReducer};

use crate::plan_agent::{PlanAgent, PlanConfig, PlanResult, PlanStep};
use crate::react_agent::{ReActAgent, ReActConfig, ReActResult};
use crate::reflection_agent::{ReflectionAgent, ReflectionConfig, ReflectionResult};
use crate::parallel_executor::{ParallelExecutor, ParallelResult, ParallelStepResult};

/// Configuration for the AgentRunner.
#[derive(Debug, Clone)]
pub struct AgentRunnerConfig {
    /// Whether to use planning (for complex tasks).
    pub use_planning: bool,

    /// Whether to use reflection (for result verification).
    pub use_reflection: bool,

    /// Whether to use parallel execution (for non-coupled steps).
    pub use_parallel: bool,

    /// Planning configuration.
    pub plan_config: PlanConfig,

    /// ReAct configuration.
    pub react_config: ReActConfig,

    /// Reflection configuration.
    pub reflection_config: ReflectionConfig,

    /// System prompt for the overall agent.
    pub system_prompt: String,
}

impl Default for AgentRunnerConfig {
    fn default() -> Self {
        Self {
            use_planning: true,
            use_reflection: true,
            use_parallel: true,
            plan_config: PlanConfig::default(),
            react_config: ReActConfig::default(),
            reflection_config: ReflectionConfig::default(),
            system_prompt: "You are an intelligent AI agent that can plan, execute, and reflect on tasks.".to_string(),
        }
    }
}

/// Result of the full paradigm fusion execution.
#[derive(Debug, Clone)]
pub struct AgentRunnerResult {
    /// The final conversation after all processing.
    pub conversation: Conversation,

    /// The final answer from the agent.
    pub final_answer: String,

    /// The global state after all reductions.
    pub global_state: GlobalState,

    /// The plan (if planning was used).
    pub plan: Option<PlanResult>,

    /// The ReAct result (if ReAct was used).
    pub react_result: Option<ReActResult>,

    /// The parallel result (if parallel execution was used).
    pub parallel_result: Option<ParallelResult>,

    /// The reflection result (if reflection was used).
    pub reflection_result: Option<ReflectionResult>,

    /// Whether the task completed successfully.
    pub success: bool,

    /// Total iterations across all paradigms.
    pub total_iterations: usize,
}

/// The top-level agent runner that orchestrates paradigm fusion.
///
/// For complex tasks, the execution flow is:
/// 1. Plan: decompose into steps
/// 2. Separate coupled vs non-coupled steps
/// 3. Execute non-coupled steps in parallel (ParallelExecutor)
/// 4. Execute coupled steps via ReAct pipeline
/// 5. Merge all results
/// 6. Reflect on the final result
///
/// For simple tasks, skip planning and go directly to ReAct.
pub struct AgentRunner {
    /// The LLM provider.
    provider: Arc<dyn LlmProvider>,

    /// Available tools.
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,

    /// Output parser (3-layer defense).
    parser: Arc<dyn OutputParser>,

    /// Approval gate for high-risk tools.
    approval_gate: Arc<dyn ApprovalGate>,

    /// State reducer for merging parallel results.
    reducer: Arc<dyn StateReducer>,

    /// Configuration.
    config: AgentRunnerConfig,
}

impl AgentRunner {
    /// Create a new AgentRunner with all dependencies.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
        parser: Arc<dyn OutputParser>,
        approval_gate: Arc<dyn ApprovalGate>,
        reducer: Arc<dyn StateReducer>,
        config: AgentRunnerConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            parser,
            approval_gate,
            reducer,
            config,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
        parser: Arc<dyn OutputParser>,
        approval_gate: Arc<dyn ApprovalGate>,
    ) -> Self {
        Self::new(
            provider,
            tools,
            parser,
            approval_gate,
            Arc::new(crate::scope_state::DefaultStateReducer),
            AgentRunnerConfig::default(),
        )
    }

    /// Run the full paradigm fusion for a task.
    ///
    /// This is the main entry point for complex task execution.
    /// The agent will:
    /// 1. Assess whether planning is needed
    /// 2. Plan if needed, or go directly to ReAct
    /// 3. Execute parallel steps if applicable
    /// 4. Execute ReAct pipeline for coupled steps
    /// 5. Reflect on the final result
    pub async fn run(&self, task: &str) -> Result<AgentRunnerResult> {
        let mut global_state = GlobalState::new();
        let mut conv = Conversation::new();
        conv.add_message(Message::system(self.config.system_prompt.clone()));
        conv.add_message(Message::user(task.to_string()));

        let mut plan_result = None;
        let mut react_result = None;
        let mut parallel_result = None;
        let mut reflection_result = None;
        let mut total_iterations = 0;

        if self.config.use_planning {
            // Step 1: Plan
            tracing::info!("AgentRunner: Starting planning phase");
            let plan_agent = PlanAgent::new(self.provider.clone(), self.config.plan_config.clone());
            let plan = plan_agent.plan(task).await?;
            tracing::info!("AgentRunner: Plan generated with {} steps", plan.steps.len());
            plan_result = Some(plan.clone());

            // Step 2: Separate coupled vs non-coupled steps
            let non_coupled_steps: Vec<PlanStep> = plan.steps.iter()
                .filter(|s| !s.coupled)
                .cloned()
                .collect();
            let coupled_steps: Vec<PlanStep> = plan.steps.iter()
                .filter(|s| s.coupled)
                .cloned()
                .collect();

            // Step 3: Execute non-coupled steps in parallel
            if self.config.use_parallel && !non_coupled_steps.is_empty() {
                tracing::info!("AgentRunner: Executing {} parallel steps", non_coupled_steps.len());
                let parallel_exec = ParallelExecutor::new(self.reducer.clone());

                // Create a simple executor using the LLM provider
                let provider = self.provider.clone();
                let config = self.config.react_config.clone();
                let tools = self.tools.clone();
                let parser = self.parser.clone();
                let approval_gate = self.approval_gate.clone();

                let par_result = parallel_exec.execute_parallel(
                    &non_coupled_steps,
                    &global_state,
                    move |step, _scope| {
                        let provider = provider.clone();
                        let tools = tools.clone();
                        let parser = parser.clone();
                        let approval_gate = approval_gate.clone();
                        let config = config.clone();
                        async move {
                            // Create a ReAct agent for each parallel step
                            let react = ReActAgent::new(
                                provider,
                                tools,
                                parser,
                                approval_gate,
                                config,
                            );

                            // Create a conversation for this step
                            let mut step_conv = Conversation::new();
                            step_conv.add_message(Message::user(step.description.clone()));

                            let result = react.run(step_conv).await?;

                            // Create reductions
                            let reduction = oneai_core::Reduction::SetResult {
                                step_id: step.id.clone(),
                                result: oneai_core::ContentBlock::Text {
                                    text: result.final_message.text_content(),
                                },
                            };

                            Ok(ParallelStepResult {
                                step_id: step.id.clone(),
                                result: result.final_message.text_content(),
                                success: result.completed,
                                reductions: vec![reduction],
                            })
                        }
                    },
                ).await?;

                total_iterations += par_result.step_results.iter()
                    .map(|_| 1)
                    .sum::<usize>();

                // Merge parallel results into global state
                global_state = par_result.global_state.clone();
                parallel_result = Some(par_result);
            }

            // Step 4: Execute coupled steps via ReAct pipeline
            if !coupled_steps.is_empty() {
                tracing::info!("AgentRunner: Executing {} coupled steps via ReAct", coupled_steps.len());

                let react_agent = ReActAgent::new(
                    self.provider.clone(),
                    self.tools.clone(),
                    self.parser.clone(),
                    self.approval_gate.clone(),
                    self.config.react_config.clone(),
                );

                // Build the task description from coupled steps
                let coupled_task = coupled_steps.iter()
                    .map(|s| format!("{}. {}", s.id, s.description))
                    .collect::<Vec<_>>()
                    .join("\n");

                // Include parallel results in context if available
                let mut react_conv = conv.clone();
                if let Some(par) = &parallel_result {
                    for step_result in &par.step_results {
                        react_conv.add_message(Message::assistant(format!(
                            "Parallel step {} result: {}", step_result.step_id, step_result.result
                        )));
                    }
                }
                react_conv.add_message(Message::user(format!(
                    "Execute these sequential steps:\n{}", coupled_task
                )));

                let result = react_agent.run(react_conv).await?;
                total_iterations += result.iterations;
                conv = result.conversation.clone();
                react_result = Some(result);
            } else if let Some(par) = &parallel_result {
                // No coupled steps — just use parallel results
                let combined_result = par.step_results.iter()
                    .map(|r| format!("{}: {}", r.step_id, r.result))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                conv.add_message(Message::assistant(combined_result));
            }
        } else {
            // Simple task — skip planning, go directly to ReAct
            tracing::info!("AgentRunner: Skipping planning, using ReAct directly");
            let react_agent = ReActAgent::new(
                self.provider.clone(),
                self.tools.clone(),
                self.parser.clone(),
                self.approval_gate.clone(),
                self.config.react_config.clone(),
            );
            let result = react_agent.run(conv).await?;
            total_iterations = result.iterations;
            conv = result.conversation.clone();
            react_result = Some(result);
        }

        // Step 5: Reflect on the final result
        if self.config.use_reflection {
            tracing::info!("AgentRunner: Starting reflection phase");
            let reflection_agent = ReflectionAgent::new(
                self.provider.clone(),
                self.config.reflection_config.clone(),
            );

            let final_text = conv.messages.iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.text_content())
                .unwrap_or_default();

            let reflection = reflection_agent.reflect(task, &final_text, Some(&conv)).await?;
            reflection_result = Some(reflection);
        }

        // Get the final answer
        let final_answer = conv.messages.iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.text_content())
            .unwrap_or_default();

        let success = reflection_result.as_ref()
            .map(|r| r.passed)
            .unwrap_or(true); // If no reflection, assume success

        Ok(AgentRunnerResult {
            conversation: conv,
            final_answer,
            global_state,
            plan: plan_result,
            react_result,
            parallel_result,
            reflection_result,
            success,
            total_iterations,
        })
    }

    /// Run a simple task without planning or reflection.
    ///
    /// This is equivalent to just running a ReAct agent directly.
    pub async fn run_simple(&self, task: &str) -> Result<AgentRunnerResult> {
        let mut conv = Conversation::new();
        conv.add_message(Message::system(self.config.system_prompt.clone()));
        conv.add_message(Message::user(task.to_string()));

        let react_agent = ReActAgent::new(
            self.provider.clone(),
            self.tools.clone(),
            self.parser.clone(),
            self.approval_gate.clone(),
            self.config.react_config.clone(),
        );

        let result = react_agent.run(conv).await?;
        let final_answer = result.final_message.text_content();
        let total_iterations = result.iterations;
        let conversation = result.conversation.clone();

        Ok(AgentRunnerResult {
            conversation,
            final_answer,
            global_state: GlobalState::new(),
            plan: None,
            react_result: Some(result),
            parallel_result: None,
            reflection_result: None,
            success: true,
            total_iterations,
        })
    }
}