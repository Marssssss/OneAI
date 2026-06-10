//! Workflow validator — detects cycles, orphan nodes, undefined dependencies.
//!
//! Validates a compiled WorkflowDAG before execution to prevent errors:
//! - Cycle detection: no circular dependencies
//! - Orphan detection: no unreachable nodes
//! - Dependency integrity: all `depends_on` references are valid step IDs
//! - Empty step detection: steps must have either a tool or a prompt
//! - Duplicate ID detection: no two steps can share an ID

use crate::dag::WorkflowDag;
use crate::config::WorkflowConfig;

/// A validation issue found in the workflow.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationIssue {
    /// The severity of the issue.
    pub severity: ValidationSeverity,

    /// The issue code (for programmatic handling).
    pub code: ValidationCode,

    /// A description of the issue.
    pub description: String,

    /// The step ID(s) involved (if applicable).
    pub step_ids: Vec<String>,
}

/// Severity of a validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationSeverity {
    /// Critical — the workflow cannot be executed.
    Error,
    /// Warning — the workflow can be executed but may have problems.
    Warning,
}

/// Code identifying the type of validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationCode {
    /// Circular dependency detected.
    CycleDetected,
    /// A step references a dependency that doesn't exist.
    UndefinedDependency,
    /// Duplicate step IDs.
    DuplicateStepId,
    /// A step has no tool or prompt defined.
    EmptyStep,
    /// A step is unreachable (no path from root).
    OrphanNode,
    /// A step depends on itself.
    SelfDependency,
    /// Missing approval gate for high-risk tool.
    MissingApprovalGate,
}

/// Result of workflow validation.
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether the workflow is valid (no errors).
    pub is_valid: bool,

    /// All issues found during validation.
    pub issues: Vec<ValidationIssue>,
}

impl ValidationResult {
    /// Create a valid result with no issues.
    pub fn valid() -> Self {
        Self {
            is_valid: true,
            issues: Vec::new(),
        }
    }

    /// Create an invalid result with issues.
    pub fn with_issues(issues: Vec<ValidationIssue>) -> Self {
        let has_errors = issues.iter().any(|i| i.severity == ValidationSeverity::Error);
        Self {
            is_valid: !has_errors,
            issues,
        }
    }

    /// Get only error-level issues.
    pub fn errors(&self) -> Vec<&ValidationIssue> {
        self.issues.iter().filter(|i| i.severity == ValidationSeverity::Error).collect()
    }

    /// Get only warning-level issues.
    pub fn warnings(&self) -> Vec<&ValidationIssue> {
        self.issues.iter().filter(|i| i.severity == ValidationSeverity::Warning).collect()
    }
}

/// Validate a WorkflowConfig before compilation.
///
/// Checks for:
/// - Duplicate step IDs
/// - Undefined dependencies (references to non-existent steps)
/// - Self-dependencies
/// - Empty steps (no tool or prompt)
pub fn validate_config(config: &WorkflowConfig) -> ValidationResult {
    let mut issues = Vec::new();

    // Check for duplicate step IDs
    let mut seen_ids = std::collections::HashSet::new();
    for step in &config.steps {
        if seen_ids.contains(&step.id) {
            issues.push(ValidationIssue {
                severity: ValidationSeverity::Error,
                code: ValidationCode::DuplicateStepId,
                description: format!("Duplicate step ID: '{}'", step.id),
                step_ids: vec![step.id.clone()],
            });
        }
        seen_ids.insert(step.id.clone());
    }

    let step_ids: std::collections::HashSet<String> = config.steps.iter()
        .map(|s| s.id.clone())
        .collect();

    // Check for undefined dependencies
    for step in &config.steps {
        for dep_id in &step.depends_on {
            if !step_ids.contains(dep_id) {
                issues.push(ValidationIssue {
                    severity: ValidationSeverity::Error,
                    code: ValidationCode::UndefinedDependency,
                    description: format!(
                        "Step '{}' depends on undefined step '{}'",
                        step.id, dep_id
                    ),
                    step_ids: vec![step.id.clone(), dep_id.clone()],
                });
            }
        }
    }

    // Check for self-dependencies
    for step in &config.steps {
        if step.depends_on.contains(&step.id) {
            issues.push(ValidationIssue {
                severity: ValidationSeverity::Error,
                code: ValidationCode::SelfDependency,
                description: format!("Step '{}' depends on itself", step.id),
                step_ids: vec![step.id.clone()],
            });
        }
    }

    // Check for empty steps (no tool or prompt)
    for step in &config.steps {
        if step.tool.is_none() && step.prompt.is_none() && !step.requires_approval {
            issues.push(ValidationIssue {
                severity: ValidationSeverity::Warning,
                code: ValidationCode::EmptyStep,
                description: format!(
                    "Step '{}' has no tool, prompt, or approval checkpoint",
                    step.id
                ),
                step_ids: vec![step.id.clone()],
            });
        }
    }

    ValidationResult::with_issues(issues)
}

