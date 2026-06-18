//! StateGraph visualization DTO — converts StateGraph data into JSON
//! for the Studio frontend to render as SVG/D3.js graphs.

use serde::{Deserialize, Serialize};
use oneai_workflow::state_graph::{StateGraph, GraphNode, GraphEdge, NodeAction, EdgeCondition};

// ─── GraphVisualization ──────────────────────────────────────────────

/// Complete visualization data for a StateGraph — rendered as SVG in the browser.
///
/// Contains all nodes, edges, entry point, and terminal nodes in a format
/// suitable for D3.js force-directed graph rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphVisualization {
    /// The graph name.
    pub name: String,

    /// All nodes in the graph, converted to frontend-friendly format.
    pub nodes: Vec<NodeView>,

    /// All edges in the graph, converted to frontend-friendly format.
    pub edges: Vec<EdgeView>,

    /// The entry point node ID.
    pub entry_point: String,

    /// Terminal node IDs (where execution ends).
    pub terminals: Vec<String>,

    /// Whether the graph contains cycles (expected for ReAct loops).
    pub has_cycles: bool,
}

// ─── NodeView ────────────────────────────────────────────────────────

/// A single node in the visualization — frontend-friendly representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    /// Unique node identifier.
    pub id: String,

    /// Human-readable label with emoji (e.g., "🧠 LLM +tools", "🔧 shell").
    pub label: String,

    /// Action type string (e.g., "llm_infer", "tool_call", "delegate", etc.).
    pub action_type: String,

    /// Whether this node is an interrupt point (execution can pause).
    pub interrupt: bool,

    /// Brief description of the action.
    pub description: String,

    /// Additional details about the node action (system prompt, tool name, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<NodeDetails>,
}

/// Additional details about a node action, specific to each action type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum NodeDetails {
    LlmInfer {
        has_system_prompt_override: bool,
        use_streaming: bool,
        include_tool_definitions: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_filter_override: Option<Vec<String>>,
    },
    ToolCall {
        tool_name: String,
    },
    Delegate {
        agent_kind: String,
    },
    HumanApproval {
        description: String,
    },
    ConditionCheck {
        condition: String,
    },
    SwitchParadigm {
        paradigm: String,
    },
}

// ─── EdgeView ────────────────────────────────────────────────────────

/// A single edge in the visualization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeView {
    /// Source node ID.
    pub from: String,

    /// Target node ID.
    pub to: String,

    /// Edge condition (if conditional) — human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,

    /// Short label for the edge (e.g., "HasToolCalls", "Always", "").
    pub label: String,

    /// Whether this edge is unconditional.
    pub is_unconditional: bool,
}

// ─── Conversion ──────────────────────────────────────────────────────

impl GraphVisualization {
    /// Convert a StateGraph into a frontend-friendly GraphVisualization.
    ///
    /// Reuses the label format from render_state_graph_ascii() but adds
    /// structured details for interactive UI.
    pub fn from_state_graph(graph: &StateGraph) -> Self {
        let nodes: Vec<NodeView> = graph.nodes.values()
            .map(|n| node_to_view(n))
            .collect();

        let edges: Vec<EdgeView> = graph.edges.values()
            .flat_map(|edge_list| edge_list.iter().map(|e| edge_to_view(e)))
            .collect();

        Self {
            name: graph.name.clone(),
            nodes,
            edges,
            entry_point: graph.entry_point.clone(),
            terminals: graph.terminal_nodes.clone(),
            has_cycles: graph.has_cycles(),
        }
    }
}

