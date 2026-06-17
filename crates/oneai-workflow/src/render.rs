//! ASCII visualization for WorkflowDag and StateGraph.
//!
//! Provides `render_dag_ascii()` and `render_state_graph_ascii()` functions
//! that produce human-readable text visualizations of workflow structures.
//! These are used by the `/wf show` and `/wf graph` CLI commands.

use crate::dag::WorkflowDag;
use crate::state_graph::{StateGraph, NodeAction, EdgeCondition};

/// Render a WorkflowDag as ASCII text showing parallel levels and step details.
///
/// Example output (conceptual):
/// ```text
/// Level 0:
///   ├── read_changes → shell
/// Level 1:
///   ├── check_syntax → shell (depends: read_changes)
///   ├── run_tests → shell (depends: read_changes)
///   ├── analyze_quality → LLM (depends: read_changes)
/// Level 2:
///   ├── compile_report → LLM (depends: check_syntax, run_tests, analyze_quality)
/// ```
pub fn render_dag_ascii(dag: &WorkflowDag) -> String {
    let mut lines = Vec::new();

    for level_idx in 0..dag.levels.len() {
        let level_ids = &dag.levels[level_idx];
        lines.push(format!("Level {}:", level_idx));
        for step_id in level_ids {
            let node = dag.get_node(step_id);
            if let Some(node) = node {
                let tool_info = if let Some(tool) = &node.step.tool {
                    format!("→ {}", tool)
                } else if node.step.prompt.is_some() {
                    "→ LLM".to_string()
                } else {
                    "→ (empty)".to_string()
                };
                let deps = if node.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(" (depends: {})", node.depends_on.join(", "))
                };
                lines.push(format!("  ├── {}{}{}", step_id, tool_info, deps));
            }
        }
    }

    if lines.is_empty() {
        lines.push("(empty DAG)".to_string());
    }

    lines.join("\n")
}

