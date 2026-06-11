//! Workflow executor — runs compiled DAG with level-based parallel execution.
//!
//! The executor runs a WorkflowDag by:
//! 1. Processing levels sequentially (level 0 → level 1 → ... → level N)
//! 2. Within each level, running all steps concurrently (since they have no mutual dependencies)
//! 3. Tracking step results and passing context between levels
//! 4. Handling timeouts, retries, and approval gates
//!
//! Step results are accumulated in a WorkflowContext that flows between levels.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{ApprovalGate, Tool};

use crate::dag::WorkflowDag;
use crate::config::{WorkflowConfig, RetryPolicy};

/// The status of a workflow step execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum StepStatus {
    /// Not yet started.
    Pending,
    /// Currently running.
    Running,
    /// Completed successfully.
    Completed,
    /// Failed (with optional error message).
    Failed,
    /// Skipped (a dependency failed and continue_on_failure is false).
    Skipped,
}

/// The result of a single workflow step execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    /// The step ID.
    pub step_id: String,

    /// The status of this step.
    pub status: StepStatus,

    /// The output content (if completed).
    pub output: Option<String>,

    /// Error message (if failed).
    pub error: Option<String>,

    /// How many retries were used.
    pub retries_used: usize,

    /// Execution time in milliseconds.
    pub execution_time_ms: Option<u64>,
}

/// The result of the entire workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowResult {
    /// The workflow name.
    pub name: String,

    /// The results of each step.
    pub step_results: HashMap<String, StepResult>,

    /// The final context variables after all steps.
    pub variables: HashMap<String, String>,

    /// Whether the workflow completed successfully.
    pub success: bool,

    /// Total execution time in milliseconds.
    pub total_time_ms: u64,

    /// Which level was being executed when the workflow ended.
    pub completed_levels: usize,
}

impl WorkflowResult {
    /// Get a step result by ID.
    pub fn get_step_result(&self, step_id: &str) -> Option<&StepResult> {
        self.step_results.get(step_id)
    }

