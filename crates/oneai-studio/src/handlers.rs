//! REST API handlers — implement each endpoint for the Studio server.

use std::sync::Arc;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    response::{Html, IntoResponse},
};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::state::StudioState;
use crate::graph_dto::GraphVisualization;
use crate::trace_dto::TraceTreeView;
use crate::checkpoint_dto::{CheckpointListView, CheckpointDetailView};

// ─── Session Handlers ────────────────────────────────────────────────

/// List all tracked sessions.
pub async fn list_sessions(
    State(state): State<Arc<StudioState>>,
) -> Json<Value> {
    let sessions = state.list_sessions().await;
    Json(json!({
        "total": sessions.len(),
        "sessions": sessions,
    }))
}

/// Get details of a specific session.
pub async fn get_session(
    State(state): State<Arc<StudioState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let session = state.get_session(&id).await;
    match session {
        Some(s) => Ok(Json(json!(s))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Get the trace tree for a session.
pub async fn get_session_trace(
    State(state): State<Arc<StudioState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let tree = state.trace_context().build_tree();
    let view = TraceTreeView::from_trace_tree(&tree);
    Json(json!(view))
}

/// Get trace metrics for a session.
pub async fn get_session_metrics(
    State(state): State<Arc<StudioState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let tree = state.trace_context().build_tree();
    let view = TraceTreeView::from_trace_tree(&tree);
    Json(json!(view.metrics))
}

// ─── Graph Handlers ──────────────────────────────────────────────────

/// Built-in demo StateGraphs for visualization.
fn demo_graphs() -> HashMap<String, GraphVisualization> {
    use oneai_workflow::state_graph::{StateGraph, GraphNode, GraphEdge, NodeAction, EdgeCondition};

    let mut graphs: HashMap<String, GraphVisualization> = HashMap::new();

    // ReAct loop graph
    let mut react = StateGraph::new("react-loop", "think");
    react.add_node(GraphNode {
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
    react.add_node(GraphNode {
        id: "act".to_string(),
        action: NodeAction::ToolCall {
            tool_name: "selected_tool".to_string(),
            args_template: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });
    react.add_node(GraphNode {
        id: "observe".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some("Observe the result and decide next".to_string()),
            use_streaming: false,
            include_tool_definitions: true,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });
    react.add_node(GraphNode {
        id: "end".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some("Provide final answer".to_string()),
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
    react.add_edge(GraphEdge {
        from: "think".to_string(),
        to: "act".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    react.add_edge(GraphEdge {
        from: "think".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    react.add_edge(GraphEdge {
        from: "act".to_string(),
        to: "observe".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });
    react.add_edge(GraphEdge {
        from: "observe".to_string(),
        to: "think".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    react.add_edge(GraphEdge {
        from: "observe".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    react.add_terminal("end".to_string());

    graphs.insert("react-loop".to_string(), GraphVisualization::from_state_graph(&react));

    // Plan-then-Execute graph
    let mut plan_exec = StateGraph::new("plan-execute", "plan");
    plan_exec.add_node(GraphNode {
        id: "plan".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some("Decompose the task into steps".to_string()),
            use_streaming: true,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: false,
        metadata: HashMap::new(),
    });
    plan_exec.add_node(GraphNode {
        id: "switch_to_react".to_string(),
        action: NodeAction::SwitchParadigm { paradigm: "react".to_string() },
        interrupt: false,
        metadata: HashMap::new(),
    });
    plan_exec.add_node(GraphNode {
        id: "execute".to_string(),
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
    plan_exec.add_node(GraphNode {
        id: "review".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some("Review the execution results".to_string()),
            use_streaming: false,
            include_tool_definitions: false,
            tool_filter_override: None,
            thinking_budget: None,
            temperature: None,
            max_tokens: None,
        },
        interrupt: true,
        metadata: HashMap::new(),
    });
    plan_exec.add_node(GraphNode {
        id: "end".to_string(),
        action: NodeAction::LlmInfer {
            system_prompt_override: Some("Final summary".to_string()),
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
    plan_exec.add_edge(GraphEdge {
        from: "plan".to_string(),
        to: "switch_to_react".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });
    plan_exec.add_edge(GraphEdge {
        from: "switch_to_react".to_string(),
        to: "execute".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });
    plan_exec.add_edge(GraphEdge {
        from: "execute".to_string(),
        to: "review".to_string(),
        condition: Some(EdgeCondition::IsFinalAnswer),
        metadata: HashMap::new(),
    });
    plan_exec.add_edge(GraphEdge {
        from: "execute".to_string(),
        to: "execute".to_string(),
        condition: Some(EdgeCondition::HasToolCalls),
        metadata: HashMap::new(),
    });
    plan_exec.add_edge(GraphEdge {
        from: "review".to_string(),
        to: "end".to_string(),
        condition: Some(EdgeCondition::Always),
        metadata: HashMap::new(),
    });
    plan_exec.add_terminal("end".to_string());

    graphs.insert("plan-execute".to_string(), GraphVisualization::from_state_graph(&plan_exec));

    graphs
}

/// List all available demo StateGraphs.
pub async fn list_graphs() -> Json<Value> {
    let graphs = demo_graphs();
    let names: Vec<String> = graphs.keys().cloned().collect();
    Json(json!({
        "total": names.len(),
        "graphs": names,
    }))
}

/// Get a specific StateGraph visualization.
pub async fn get_graph(
    Path(name): Path<String>,
) -> Result<Json<GraphVisualization>, StatusCode> {
    let graphs = demo_graphs();
    match graphs.get(&name) {
        Some(viz) => Ok(Json(viz.clone())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

// ─── Checkpoint Handlers ─────────────────────────────────────────────

/// List all available checkpoints.
pub async fn list_checkpoints(
    State(state): State<Arc<StudioState>>,
) -> Json<Value> {
    use oneai_core::traits::StatePersistence;
    let persistence = state.persistence();
    let infos = persistence.list_checkpoints().await
        .unwrap_or_default();
    let view = CheckpointListView::from_checkpoint_infos(&infos);
    Json(json!(view))
}

/// Get details of a specific checkpoint.
pub async fn get_checkpoint(
    State(state): State<Arc<StudioState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    use oneai_core::traits::StatePersistence;
    let persistence = state.persistence();

    let infos = persistence.list_checkpoints().await.unwrap_or_default();
    let info = infos.iter().find(|i| i.id == id);
    match info {
        Some(info) => {
            let state_result = persistence.load_checkpoint(&id).await;
            match state_result {
                Ok(agent_state) => {
                    let view = CheckpointDetailView::from_info_and_state(info, &agent_state);
                    Ok(Json(json!(view)))
                }
                Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Restore from a checkpoint (time-travel).
pub async fn restore_checkpoint(
    State(state): State<Arc<StudioState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    use oneai_core::traits::StatePersistence;
    let persistence = state.persistence();

    let result = persistence.load_checkpoint(&id).await;
    match result {
        Ok(agent_state) => {
            // Register the restored session
            let session = crate::state::SessionView {
                id: agent_state.session_id.clone(),
                paradigm: agent_state.active_paradigm.clone(),
                iteration: 0,
                running: false,
                total_tokens: 0,
                estimated_cost: 0.0,
            };
            state.register_session(session).await;

            Ok(Json(json!({
                "restored": true,
                "checkpoint_id": id,
                "session_id": agent_state.session_id,
                "paradigm": agent_state.active_paradigm,
            })))
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

// ─── Domain Pack Handlers ────────────────────────────────────────────

/// List available DomainPacks (using builtin definitions).
pub async fn list_domain_packs() -> Json<Value> {
    // List the builtin domain pack names
    let packs = vec![
        json!({
            "name": "coding",
            "description": "Coding agent — file editing, code review, test execution",
            "tools": ["read_file", "write_file", "shell", "grep", "glob", "apply_patch"],
        }),
        json!({
            "name": "research",
            "description": "Research agent — web search, document retrieval, summarization",
            "tools": ["web_search", "web_fetch", "read_file", "summarize"],
        }),
    ];

    Json(json!({
        "total": packs.len(),
        "packs": packs,
    }))
}

/// Get details of a specific DomainPack.
pub async fn get_domain_pack(
    Path(name): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let packs = vec![
        ("coding", json!({
            "name": "coding",
            "description": "Coding agent — file editing, code review, test execution",
            "tools": ["read_file", "write_file", "shell", "grep", "glob", "apply_patch"],
            "paradigms": ["react", "plan", "reflect"],
            "permissions": "standard",
        })),
        ("research", json!({
            "name": "research",
            "description": "Research agent — web search, document retrieval, summarization",
            "tools": ["web_search", "web_fetch", "read_file", "summarize"],
            "paradigms": ["react", "explore"],
            "permissions": "read",
        })),
    ];

    for (pack_name, pack_json) in packs {
        if pack_name == name {
            return Ok(Json(pack_json));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

// ─── Tools Handler ───────────────────────────────────────────────────

/// List all registered tools.
pub async fn list_tools(
    State(state): State<Arc<StudioState>>,
) -> Json<Value> {
    let tool_names = state.tool_registry().list_names().await;
    Json(json!({
        "total": tool_names.len(),
        "tools": tool_names,
    }))
}

// ─── Index Page ──────────────────────────────────────────────────────

/// Serve the Studio HTML index page.
pub async fn index() -> Html<&'static str> {
    Html(STUDIO_HTML)
}

/// Static HTML for the Studio frontend, embedded in the Rust binary.
static STUDIO_HTML: &str = include_str!("../static/index.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_graphs() {
        let response = list_graphs().await;
        let value = response.0;
        assert!(value.get("total").unwrap().as_u64() >= Some(2));
        let graphs = value.get("graphs").unwrap().as_array().unwrap();
        assert!(graphs.contains(&serde_json::Value::String("react-loop".to_string())));
        assert!(graphs.contains(&serde_json::Value::String("plan-execute".to_string())));
    }

    #[tokio::test]
    async fn test_get_graph_react_loop() {
        let result = get_graph(Path("react-loop".to_string())).await;
        match result {
            Ok(Json(viz)) => {
                assert_eq!(viz.name, "react-loop");
                assert_eq!(viz.nodes.len(), 4);
                assert!(viz.has_cycles);
            }
            Err(_) => panic!("Expected react-loop graph to exist"),
        }
    }

    #[tokio::test]
    async fn test_get_graph_not_found() {
        let result = get_graph(Path("nonexistent".to_string())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_domain_packs() {
        let response = list_domain_packs().await;
        let value = response.0;
        assert!(value.get("total").unwrap().as_u64() >= Some(2));
    }

    #[tokio::test]
    async fn test_get_domain_pack_coding() {
        let result = get_domain_pack(Path("coding".to_string())).await;
        match result {
            Ok(Json(value)) => {
                assert_eq!(value.get("name").unwrap(), "coding");
            }
            Err(_) => panic!("Expected coding domain pack"),
        }
    }

    #[tokio::test]
    async fn test_get_domain_pack_not_found() {
        let result = get_domain_pack(Path("nonexistent".to_string())).await;
        assert!(result.is_err());
    }
}