/// Convert a GraphNode to a NodeView.
fn node_to_view(node: &GraphNode) -> NodeView {
    let (label, action_type, description, details) = match &node.action {
        NodeAction::LlmInfer {
            system_prompt_override,
            use_streaming,
            include_tool_definitions,
            tool_filter_override,
            thinking_budget: _,
            temperature: _,
            max_tokens: _,
        } => {
            let tools_str = if *include_tool_definitions { " +tools" } else { "" };
            let prompt_str = if system_prompt_override.is_some() { " (custom prompt)" } else { "" };
            (
                format!("🧠 LLM{}{}", tools_str, prompt_str),
                "llm_infer".to_string(),
                "LLM inference node — sends conversation to the model".to_string(),
                Some(NodeDetails::LlmInfer {
                    has_system_prompt_override: system_prompt_override.is_some(),
                    use_streaming: *use_streaming,
                    include_tool_definitions: *include_tool_definitions,
                    tool_filter_override: tool_filter_override.clone(),
                }),
            )
        }
        NodeAction::ToolCall { tool_name, args_template: _ } => (
            format!("🔧 {}", tool_name),
            "tool_call".to_string(),
            "Tool execution node".to_string(),
            Some(NodeDetails::ToolCall { tool_name: tool_name.clone() }),
        ),
        NodeAction::Delegate { agent_kind, task_template: _ } => (
            format!("🤖 →{}", agent_kind),
            "delegate".to_string(),
            "Sub-agent delegation node".to_string(),
            Some(NodeDetails::Delegate { agent_kind: agent_kind.clone() }),
        ),
        NodeAction::HumanApproval { description } => (
            format!("✋ {}", description),
            "human_approval".to_string(),
            "Human approval checkpoint".to_string(),
            Some(NodeDetails::HumanApproval { description: description.clone() }),
        ),
        NodeAction::ConditionCheck { condition } => (
            format!("🔀 {}", condition),
            "condition_check".to_string(),
            "Routing condition node".to_string(),
            Some(NodeDetails::ConditionCheck { condition: condition.clone() }),
        ),
        NodeAction::SwitchParadigm { paradigm } => (
            format!("🔄 →{}", paradigm),
            "switch_paradigm".to_string(),
            "Paradigm switch node".to_string(),
            Some(NodeDetails::SwitchParadigm { paradigm: paradigm.clone() }),
        ),
        // Catch-all for #[non_exhaustive] future variants
        _ => (
            format!("⬜ {}", node.id),
            "unknown".to_string(),
            "Unknown node action".to_string(),
            None,
        ),
    };

    NodeView {
        id: node.id.clone(),
        label,
        action_type,
        description,
        interrupt: node.interrupt,
        details,
    }
}