/// Validate a compiled WorkflowDAG.
///
/// Checks for:
/// - Cycles in the dependency graph
/// - Orphan nodes (unreachable from roots)
pub fn validate_dag(dag: &WorkflowDag) -> ValidationResult {
    let mut issues = Vec::new();

    // Check for cycles
    if dag.has_cycle() {
        issues.push(ValidationIssue {
            severity: ValidationSeverity::Error,
            code: ValidationCode::CycleDetected,
            description: "Workflow contains a circular dependency cycle".to_string(),
            step_ids: vec![],
        });
    }

    // Check for orphan nodes (not reachable from any root)
    if !dag.roots.is_empty() {
        // All nodes reachable from roots
        let reachable: std::collections::HashSet<String> = dag.roots.iter()
            .flat_map(|root_id| {
                let mut visited = std::collections::HashSet::new();
                let mut queue = std::collections::VecDeque::new();
                queue.push_back(root_id.clone());
                while let Some(id) = queue.pop_front() {
                    if visited.contains(&id) {
                        continue;
                    }
                    visited.insert(id.clone());
                    // Visit this node AND nodes it depends on (for full reachability)
                    if let Some(node) = dag.get_node(&id) {
                        for dep_id in &node.depends_on {
                            if !visited.contains(dep_id) {
                                queue.push_back(dep_id.clone());
                            }
                        }
                        for child_id in &node.children {
                            if !visited.contains(child_id) {
                                queue.push_back(child_id.clone());
                            }
                        }
                    }
                }
                visited
            })
            .collect();

        for (id, _) in &dag.nodes {
            if !reachable.contains(id) {
                issues.push(ValidationIssue {
                    severity: ValidationSeverity::Warning,
                    code: ValidationCode::OrphanNode,
                    description: format!("Node '{}' is unreachable from any root", id),
                    step_ids: vec![id.clone()],
                });
            }
        }
    } else if !dag.nodes.is_empty() && dag.roots.is_empty() {
        // No roots but nodes exist — likely all in a cycle
        issues.push(ValidationIssue {
            severity: ValidationSeverity::Error,
            code: ValidationCode::CycleDetected,
            description: "No root nodes found — all nodes may be in a cycle".to_string(),
            step_ids: vec![],
        });
    }

    // Check that nodes at the same level don't depend on each other
    for level in &dag.levels {
        for node_id in level {
            if let Some(node) = dag.get_node(node_id) {
                for dep_id in &node.depends_on {
                    if level.contains(dep_id) {
                        issues.push(ValidationIssue {
                            severity: ValidationSeverity::Error,
                            code: ValidationCode::CycleDetected,
                            description: format!(
                                "Node '{}' at same level as its dependency '{}'",
                                node_id, dep_id
                            ),
                            step_ids: vec![node_id.clone(), dep_id.clone()],
                        });
                    }
                }
            }
        }
    }

    ValidationResult::with_issues(issues)
}

