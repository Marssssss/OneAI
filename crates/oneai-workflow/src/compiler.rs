//! Workflow compiler — converts WorkflowConfig into WorkflowDAG.
//!
//! The compiler takes a user-defined workflow configuration and produces
//! a compiled DAG ready for execution. The compilation process:
//! 1. Creates DAG nodes from step configs
//! 2. Resolves dependency edges
//! 3. Computes parallel levels
//! 4. Optionally runs validation

use crate::config::WorkflowConfig;
use crate::dag::{DagNode, WorkflowDag};

/// Compile a WorkflowConfig into a WorkflowDAG.
///
/// Converts each StepConfig into a DagNode, sets up dependency edges,
/// and computes parallel execution levels.
pub fn compile(config: &WorkflowConfig) -> WorkflowDag {
    let mut dag = WorkflowDag::new(config.name.clone());

    // Create nodes from step configs
    for step in &config.steps {
        let node = DagNode {
            id: step.id.clone(),
            step: step.clone(),
            depends_on: step.depends_on.clone(),
            children: Vec::new(), // Will be filled during build()
            level: 0,
        };
        dag.add_node(node);
    }

    // Build dependency edges and compute levels
    dag.build();

    dag
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{StepConfig, RetryPolicy};
    use std::collections::HashMap;

    fn make_step(id: &str, depends_on: Vec<&str>) -> StepConfig {
        StepConfig {
            id: id.to_string(),
            description: format!("Step {}", id),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            tool: None,
            tool_args: None,
            prompt: None,
            requires_approval: false,
            timeout_secs: None,
            retry_policy: None,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn test_compile_simple_workflow() {
        let config = WorkflowConfig::new("simple", vec![
            make_step("step1", vec![]),
            make_step("step2", vec!["step1"]),
            make_step("step3", vec!["step2"]),
        ]);

        let dag = compile(&config);

        assert_eq!(dag.name, "simple");
        assert_eq!(dag.node_count(), 3);
        assert_eq!(dag.level_count(), 3);
        assert_eq!(dag.roots, vec!["step1"]);
        assert_eq!(dag.leaves, vec!["step3"]);
    }

    #[test]
    fn test_compile_parallel_workflow() {
        let config = WorkflowConfig::new("parallel", vec![
            make_step("fetch_data", vec![]),
            make_step("analyze_data", vec!["fetch_data"]),
            make_step("generate_report", vec!["fetch_data"]),
            make_step("publish", vec!["analyze_data", "generate_report"]),
        ]);

        let dag = compile(&config);

        assert_eq!(dag.node_count(), 4);
        assert_eq!(dag.level_count(), 3);
        // Level 0: fetch_data
        // Level 1: analyze_data, generate_report (parallel)
        // Level 2: publish
        assert_eq!(dag.levels[0], vec!["fetch_data"]);
        assert!(dag.levels[1].contains(&"analyze_data".to_string()));
        assert!(dag.levels[1].contains(&"generate_report".to_string()));
        assert_eq!(dag.levels[2], vec!["publish"]);
    }

    #[test]
    fn test_compile_preserves_step_config() {
        let step = StepConfig {
            id: "calc".to_string(),
            description: "Calculate something".to_string(),
            depends_on: vec!["fetch".to_string()],
            tool: Some("calculator".to_string()),
            tool_args: Some(serde_json::json!({"expression": "2+2"})),
            prompt: None,
            requires_approval: false,
            timeout_secs: Some(30),
            retry_policy: Some(RetryPolicy { max_retries: 5, retry_delay_secs: 10, retry_on_all_errors: false }),
            metadata: HashMap::new(),
        };

        let config = WorkflowConfig::new("test", vec![
            make_step("fetch", vec![]),
            step.clone(),
        ]);

        let dag = compile(&config);
        let node = dag.get_node("calc").unwrap();
        assert_eq!(node.step.tool, Some("calculator".to_string()));
        assert_eq!(node.step.timeout_secs, Some(30));
    }

    #[test]
    fn test_compile_empty_workflow() {
        let config = WorkflowConfig::new("empty", vec![]);
        let dag = compile(&config);

        assert_eq!(dag.node_count(), 0);
        assert_eq!(dag.level_count(), 0);
    }

    #[test]
    fn test_compile_from_json() {
        let json = r#"{
            "name": "test_workflow",
            "description": "A test workflow",
            "version": "1.0",
            "steps": [
                {
                    "id": "step1",
                    "description": "First step",
                    "depends_on": [],
                    "prompt": "Do something"
                },
                {
                    "id": "step2",
                    "description": "Second step",
                    "depends_on": ["step1"],
                    "tool": "calculator",
                    "tool_args": {"expression": "2+2"}
                }
            ],
            "variables": {},
            "default_retry_policy": {"max_retries": 3, "retry_delay_secs": 5, "retry_on_all_errors": false},
            "continue_on_failure": false
        }"#;

        let config = WorkflowConfig::from_json(json).unwrap();
        let dag = compile(&config);

        assert_eq!(dag.node_count(), 2);
        assert_eq!(dag.levels[0], vec!["step1"]);
        assert_eq!(dag.levels[1], vec!["step2"]);
    }
}