/// Convert a GraphEdge to an EdgeView.
fn edge_to_view(edge: &GraphEdge) -> EdgeView {
    let (condition, label, is_unconditional) = match &edge.condition {
        Some(EdgeCondition::HasToolCalls) =>
            (Some("Model output contains tool calls".to_string()), "HasToolCalls".to_string(), false),
        Some(EdgeCondition::IsFinalAnswer) =>
            (Some("Model output is a final answer".to_string()), "IsFinalAnswer".to_string(), false),
        Some(EdgeCondition::RequestsDelegation) =>
            (Some("Model requests delegation".to_string()), "RequestsDelegation".to_string(), false),
        Some(EdgeCondition::ErrorOccurred) =>
            (Some("An error occurred".to_string()), "ErrorOccurred".to_string(), false),
        Some(EdgeCondition::StateEquals { variable, value }) =>
            (Some(format!("State {} = {}", variable, value)), format!("{}={}", variable, value), false),
        Some(EdgeCondition::Always) =>
            (None, String::new(), true),
        Some(EdgeCondition::Custom { name, description }) =>
            (Some(description.clone()), format!("Custom:{}", name), false),
        Some(EdgeCondition::ParadigmEquals { paradigm }) =>
            (Some(format!("Active paradigm = {}", paradigm)), format!("Paradigm={}", paradigm), false),
        Some(EdgeCondition::IterationExceeds { count }) =>
            (Some(format!("Iterations exceed {}", count)), format!("Iter>{}", count), false),
        // Catch-all for #[non_exhaustive] future variants
        Some(_) =>
            (None, String::new(), false),
        None =>
            (None, String::new(), true),
    };

    EdgeView {
        from: edge.from.clone(),
        to: edge.to.clone(),
        condition,
        label,
        is_unconditional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_react_graph() -> StateGraph {
        let mut graph = StateGraph::new("react-loop", "think");

        graph.add_node(GraphNode {
            id: "think".to_string(),
            action: NodeAction::LlmInfer {
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

        graph.add_node(GraphNode {
            id: "act".to_string(),
            action: NodeAction::ToolCall {
                tool_name: "selected_tool".to_string(),
                args_template: None,
            },
            interrupt: false,
            metadata: HashMap::new(),
        });

        graph.add_node(GraphNode {
            id: "end".to_string(),
            action: NodeAction::LlmInfer {
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

        graph.add_edge(GraphEdge {
            from: "think".to_string(),
            to: "act".to_string(),
            condition: Some(EdgeCondition::HasToolCalls),
            metadata: HashMap::new(),
        });

        graph.add_edge(GraphEdge {
            from: "think".to_string(),
            to: "end".to_string(),
            condition: Some(EdgeCondition::IsFinalAnswer),
            metadata: HashMap::new(),
        });

        graph.add_edge(GraphEdge {
            from: "act".to_string(),
            to: "think".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });

        graph.add_terminal("end".to_string());

        graph
    }

    #[test]
    fn test_graph_visualization_from_state_graph() {
        let graph = make_react_graph();
        let viz = GraphVisualization::from_state_graph(&graph);

        assert_eq!(viz.name, "react-loop");
        assert_eq!(viz.nodes.len(), 3);
        assert_eq!(viz.edges.len(), 3);
        assert_eq!(viz.entry_point, "think");
        assert_eq!(viz.terminals, vec!["end"]);
        assert!(viz.has_cycles); // act → think is a cycle
    }

    #[test]
    fn test_node_view_llm_infer() {
        let node = GraphNode {
            id: "think".to_string(),
            action: NodeAction::LlmInfer {
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
        };

        let view = node_to_view(&node);
        assert_eq!(view.id, "think");
        assert_eq!(view.action_type, "llm_infer");
        assert!(view.label.contains("🧠"));
        assert!(view.label.contains("+tools"));
    }

    #[test]
    fn test_node_view_tool_call() {
        let node = GraphNode {
            id: "act".to_string(),
            action: NodeAction::ToolCall {
                tool_name: "shell".to_string(),
                args_template: None,
            },
            interrupt: false,
            metadata: HashMap::new(),
        };

        let view = node_to_view(&node);
        assert_eq!(view.id, "act");
        assert_eq!(view.action_type, "tool_call");
        assert!(view.label.contains("🔧 shell"));
    }

    #[test]
    fn test_node_view_human_approval() {
        let node = GraphNode {
            id: "approve".to_string(),
            action: NodeAction::HumanApproval {
                description: "Delete file?".to_string(),
            },
            interrupt: true,
            metadata: HashMap::new(),
        };

        let view = node_to_view(&node);
        assert!(view.interrupt);
        assert_eq!(view.action_type, "human_approval");
        assert!(view.label.contains("✋"));
    }

    #[test]
    fn test_edge_view_conditional() {
        let edge = GraphEdge {
            from: "think".to_string(),
            to: "act".to_string(),
            condition: Some(EdgeCondition::HasToolCalls),
            metadata: HashMap::new(),
        };

        let view = edge_to_view(&edge);
        assert_eq!(view.from, "think");
        assert_eq!(view.to, "act");
        assert!(!view.is_unconditional);
        assert_eq!(view.label, "HasToolCalls");
        assert!(view.condition.is_some());
    }

    #[test]
    fn test_edge_view_unconditional() {
        let edge = GraphEdge {
            from: "act".to_string(),
            to: "think".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        };

        let view = edge_to_view(&edge);
        assert!(view.is_unconditional);
        assert_eq!(view.label, "");
        assert!(view.condition.is_none());
    }

    #[test]
    fn test_graph_visualization_json_serialization() {
        let graph = make_react_graph();
        let viz = GraphVisualization::from_state_graph(&graph);

        let json = serde_json::to_string_pretty(&viz).unwrap();
        assert!(json.contains("\"react-loop\""));
        assert!(json.contains("\"think\""));
        assert!(json.contains("\"HasToolCalls\""));
        assert!(json.contains("\"has_cycles\": true"));
    }
}