/// Full validation: config + DAG.
pub fn validate(config: &WorkflowConfig, dag: &WorkflowDag) -> ValidationResult {
    let config_result = validate_config(config);
    let dag_result = validate_dag(dag);

    let all_issues: Vec<ValidationIssue> = config_result.issues.iter()
        .chain(dag_result.issues.iter())
        .cloned()
        .collect();

    ValidationResult::with_issues(all_issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{StepConfig, WorkflowConfig};
    use crate::dag::{DagNode, WorkflowDag};
    use crate::compiler::compile;
    use std::collections::HashMap;

    fn make_step(id: &str, depends_on: Vec<&str>) -> StepConfig {
        StepConfig {
            id: id.to_string(),
            description: format!("Step {}", id),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            tool: Some("test_tool".to_string()),
            tool_args: None,
            prompt: None,
            requires_approval: false,
            timeout_secs: None,
            retry_policy: None,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn test_validate_valid_config() {
        let config = WorkflowConfig::new("valid", vec![
            make_step("step1", vec![]),
            make_step("step2", vec!["step1"]),
        ]);

        let result = validate_config(&config);
        assert!(result.is_valid);
        assert!(result.issues.is_empty());
    }

    #[test]
    fn test_validate_duplicate_ids() {
        let config = WorkflowConfig::new("invalid", vec![
            make_step("step1", vec![]),
            make_step("step1", vec![]),
        ]);

        let result = validate_config(&config);
        assert!(!result.is_valid);
        assert!(result.issues.iter().any(|i| i.code == ValidationCode::DuplicateStepId));
    }

    #[test]
    fn test_validate_undefined_dependency() {
        let config = WorkflowConfig::new("invalid", vec![
            make_step("step1", vec!["nonexistent"]),
        ]);

        let result = validate_config(&config);
        assert!(!result.is_valid);
        assert!(result.issues.iter().any(|i| i.code == ValidationCode::UndefinedDependency));
    }

    #[test]
    fn test_validate_self_dependency() {
        let config = WorkflowConfig::new("invalid", vec![
            StepConfig {
                id: "loop".to_string(),
                description: "Self-loop".to_string(),
                depends_on: vec!["loop".to_string()],
                tool: Some("test".to_string()),
                tool_args: None,
                prompt: None,
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ]);

        let result = validate_config(&config);
        assert!(!result.is_valid);
        assert!(result.issues.iter().any(|i| i.code == ValidationCode::SelfDependency));
    }

    #[test]
    fn test_validate_empty_step_warning() {
        let config = WorkflowConfig::new("warn", vec![
            StepConfig {
                id: "empty".to_string(),
                description: "Empty step".to_string(),
                depends_on: vec![],
                tool: None,
                tool_args: None,
                prompt: None,
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
        ]);

        let result = validate_config(&config);
        // Empty step is a warning, not an error
        assert!(result.is_valid); // Warnings don't make it invalid
        assert!(result.issues.iter().any(|i| i.code == ValidationCode::EmptyStep));
    }

    #[test]
    fn test_validate_dag_cycle() {
        let mut dag = WorkflowDag::new("cycle");
        dag.add_node(DagNode {
            id: "a".to_string(),
            step: make_step("a", vec!["b"]),
            depends_on: vec!["b".to_string()],
            children: Vec::new(),
            level: 0,
        });
        dag.add_node(DagNode {
            id: "b".to_string(),
            step: make_step("b", vec!["a"]),
            depends_on: vec!["a".to_string()],
            children: Vec::new(),
            level: 0,
        });
        dag.build();

        let result = validate_dag(&dag);
        assert!(!result.is_valid);
        assert!(result.issues.iter().any(|i| i.code == ValidationCode::CycleDetected));
    }

    #[test]
    fn test_validate_dag_no_cycle() {
        let config = WorkflowConfig::new("valid", vec![
            make_step("step1", vec![]),
            make_step("step2", vec!["step1"]),
        ]);
        let dag = compile(&config);

        let result = validate_dag(&dag);
        assert!(result.is_valid);
    }

    #[test]
    fn test_full_validation_valid() {
        let config = WorkflowConfig::new("full_valid", vec![
            make_step("step1", vec![]),
            make_step("step2", vec!["step1"]),
            make_step("step3", vec!["step2"]),
        ]);
        let dag = compile(&config);

        let result = validate(&config, &dag);
        assert!(result.is_valid);
    }

    #[test]
    fn test_full_validation_invalid() {
        let config = WorkflowConfig::new("full_invalid", vec![
            make_step("step1", vec!["nonexistent"]),
        ]);
        let dag = compile(&config);

        let result = validate(&config, &dag);
        assert!(!result.is_valid);
    }
}