/// Render a StateGraph as ASCII text showing nodes, actions, edges, and conditions.
///
/// Example output (conceptual):
/// ```text
/// Entry: think ->
///   think [LLM]
///     -> act [HasToolCalls]
///     -> end [IsFinalAnswer]
///   act [selected_tool]
///     -> observe
///   observe [has_more_tool_calls]
///     -> think [has_more_tool_calls=true]
///     -> end [has_more_tool_calls=false]
///   end [LLM]
/// Terminal: end
/// ```
pub fn render_state_graph_ascii(graph: &StateGraph) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Entry: {} →", graph.entry_point));

    // Sort node IDs for consistent display
    let mut node_ids: Vec<String> = graph.nodes.keys().cloned().collect();
    node_ids.sort();

    for node_id in &node_ids {
        let node = graph.nodes.get(node_id).unwrap();
        let action_str = match &node.action {
            NodeAction::LlmInfer { system_prompt_override, include_tool_definitions, .. } => {
                let tools_str = if *include_tool_definitions { " +tools" } else { "" };
                if system_prompt_override.is_some() {
                    format!("🧠 LLM (custom prompt){}", tools_str)
                } else {
                    format!("🧠 LLM{}", tools_str)
                }
            }
            NodeAction::ToolCall { tool_name, .. } => format!("🔧 {}", tool_name),
            NodeAction::Delegate { agent_kind, .. } => format!("🤖 →{}", agent_kind),
            NodeAction::HumanApproval { description } => format!("✋ {}", description),
            NodeAction::ConditionCheck { condition } => format!("🔀 {}", condition),
            NodeAction::SwitchParadigm { paradigm } => format!("🔄 →{}", paradigm),
        };
        let interrupt_str = if node.interrupt { " ⏸" } else { "" };
        lines.push(format!("  {} [{}]{}", node_id, action_str, interrupt_str));

        // Show outgoing edges
        if let Some(edges) = graph.edges.get(node_id) {
            for edge in edges {
                let cond_str: String = match &edge.condition {
                    Some(EdgeCondition::HasToolCalls) => " [HasToolCalls]".to_string(),
                    Some(EdgeCondition::IsFinalAnswer) => " [IsFinalAnswer]".to_string(),
                    Some(EdgeCondition::RequestsDelegation) => " [RequestsDelegation]".to_string(),
                    Some(EdgeCondition::ErrorOccurred) => " [ErrorOccurred]".to_string(),
                    Some(EdgeCondition::StateEquals { variable, value }) =>
                        format!(" [{}={}]", variable, value),
                    Some(EdgeCondition::Always) => String::new(),
                    Some(EdgeCondition::Custom { name, .. }) => format!(" [Custom:{}]", name),
                    Some(EdgeCondition::ParadigmEquals { paradigm }) => format!(" [Paradigm={}]", paradigm),
                    Some(EdgeCondition::IterationExceeds { count }) => format!(" [Iter>{}]", count),
                    None => String::new(),
                };
                lines.push(format!("    → {}{}", edge.to, cond_str));
            }
        }
    }

    lines.push(format!("Terminal: {}", graph.terminal_nodes.join(", ")));

    // Add cycle info
    if graph.has_cycles() {
        lines.push("⚠ Contains cycles (expected for ReAct loops)".to_string());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StepConfig;
    use crate::dag::DagNode;
    use std::collections::HashMap;

    fn make_step(id: &str, depends_on: Vec<&str>, tool: Option<&str>) -> StepConfig {
        StepConfig {
            id: id.to_string(),
            description: format!("Step {}", id),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            tool: tool.map(|t| t.to_string()),
            tool_args: None,
            prompt: None,
            requires_approval: false,
            timeout_secs: None,
            retry_policy: None,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn test_render_dag_ascii() {
        let mut dag = WorkflowDag::new("test");
        dag.add_node(DagNode {
            id: "read".to_string(),
            step: make_step("read", vec![], Some("shell")),
            depends_on: vec![],
            children: Vec::new(),
            level: 0,
        });
        dag.add_node(DagNode {
            id: "analyze".to_string(),
            step: StepConfig {
                id: "analyze".to_string(),
                description: "Analyze".to_string(),
                depends_on: vec!["read".to_string()],
                tool: None,
                tool_args: None,
                prompt: Some("Analyze code".to_string()),
                requires_approval: false,
                timeout_secs: None,
                retry_policy: None,
                metadata: HashMap::new(),
            },
            depends_on: vec!["read".to_string()],
            children: Vec::new(),
            level: 0,
        });
        dag.build();

        let viz = render_dag_ascii(&dag);
        assert!(viz.contains("Level 0:"));
        assert!(viz.contains("→ shell"));
        assert!(viz.contains("→ LLM"));
    }

    #[test]
    fn test_render_state_graph_ascii() {
        let mut graph = StateGraph::new("react-loop", "think");

        graph.add_node(crate::state_graph::GraphNode {
            id: "think".to_string(),
            action: crate::state_graph::NodeAction::LlmInfer {
                system_prompt_override: None,
                use_streaming: true,
                include_tool_definitions: true,
                tool_filter_override: None,
                thinking_budget: None,
                temperature: None,
                max_tokens: None,
            },
            interrupt: false,
            metadata: HashMap::new(),
        });

        graph.add_node(crate::state_graph::GraphNode {
            id: "end".to_string(),
            action: crate::state_graph::NodeAction::LlmInfer {
                system_prompt_override: Some("Final answer".to_string()),
                use_streaming: false,
                include_tool_definitions: false,
                tool_filter_override: None,
                thinking_budget: None,
                temperature: None,
                max_tokens: None,
            },
            interrupt: false,
            metadata: HashMap::new(),
        });

        graph.add_edge(crate::state_graph::GraphEdge {
            from: "think".to_string(),
            to: "end".to_string(),
            condition: Some(crate::state_graph::EdgeCondition::IsFinalAnswer),
            metadata: HashMap::new(),
        });

        graph.add_terminal("end".to_string());

        let viz = render_state_graph_ascii(&graph);
        assert!(viz.contains("Entry: think"));
        assert!(viz.contains("🧠 LLM"));
        assert!(viz.contains("Terminal: end"));
        assert!(viz.contains("[IsFinalAnswer]"));
    }
}
