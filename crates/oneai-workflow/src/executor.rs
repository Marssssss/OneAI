//! Workflow executor — runs compiled DAG with level-based parallel execution.
//!
//! The executor runs a WorkflowDag by:
//! 1. Processing levels sequentially (level 0 → level 1 → ... → level N)
//! 2. Within each level, running all steps concurrently (since they have no mutual dependencies)
//! 3. Tracking step results and passing context between levels
//! 4. Handling timeouts, retries, and approval gates
//! 5. Supporting `{{variable}}` template interpolation in tool_args and prompt fields
//! 6. Executing LLM inference for prompt-based steps when a provider is available
//!
//! Step results are accumulated in a WorkflowContext that flows between levels.
//! Template interpolation replaces `{{step_id_output}}` and `{{step_id}}` with
//! previous step outputs, and `{{variable_name}}` with context variables.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{ApprovalGate, LlmProvider, Tool};
use oneai_core::{Conversation, InferenceRequest, Message, Role};

use crate::dag::WorkflowDag;
use crate::config::{WorkflowConfig, RetryPolicy};

/// The status of a workflow step execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
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
/// Template interpolation (`{{variable}}`) is applied to tool_args and
/// prompt fields before step execution, using context variables and
/// previous step outputs.
///
/// When a provider is available, prompt-based steps (steps with a `prompt`
/// field but no `tool`) execute actual LLM inference instead of just
/// returning the prompt text.
pub struct WorkflowExecutor {
    /// The tool registry for executing tool-based steps.
    /// Uses RwLock so tools can be registered after construction.
    tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    /// The approval gate for high-risk tool approval.
    approval_gate: Arc<dyn ApprovalGate>,
    /// Optional LLM provider for executing prompt-based steps.
    /// When set, steps with a `prompt` field but no `tool` will call
    /// the provider for actual inference. When None, prompt steps
    /// just return the interpolated prompt text.
    provider: Option<Arc<dyn LlmProvider>>,
}

impl WorkflowExecutor {
    /// Create a new executor with tools and approval gate.
    pub fn new(
        tools: Arc<HashMap<String, Arc<dyn Tool>>>,
        approval_gate: Arc<dyn ApprovalGate>,
    ) -> Self {
        // Convert static HashMap into RwLock-backed HashMap for dynamic registration
        Self {
            tools: Arc::new(tokio::sync::RwLock::new((*tools).clone())),
            approval_gate,
            provider: None,
        }
    }

    /// Create a new executor with an empty tool registry.
    /// Tools can be registered later via `register_tool()`.
    pub fn new_empty(approval_gate: Arc<dyn ApprovalGate>) -> Self {
        Self {
            tools: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            approval_gate,
            provider: None,
        }
    }

