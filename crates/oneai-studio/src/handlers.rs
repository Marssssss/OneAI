//! REST API handlers — implement each endpoint for the Studio server.

use std::sync::Arc;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    Json,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
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
    Path(_id): Path<String>,
) -> Json<Value> {
    let tree = state.trace_context().build_tree();
    let view = TraceTreeView::from_trace_tree(&tree);
    Json(json!(view))
}

/// Get trace metrics for a session.
pub async fn get_session_metrics(
    State(state): State<Arc<StudioState>>,
    Path(_id): Path<String>,
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

// ─── Static Assets ────────────────────────────────────────────────────

/// Embed the frontend assets directly into the binary so the Studio is
/// fully self-contained (no dependency on a `static/` dir on disk at
/// runtime — works from any CWD and in release distributions). Mirrors how
/// `index.html` is embedded above.
static STUDIO_CSS: &str = include_str!("../static/studio.css");
static STUDIO_JS: &str = include_str!("../static/studio.js");
static GRAPH_JS: &str = include_str!("../static/graph-render.js");

/// Serve a frontend asset from `/static/{file}`. The old router only
/// registered `/`, so the CSS/JS referenced by `index.html` 404'd and the
/// page rendered as unstyled, JS-less HTML — i.e. unusable.
pub async fn serve_static(Path(file): Path<String>) -> Response {
    let (ctype, bytes): (&str, &'static [u8]) = match file.as_str() {
        "studio.css" => ("text/css; charset=utf-8", STUDIO_CSS.as_bytes()),
        "studio.js" => (
            "application/javascript; charset=utf-8",
            STUDIO_JS.as_bytes(),
        ),
        "graph-render.js" => (
            "application/javascript; charset=utf-8",
            GRAPH_JS.as_bytes(),
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, ctype)], bytes).into_response()
}

// ─── Run Task (interactive playground) ────────────────────────────────

/// Request body for `POST /api/run` — a user prompt to drive the agent.
#[derive(Debug, Deserialize)]
pub struct RunRequest {
    pub prompt: String,
}

/// Drive one agent turn from a user prompt. Returns immediately; the
/// agent's iterations, tool calls, streaming chunks, and final answer
/// stream to all WebSocket subscribers as `StudioEvent`s (the
/// `StudioState` is the observer passed to `run_agent`).
///
/// The actual driving is delegated to a `StudioRunner` (implemented in
/// the CLI layer, since `oneai-studio` sits below `oneai-app` and cannot
/// hold an `AppSession`).
pub async fn run_task(
    State(state): State<Arc<StudioState>>,
    Json(req): Json<RunRequest>,
) -> (StatusCode, Json<Value>) {
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "accepted": false, "error": "empty prompt" })),
        );
    }

    let runner = state.runner().await;
    let runner = match runner {
        Some(r) => r,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "accepted": false,
                    "error": "No agent runner attached. The Studio server was \
                              started without an agent (e.g. `serve()` standalone). \
                              Use `oneai studio` to get an interactive agent.",
                })),
            );
        }
    };

    let status = runner.status();
    if !status.has_provider {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "accepted": false,
                "error": "No LLM provider configured. Set ONEAI_API_KEY and \
                          ONEAI_BASE_URL (or configure ~/.oneai/config.toml) \
                          and restart `oneai studio`.",
            })),
        );
    }
    if status.busy {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "accepted": false,
                "error": "Agent is already running. Wait for it to finish.",
            })),
        );
    }

    // Spawn so the HTTP call returns immediately; events flow over /ws.
    let task = prompt.to_string();
    let observer = state.clone();
    tokio::spawn(async move {
        let _ = runner.run_task(&task, observer).await;
    });

    (StatusCode::ACCEPTED, Json(json!({ "accepted": true })))
}

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

    #[tokio::test]
    async fn test_serve_static_assets() {
        // Known assets resolve with the correct content-type.
        for (file, expect_contains) in [
            ("studio.css", "text/css"),
            ("studio.js", "javascript"),
            ("graph-render.js", "javascript"),
        ] {
            let resp = serve_static(Path(file.to_string())).await;
            assert_eq!(resp.status(), StatusCode::OK, "{} should be 200", file);
            let ct = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .expect("content-type header")
                .to_str()
                .unwrap();
            assert!(ct.contains(expect_contains), "{} -> {}", file, ct);
        }
    }

    #[tokio::test]
    async fn test_serve_static_unknown_404() {
        let resp = serve_static(Path("does-not-exist.css".to_string())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_run_task_rejects_empty_prompt() {
        let state = Arc::new(StudioState::new_default());
        let (code, Json(v)) = run_task(
            State(state),
            Json(RunRequest { prompt: "   ".to_string() }),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["accepted"], false);
    }

    #[tokio::test]
    async fn test_run_task_no_runner_returns_503() {
        // Standalone `serve()` has no runner attached → a non-empty prompt
        // must surface a 503 with a hint, not spawn anything.
        let state = Arc::new(StudioState::new_default());
        let (code, Json(v)) = run_task(
            State(state),
            Json(RunRequest { prompt: "hello".to_string() }),
        )
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(v["accepted"], false);
        assert!(v["error"].as_str().unwrap().contains("runner"));
    }
}