    /// Get all completed step IDs.
    pub fn completed_steps(&self) -> Vec<String> {
        self.step_results.iter()
            .filter(|(_, r)| r.status == StepStatus::Completed)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get all failed step IDs.
    pub fn failed_steps(&self) -> Vec<String> {
        self.step_results.iter()
            .filter(|(_, r)| r.status == StepStatus::Failed)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// Workflow execution context — shared state that flows between levels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowContext {
    /// Context variables (initially from WorkflowConfig, updated by steps).
    pub variables: HashMap<String, String>,
    /// Results from completed steps (available to subsequent steps).
    pub step_outputs: HashMap<String, String>,
}

impl WorkflowContext {
    /// Create a context from workflow config variables.
    pub fn from_config(config: &WorkflowConfig) -> Self {
        Self {
            variables: config.variables.clone(),
            step_outputs: HashMap::new(),
        }
    }

    /// Set a variable.
    pub fn set_variable(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.variables.insert(key.into(), value.into());
    }

    /// Get a variable.
    pub fn get_variable(&self, key: &str) -> Option<&String> {
        self.variables.get(key)
    }

    /// Store a step's output.
    pub fn set_step_output(&mut self, step_id: impl Into<String>, output: impl Into<String>) {
        self.step_outputs.insert(step_id.into(), output.into());
    }
}

/// Workflow executor — runs a compiled DAG.
///
/// The executor processes the DAG level by level:
/// - Level 0: all root nodes run concurrently
/// - Level 1: nodes that depend only on level 0 nodes run concurrently
/// - Level N: nodes that depend only on earlier levels run concurrently
///
/// Between levels, the WorkflowContext is updated with step results.
pub struct WorkflowExecutor {
    /// The tool registry for executing tool-based steps.
    tools: Arc<HashMap<String, Arc<dyn Tool>>>,
    /// The approval gate for high-risk tool approval.
    approval_gate: Arc<dyn ApprovalGate>,
}

impl WorkflowExecutor {
    /// Create a new executor with tools and approval gate.
    pub fn new(
        tools: Arc<HashMap<String, Arc<dyn Tool>>>,
        approval_gate: Arc<dyn ApprovalGate>,
    ) -> Self {
        Self {
            tools,
            approval_gate,
        }
    }

    /// Execute a workflow DAG.
    ///
    /// Runs the DAG level by level, with steps at the same level
    /// executing concurrently. Returns a WorkflowResult with the
    /// status of each step.
    pub async fn execute(
        &self,
        dag: &WorkflowDag,
        config: &WorkflowConfig,
    ) -> Result<WorkflowResult> {
        let start_time = std::time::Instant::now();
        let mut step_results: HashMap<String, StepResult> = HashMap::new();
        let mut context = WorkflowContext::from_config(config);
        let mut completed_levels = 0;

        // Mark all steps as pending initially
        for (id, _) in &dag.nodes {
            step_results.insert(id.clone(), StepResult {
                step_id: id.clone(),
                status: StepStatus::Pending,
                output: None,
                error: None,
                retries_used: 0,
                execution_time_ms: None,
            });
        }

        // Execute each level
        for level_ids in &dag.levels {
            let level_start = std::time::Instant::now();

            // Check if any dependencies failed — skip if not continue_on_failure
            let mut skip_level = false;
            if !config.continue_on_failure {
                for step_id in level_ids {
                    if let Some(node) = dag.get_node(step_id) {
                        for dep_id in &node.depends_on {
                            if let Some(dep_result) = step_results.get(dep_id) {
                                if dep_result.status == StepStatus::Failed || dep_result.status == StepStatus::Skipped {
                                    skip_level = true;
                                    break;
                                }
                            }
                        }
                    }
                    if skip_level {
                        break;
                    }
                }
            }

            if skip_level {
                // Skip all steps in this level
                for step_id in level_ids {
                    if let Some(result) = step_results.get_mut(step_id) {
                        result.status = StepStatus::Skipped;
                        result.error = Some("Dependency failed — step skipped".to_string());
                    }
                }
                continue;
            }

            // Execute all steps in this level concurrently
            let mut level_futures = Vec::new();

            for step_id in level_ids {
                let node = dag.get_node(step_id).unwrap();
                let step_config = node.step.clone();
                let retry_policy = config.effective_retry_policy(step_id);
                let timeout_secs = config.effective_timeout(step_id);
                let tools = self.tools.clone();
                let approval_gate = self.approval_gate.clone();
                let context_snapshot = context.clone();

                level_futures.push(tokio::spawn(async move {
                    execute_step(
                        step_config,
                        retry_policy,
                        timeout_secs,
                        tools,
                        approval_gate,
                        context_snapshot,
                    ).await
                }));
            }

            // Collect results from all steps in this level
            for future in level_futures {
                let result = future.await.map_err(|e| {
                    OneAIError::Workflow(format!("Task join error: {}", e))
                })?;

                match result {
                    Ok(step_result) => {
                        step_results.insert(step_result.step_id.clone(), step_result.clone());
                        if step_result.status == StepStatus::Completed {
                            if let Some(output) = &step_result.output {
                                context.set_step_output(&step_result.step_id, output);
                            }
                        }
                    }
                    Err(e) => {
                        // Step execution error
                        tracing::error!("Step execution error: {}", e);
                    }
                }
            }

            completed_levels += 1;
        }

        let total_time_ms = start_time.elapsed().as_millis() as u64;

        let success = step_results.values()
            .all(|r| r.status == StepStatus::Completed || r.status == StepStatus::Skipped);

        Ok(WorkflowResult {
            name: dag.name.clone(),
            step_results,
            variables: context.variables,
            success,
            total_time_ms,
            completed_levels,
        })
    }
}

/// Execute a single workflow step with retry support.
async fn execute_step(
    step: crate::config::StepConfig,
    retry_policy: RetryPolicy,
    timeout_secs: Option<u64>,
    tools: Arc<HashMap<String, Arc<dyn Tool>>>,
    approval_gate: Arc<dyn ApprovalGate>,
    context: WorkflowContext,
) -> Result<StepResult> {
    let start_time = std::time::Instant::now();
    let step_id = step.id.clone();
    let mut retries_used = 0;
    let mut last_error: Option<String> = None;

    // If this step requires approval, request it
    if step.requires_approval {
        let approval_request = oneai_core::ApprovalRequest {
            tool_name: step.id.clone(),
            args: step.tool_args.clone().unwrap_or(serde_json::json!({})),
            risk_level: oneai_core::RiskLevel::High,
            permission_level: Some(oneai_core::PermissionLevel::Full),
            justification: format!("Workflow step '{}' requires human approval", step.id),
        };

        let approval_response = approval_gate.request_approval(approval_request).await?;
        match approval_response {
            oneai_core::ApprovalResponse::Denied { reason } => {
                return Ok(StepResult {
                    step_id,
                    status: StepStatus::Failed,
                    output: None,
                    error: Some(format!("Approval denied: {}", reason)),
                    retries_used: 0,
                    execution_time_ms: Some(start_time.elapsed().as_millis() as u64),
                });
            }
            oneai_core::ApprovalResponse::Approved { modified_args } => {
                // Use modified args if provided
                if let Some(modified) = modified_args {
                    // Update step args with modified version
                    // (In a real implementation, we'd merge modified args into tool_args)
                }
            }
            oneai_core::ApprovalResponse::Modified { args } => {
                // Use modified args
            }
            oneai_core::ApprovalResponse::Observe { observation } => {
                // Observe mode — pause for human inspection
                // In workflow context, treat observation as a pause/resume signal
                tracing::info!("Workflow step '{}' paused for observation: {}", step_id, observation);
            }
        }
    }

    // Execute with retry loop
    for attempt in 0..=retry_policy.max_retries {
        let exec_result: std::result::Result<oneai_core::ToolOutput, OneAIError> = if let Some(tool_name) = &step.tool {
            // Tool-based step
            let tool = tools.get(tool_name);
            if let Some(tool) = tool {
                let args = step.tool_args.clone().unwrap_or(serde_json::json!({}));
                let timeout = timeout_secs.unwrap_or(60);

                let timeout_result = tokio::time::timeout(
                    std::time::Duration::from_secs(timeout),
                    tool.execute(args),
                ).await;

                match timeout_result {
                    Ok(inner_result) => inner_result, // Result<ToolOutput, OneAIError>
                    Err(_) => Err(OneAIError::Timeout(format!(
                        "Step '{}' timed out after {} seconds", step_id, timeout
                    ))),
                }
            } else {
                Err(OneAIError::Workflow(format!(
                    "Tool '{}' not found for step '{}'",
                    tool_name, step_id
                )))
            }
        } else {
            // No tool defined — just mark as completed with prompt
            Ok(oneai_core::ToolOutput {
                success: true,
                content: step.prompt.clone().unwrap_or_default(),
                error: None,
            })
        };

        match exec_result {
            Ok(output) => {
                if output.success {
                    return Ok(StepResult {
                        step_id,
                        status: StepStatus::Completed,
                        output: Some(output.content),
                        error: None,
                        retries_used,
                        execution_time_ms: Some(start_time.elapsed().as_millis() as u64),
                    });
                } else {
                    last_error = Some(output.error.unwrap_or_else(|| "Tool execution failed".to_string()));
                }
            }
            Err(e) => {
                last_error = Some(format!("Step error: {}", e));
            }
        }

        retries_used = attempt;

        if attempt < retry_policy.max_retries {
            // Wait before retry
            tokio::time::sleep(
                std::time::Duration::from_secs(retry_policy.retry_delay_secs)
            ).await;
        }
    }

    // All retries exhausted
    Ok(StepResult {
        step_id,
        status: StepStatus::Failed,
        output: None,
        error: last_error,
        retries_used,
        execution_time_ms: Some(start_time.elapsed().as_millis() as u64),
    })
}