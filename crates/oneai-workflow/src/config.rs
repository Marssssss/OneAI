//! Workflow configuration DSL — defines workflow steps, dependencies, and tool bindings.
//!
//! A workflow is defined as a set of steps with dependencies between them.
//! The configuration specifies:
//! - Steps: each step has an ID, description, and optional tool bindings
//! - Dependencies: which steps must complete before others can start
//! - Variables: shared context variables for passing data between steps
//! - Retry policy: how to handle step failures
//! - Timeout: maximum time for the entire workflow or individual steps

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A workflow configuration — the user-facing definition of a workflow.
///
/// Workflows are defined in YAML/JSON and compiled into a DAG for execution.
/// Each step specifies what it does and what it depends on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowConfig {
    /// Unique workflow name/identifier.
    pub name: String,

    /// Human-readable description.
    #[serde(default)]
    pub description: String,

    /// Version of this workflow configuration.
    #[serde(default = "default_version")]
    pub version: String,

    /// The steps in this workflow.
    pub steps: Vec<StepConfig>,

    /// Shared context variables (initial values).
    #[serde(default)]
    pub variables: HashMap<String, String>,

    /// Workflow-level timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Default retry policy for steps (can be overridden per-step).
    #[serde(default)]
    pub default_retry_policy: RetryPolicy,

    /// Whether to continue on step failure.
    #[serde(default)]
    pub continue_on_failure: bool,
}

fn default_version() -> String {
    "1.0".to_string()
}

/// Configuration for a single workflow step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepConfig {
    /// Unique step identifier within the workflow.
    pub id: String,

    /// Human-readable description of what this step does.
    pub description: String,

    /// IDs of steps that must complete before this step can start.
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// The tool to use for this step (if applicable).
    #[serde(default)]
    pub tool: Option<String>,

    /// Arguments for the tool (JSON or template expressions).
    #[serde(default)]
    pub tool_args: Option<serde_json::Value>,

    /// The prompt template for LLM-based steps.
    #[serde(default)]
    pub prompt: Option<String>,

    /// Whether this step is an approval checkpoint (requires human approval).
    #[serde(default)]
    pub requires_approval: bool,

    /// Step-level timeout in seconds (overrides workflow timeout).
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Step-level retry policy (overrides default).
    #[serde(default)]
    pub retry_policy: Option<RetryPolicy>,

    /// Step-specific metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// Retry policy for workflow steps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryPolicy {
    /// Maximum number of retries.
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,

    /// Delay between retries in seconds.
    #[serde(default = "default_retry_delay_secs")]
    pub retry_delay_secs: u64,

    /// Whether to retry on all errors or only specific ones.
    #[serde(default)]
    pub retry_on_all_errors: bool,
}

fn default_max_retries() -> usize {
    3
}

fn default_retry_delay_secs() -> u64 {
    5
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            retry_delay_secs: default_retry_delay_secs(),
            retry_on_all_errors: false,
        }
    }
}

impl WorkflowConfig {
    /// Create a new workflow config with name and steps.
    pub fn new(name: impl Into<String>, steps: Vec<StepConfig>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            version: default_version(),
            steps,
            variables: HashMap::new(),
            timeout_secs: None,
            default_retry_policy: RetryPolicy::default(),
            continue_on_failure: false,
        }
    }

    /// Parse a workflow config from JSON.
    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    /// Serialize to JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Get all step IDs.
    pub fn step_ids(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.id.clone()).collect()
    }

    /// Get a step by ID.
    pub fn get_step(&self, id: &str) -> Option<&StepConfig> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Get the effective retry policy for a step.
    pub fn effective_retry_policy(&self, step_id: &str) -> RetryPolicy {
        self.get_step(step_id)
            .and_then(|s| s.retry_policy.clone())
            .unwrap_or_else(|| self.default_retry_policy.clone())
    }

    /// Get the effective timeout for a step.
    pub fn effective_timeout(&self, step_id: &str) -> Option<u64> {
        self.get_step(step_id)
            .and_then(|s| s.timeout_secs)
            .or(self.timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_config_creation() {
        let config = WorkflowConfig::new("test_workflow", vec![
            StepConfig {
                id: "step1".to_string(),
                description: "First step".to_string(),
                depends_on: vec![],
                tool: None,
                tool_args: None,
                prompt: Some("Do something".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "step2".to_string(),
                description: "Second step".to_string(),
                depends_on: vec!["step1".to_string()],
                tool: Some("calculator".to_string()),
                tool_args: Some(serde_json::json!({"expression": "2+2"})),
                prompt: None,
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ]);

        assert_eq!(config.name, "test_workflow");
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.step_ids(), vec!["step1", "step2"]);
    }

    #[test]
    fn test_workflow_config_serialization() {
        let config = WorkflowConfig::new("test_workflow", vec![
            StepConfig {
                id: "step1".to_string(),
                description: "First step".to_string(),
                depends_on: vec![],
                tool: None,
                tool_args: None,
                prompt: Some("Calculate something".to_string()),
                requires_approval: false,
                timeout_secs: Some(30),
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ]);

        let json = config.to_json().unwrap();
        let parsed: WorkflowConfig = WorkflowConfig::from_json(&json).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn test_effective_retry_policy() {
        let config = WorkflowConfig::new("test", vec![
            StepConfig {
                id: "step1".to_string(),
                description: "Step 1".to_string(),
                depends_on: vec![],
                tool: None,
                tool_args: None,
                prompt: None,
                requires_approval: false,
                timeout_secs: None,
                retry_policy: Some(RetryPolicy { max_retries: 5, retry_delay_secs: 10, retry_on_all_errors: true }),
                metadata: HashMap::new(),
            },
            StepConfig {
                id: "step2".to_string(),
                description: "Step 2".to_string(),
                depends_on: vec!["step1".to_string()],
                tool: None,
                tool_args: None,
                prompt: None,
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ]);

        let policy1 = config.effective_retry_policy("step1");
        assert_eq!(policy1.max_retries, 5);

        let policy2 = config.effective_retry_policy("step2");
        assert_eq!(policy2.max_retries, 3);
    }

    #[test]
    fn test_effective_timeout() {
        let mut config = WorkflowConfig::new("test", vec![
            StepConfig {
                id: "step1".to_string(),
                description: "Step 1".to_string(),
                depends_on: vec![],
                tool: None,
                tool_args: None,
                prompt: None,
                requires_approval: false,
                timeout_secs: Some(60),
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ]);
        config.timeout_secs = Some(120);

        assert_eq!(config.effective_timeout("step1"), Some(60));
        assert_eq!(config.effective_timeout("nonexistent"), Some(120));
    }
}