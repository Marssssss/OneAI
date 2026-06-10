//! Parallel Executor — runs non-coupled sub-agent steps concurrently with isolated ScopeState.
//!
//! Each sub-agent gets an isolated ScopeState (read-only global memory + private sandbox).
//! After all sub-agents complete, their reductions are merged back via StateReducer.
//!
//! This implements the MVI/Redux pattern for state isolation in concurrent agent execution:
//! - Sub-agents clone read-only global memory (no writes to global state during execution)
//! - Sub-agents run in private Sandbox Scope with local mutations
//! - Results are accumulated as Reductions
//! - After all sub-agents finish, StateReducer merges Reductions back into GlobalState

use std::sync::Arc;
use tokio::task::JoinSet;

use oneai_core::{ContentBlock, GlobalState, Reduction};
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::StateReducer;

use crate::scope_state::ScopeState;
use crate::plan_agent::PlanStep;

/// Result of a parallel execution step.
#[derive(Debug, Clone)]
pub struct ParallelStepResult {
    /// The step ID that was executed.
    pub step_id: String,

    /// The result content from this step.
    pub result: String,

    /// Whether this step completed successfully.
    pub success: bool,

    /// The reductions produced by this step.
    pub reductions: Vec<Reduction>,
}

/// Result of the entire parallel execution.
#[derive(Debug, Clone)]
pub struct ParallelResult {
    /// Results from each parallel step.
    pub step_results: Vec<ParallelStepResult>,

    /// The global state after all reductions are merged.
    pub global_state: GlobalState,
}

/// Parallel executor for non-coupled sub-agent steps.
///
/// Runs multiple independent steps concurrently using isolated ScopeState.
/// After all steps complete, merges their reductions into the global state.
pub struct ParallelExecutor {
    /// The state reducer for merging sub-agent results.
    reducer: Arc<dyn StateReducer>,
}

impl ParallelExecutor {
    /// Create a new parallel executor with the given reducer.
    pub fn new(reducer: Arc<dyn StateReducer>) -> Self {
        Self { reducer }
    }

    /// Create with the default state reducer.
    pub fn with_defaults() -> Self {
        Self::new(Arc::new(crate::scope_state::DefaultStateReducer))
    }

    /// Execute non-coupled steps in parallel.
    ///
    /// Takes a list of PlanSteps (must all be non-coupled),
    /// and executes them concurrently with isolated ScopeState.
    ///
    /// Each step is executed by calling the provided executor function,
    /// which typically wraps a ReActAgent or simple LLM call.
    ///
    /// After all steps complete, reductions are merged into the global state.
    pub async fn execute_parallel<F, Fut>(
        &self,
        steps: &[PlanStep],
        global_state: &GlobalState,
        step_executor: F,
    ) -> Result<ParallelResult>
    where
        F: Fn(PlanStep, ScopeState) -> Fut + Clone + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<ParallelStepResult>> + Send + 'static,
    {
        // Validate that all steps are non-coupled
        for step in steps {
            if step.coupled {
                return Err(OneAIError::Agent(format!(
                    "ParallelExecutor cannot execute coupled step '{}' — use ReActAgent instead",
                    step.id
                )));
            }
        }

        // Create isolated ScopeState for each sub-agent
        let mut join_set = JoinSet::new();

        for step in steps {
            let scope_state = ScopeState::from_global(global_state);
            let executor = step_executor.clone();
            let step = step.clone();

            join_set.spawn(async move {
                executor(step, scope_state).await
            });
        }

        // Collect all results
        let mut step_results = Vec::new();
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(step_result) => {
                    // step_result is Result<ParallelStepResult, OneAIError>
                    match step_result {
                        Ok(r) => step_results.push(r),
                        Err(e) => {
                            tracing::error!("Parallel step failed: {:?}", e);
                            // Continue collecting other results
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Parallel step task failed: {:?}", e);
                }
            }
        }

        // Merge reductions into global state
        let mut global_state = global_state.clone();
        let all_reductions: Vec<Reduction> = step_results.iter()
            .flat_map(|r| r.reductions.clone())
            .collect();

        if !all_reductions.is_empty() {
            self.reducer.reduce(&mut global_state, all_reductions)?;
        }

        Ok(ParallelResult {
            step_results,
            global_state,
        })
    }

    /// Execute steps with a simple string-based executor (no tool calls).
    ///
    /// This is a convenience method for simple parallel LLM calls
    /// where each step is just a prompt to the model.
    pub async fn execute_simple<F, Fut>(
        &self,
        steps: &[PlanStep],
        global_state: &GlobalState,
        executor: F,
    ) -> Result<ParallelResult>
    where
        F: Fn(String, String) -> Fut + Clone + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        self.execute_parallel(steps, global_state, move |step, _scope| {
            let executor = executor.clone();
            async move {
                let step_id = step.id.clone();
                let description = step.description.clone();

                let result_text = executor(step_id.clone(), description).await?;

                // Create a reduction for this step's result
                let reduction = Reduction::SetResult {
                    step_id: step_id.clone(),
                    result: ContentBlock::Text { text: result_text.clone() },
                };

                Ok(ParallelStepResult {
                    step_id,
                    result: result_text,
                    success: true,
                    reductions: vec![reduction],
                })
            }
        }).await
    }
}