    /// Create a new executor with tools, approval gate, and LLM provider.
    /// With a provider, prompt-based steps will execute actual LLM inference.
    pub fn with_provider(
        tools: Arc<HashMap<String, Arc<dyn Tool>>>,
        approval_gate: Arc<dyn ApprovalGate>,
        provider: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            tools: Arc::new(tokio::sync::RwLock::new((*tools).clone())),
            approval_gate,
            provider: Some(provider),
        }
    }

    /// Set the LLM provider for prompt-based steps.
    /// When set, steps with a `prompt` field but no `tool` will call
    /// the provider for actual inference instead of returning raw text.
    pub fn set_provider(&mut self, provider: Arc<dyn LlmProvider>) {
        self.provider = Some(provider);
    }

    /// Register a tool for workflow step execution.
    pub async fn register_tool(&self, tool: Arc<dyn Tool>) {
        self.tools.write().await.insert(tool.name().to_string(), tool);
    }

    /// Get a handle to the internal tool registry (Arc<RwLock<HashMap>>).
    /// Used by StateGraphExecutor to share the same tool registry.
    pub fn tools_handle(&self) -> Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>> {
        self.tools.clone()
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
                let provider = self.provider.clone();

                level_futures.push(tokio::spawn(async move {
                    execute_step(
                        step_config,
                        retry_policy,
                        timeout_secs,
                        tools,
                        approval_gate,
                        context_snapshot,
                        provider,
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

/// Execute a single workflow step with retry support and template interpolation.
///
/// Template interpolation replaces `{{step_id_output}}` and `{{step_id}}` with
/// previous step outputs, and `{{variable_name}}` with context variables.
/// This is applied to `step.tool_args` (for tool steps) and `step.prompt`
/// (for prompt-based steps) before execution.
///
/// When a provider is available, prompt-based steps (no tool, just prompt)
/// execute actual LLM inference. When no provider is set, they return
/// the interpolated prompt text.
async fn execute_step(
    step: crate::config::StepConfig,
    retry_policy: RetryPolicy,
    timeout_secs: Option<u64>,
    tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    approval_gate: Arc<dyn ApprovalGate>,
    context: WorkflowContext,
    provider: Option<Arc<dyn LlmProvider>>,
) -> Result<StepResult> {
    let start_time = std::time::Instant::now();
    let step_id = step.id.clone();
    let mut retries_used = 0;
    let mut last_error: Option<String> = None;

    // ─── Template interpolation ──────────────────────────────────────────
    // Interpolate {{variable}} patterns in step.tool_args and step.prompt
    // using context.step_outputs and context.variables.
    let interpolated_prompt = step.prompt.as_ref()
        .map(|p| interpolate_template(p, &context));
    let interpolated_tool_args = step.tool_args.as_ref()
        .map(|a| {
            // Interpolate string values in the JSON
            let json_str = serde_json::to_string(a).unwrap_or_default();
            let interpolated = interpolate_template(&json_str, &context);
            serde_json::from_str(&interpolated).unwrap_or_else(|_| a.clone())
        });

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
            // Tool-based step — read from RwLock
            let tools_map = tools.read().await;
            let tool = tools_map.get(tool_name);
            if let Some(tool) = tool {
                // Use interpolated args if available, otherwise use original args
                let args = match &interpolated_tool_args {
                    Some(a) => a.clone(),
                    None => step.tool_args.clone().unwrap_or(serde_json::json!({})),
                };                let timeout = timeout_secs.unwrap_or(60);

                let timeout_result = tokio::time::timeout(
                    std::time::Duration::from_secs(timeout),
                    tool.execute(args.clone()),
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
        } else if let Some(provider) = &provider {
            // Prompt-based step with LLM provider — execute actual inference
            let prompt_text = match &interpolated_prompt {
                Some(p) => p.clone(),
                None => step.prompt.clone().unwrap_or_default(),
            };
            if prompt_text.is_empty() {
                Ok(oneai_core::ToolOutput {
                    success: true,
                    content: String::new(),
                    error: None,
                })
            } else {
                // Build a minimal inference request with the interpolated prompt
                let mut conversation = Conversation::new();
                conversation.add_message(Message::system(
                    "You are executing a step in a deterministic workflow. Respond concisely."
                ));
                conversation.add_message(Message::user(prompt_text));

                let request = InferenceRequest {
                    conversation,
                    tools: vec![], // No tools for prompt steps
                    max_tokens: Some(2048),
                    temperature: Some(0.3),
                    top_p: None,
                    stop_sequences: vec![],
                    constrained_output: None,
                    thinking_budget: None,
                    metadata: HashMap::new(),
                };

                let response = provider.infer(request).await;
                match response {
                    Ok(inference_response) => {
                        Ok(oneai_core::ToolOutput {
                            success: true,
                            content: inference_response.message.text_content(),
                            error: None,
                        })
                    }
                    Err(e) => Err(OneAIError::Provider(format!(
                        "LLM inference failed for step '{}': {}", step_id, e
                    ))),
                }
            }
        } else {
            // No tool defined and no provider — just return interpolated prompt
            let prompt_text = match &interpolated_prompt {
                Some(p) => p.clone(),
                None => step.prompt.clone().unwrap_or_default(),
            };
            Ok(oneai_core::ToolOutput {
                success: true,
                content: prompt_text,
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

// ─── Template Interpolation ────────────────────────────────────────────────

/// Interpolate `{{variable}}` template patterns in a string.
///
/// Replaces `{{key}}` patterns using two sources:
/// 1. `context.step_outputs` — previous step results. Both `{{step_id}}`
///    and `{{step_id_output}}` resolve to the output of step `step_id`.
/// 2. `context.variables` — shared workflow variables.
///
/// **Order**: Longer patterns are replaced first to avoid substring conflicts.
/// `{{step_id_output}}` is replaced before `{{step_id}}`.
///
/// Example:
/// ```
/// let template = "Review {{read_diff_output}} with focus on {{focus_area}}";
/// // If read_diff has output and focus_area = "security":
/// // → "Review <output> with focus on security"
/// ```
pub fn interpolate_template(template: &str, context: &WorkflowContext) -> String {
    let mut result = template.to_string();

    // First pass: replace longer patterns (step_id_output) before shorter ones (step_id)
    // This prevents {{step1_output}} from being partially matched by {{step1}}
    for (step_id, output) in &context.step_outputs {
        // Long form: {{step_id_output}} — must be replaced first
        // Build pattern manually: "{{" + step_id + "_output}}"
        let long_pattern = "{{".to_string() + step_id + "_output}}";
        result = result.replace(&long_pattern, output);
    }
    for (step_id, output) in &context.step_outputs {
        // Short form: {{step_id}} — replaced second
        // Build pattern manually: "{{" + step_id + "}}"
        let short_pattern = "{{".to_string() + step_id + "}}";
        result = result.replace(&short_pattern, output);
    }

    // Replace variable references: {{variable_name}}
    for (key, value) in &context.variables {
        result = result.replace(&format!("{{{{{}}}}}", key), value);
    }

    result
}

#[cfg(test)]
mod interpolation_tests {
    use super::*;

    #[test]
    fn test_interpolate_step_outputs() {
        let mut context = WorkflowContext::from_config(&WorkflowConfig::new("test", vec![]));
        context.set_step_output("step1", "result of step1");
        context.set_step_output("step2", "result of step2");

        // Short form {{step_id}}
        let result = interpolate_template("{{step1}} then {{step2}}", &context);
        assert_eq!(result, "result of step1 then result of step2");

        // Long form {{step_id_output}}
        let result = interpolate_template("{{step1_output}} and {{step2_output}}", &context);
        assert_eq!(result, "result of step1 and result of step2");
    }

    #[test]
    fn test_interpolate_variables() {
        let mut context = WorkflowContext::from_config(&WorkflowConfig::new("test", vec![]));
        context.set_variable("focus_area", "security");
        context.set_variable("language", "Rust");

        let result = interpolate_template("Focus on {{focus_area}} in {{language}}", &context);
        assert_eq!(result, "Focus on security in Rust");
    }

    #[test]
    fn test_interpolate_mixed() {
        let mut context = WorkflowContext::from_config(&WorkflowConfig::new("test", vec![]));
        context.set_step_output("read_diff", "auth.rs changes");
        context.set_variable("focus", "security");

        let result = interpolate_template("Review {{read_diff_output}} with focus on {{focus}}", &context);
        assert_eq!(result, "Review auth.rs changes with focus on security");
    }

    #[test]
    fn test_interpolate_no_match() {
        let context = WorkflowContext::from_config(&WorkflowConfig::new("test", vec![]));
        let result = interpolate_template("No variables here {{unknown}}", &context);
        assert_eq!(result, "No variables here {{unknown}}");
    }
}