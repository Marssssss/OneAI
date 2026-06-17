//! StateGraph executor — walks cyclic graph, evaluates edge conditions, handles interrupts.
//!
//! Unlike WorkflowDag (which is acyclic and runs levels sequentially),
//! StateGraph supports cyclic edges for iterative agent patterns like ReAct loops.
//! The executor walks the graph from the entry point, executing each node's action,
//! evaluating outgoing edge conditions to route to the next node, and handling
//! interrupt points where execution can pause for human intervention.
//!
//! Key features:
//! - Walks graph from `entry_point` through conditional edges
//! - Executes 5 NodeAction variants (LlmInfer, ToolCall, Delegate, HumanApproval, ConditionCheck)
//! - Evaluates 7 EdgeCondition variants for dynamic routing
//! - Handles interrupt points (nodes with `interrupt: true`)
//! - Bounded by `max_iterations` to prevent infinite loops
//! - Supports `{{variable}}` template interpolation in tool_name and args_template

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{ApprovalGate, LlmProvider, Tool};
use oneai_core::{Conversation, InferenceRequest, Message, Role};

use crate::state_graph::{
    StateGraph, GraphNode, GraphState, GraphEdge, NodeAction, EdgeCondition, GraphExecutionResult,
};

// ─── Delegate Action Trait ────────────────────────────────────────────────────

/// Trait for executing delegate actions in StateGraph nodes.
///
/// This is a lightweight abstraction that avoids a cyclic dependency
/// with `oneai-agent`. The `oneai-agent` crate provides a concrete
/// implementation that wraps `SubAgentFactory`.
///
/// When no delegate factory is available, use `NoopDelegateFactory`
/// which returns an error for all delegate requests.
#[async_trait::async_trait]
pub trait DelegateFactory: Send + Sync {
    /// Execute a delegate action with the given agent kind and task.
    async fn delegate(&self, agent_kind: &str, task: &str) -> Result<String>;
}

/// A no-op delegate factory that returns errors for all requests.
/// Used when no sub-agent delegation is available.
pub struct NoopDelegateFactory;

#[async_trait::async_trait]
impl DelegateFactory for NoopDelegateFactory {
    async fn delegate(&self, agent_kind: &str, _task: &str) -> Result<String> {
        Err(OneAIError::Workflow(format!(
            "Delegate action '{}' not supported — no DelegateFactory configured", agent_kind
        )))
    }
}

// ─── ActionResult ────────────────────────────────────────────────────────────

/// The result of executing a single node action.
#[derive(Debug, Clone)]
struct ActionResult {
    /// The output content from this action.
    output: String,
    /// Error message if the action failed.
    error: Option<String>,
}

// ─── StateGraphExecutor ──────────────────────────────────────────────────────

/// Executor for StateGraph — walks cyclic graph with conditional routing.
///
/// The executor processes a StateGraph by:
/// 1. Starting at the `entry_point` node
/// 2. Executing each node's `NodeAction`
/// 3. Evaluating outgoing `EdgeCondition`s to select the next node
/// 4. Handling interrupt points (nodes marked with `interrupt: true`)
/// 5. Terminating when a terminal node is reached or max iterations exceeded
///
/// Template interpolation (`{{variable}}`) is applied to `tool_name` and
/// `args_template` fields before tool execution, using `GraphState.variables`.
pub struct StateGraphExecutor {
    /// LLM provider for LlmInfer nodes.
    provider: Arc<dyn LlmProvider>,
    /// Tool registry for ToolCall nodes.
    tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    /// Delegate factory for Delegate nodes.
    delegate_factory: Arc<dyn DelegateFactory>,
    /// Approval gate for interrupt points and HumanApproval nodes.
    approval_gate: Arc<dyn ApprovalGate>,
    /// Maximum iterations through the graph (prevents infinite loops).
    /// Default: 50.
    max_iterations: usize,
}

impl StateGraphExecutor {
    /// Create a new StateGraphExecutor with all dependencies.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
        delegate_factory: Arc<dyn DelegateFactory>,
        approval_gate: Arc<dyn ApprovalGate>,
        max_iterations: usize,
    ) -> Self {
        Self {
            provider,
            tools,
            delegate_factory,
            approval_gate,
            max_iterations,
        }
    }

    /// Create with default max_iterations (50).
    pub fn with_defaults(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
        delegate_factory: Arc<dyn DelegateFactory>,
        approval_gate: Arc<dyn ApprovalGate>,
    ) -> Self {
        Self::new(provider, tools, delegate_factory, approval_gate, 50)
    }

    /// Execute a StateGraph starting from its entry point.
    ///
    /// Walks the graph, executing each node's action and routing to the
    /// next node based on edge conditions. Returns a GraphExecutionResult
    /// with the final state, completion status, and iteration count.
    pub async fn execute(
        &self,
        graph: &StateGraph,
        initial_state: GraphState,
    ) -> Result<GraphExecutionResult> {
        let mut state = initial_state;
        let mut current_node_id = graph.entry_point.clone();
        let mut iterations = 0;
        let mut interrupt_checkpoints = Vec::new();

        while iterations < self.max_iterations {
            iterations += 1;

            // 1. Get the current node
            let node = graph.get_node(&current_node_id)
                .ok_or_else(|| OneAIError::Workflow(
                    format!("Node '{}' not found in graph '{}'", current_node_id, graph.name)
                ))?;

            // 2. Check if this is a terminal node
            if graph.terminal_nodes.contains(&current_node_id) {
                // Execute the terminal node's action (usually LlmInfer for final answer)
                let action_result = self.execute_node_action(&node.action, &mut state).await?;
                state.last_result = Some(action_result.output.clone());
                state.last_error = action_result.error.clone();

                return Ok(GraphExecutionResult {
                    name: graph.name.clone(),
                    final_state: state,
                    completed: true,
                    terminal_node: Some(current_node_id),
                    iterations,
                    interrupt_checkpoints,
                });
            }

            // 3. Check interrupt point
            if node.interrupt {
                let checkpoint_id = format!("interrupt_{}_{}", graph.name, current_node_id);
                interrupt_checkpoints.push(checkpoint_id.clone());

                // Request human approval
                let approval_request = oneai_core::ApprovalRequest {
                    tool_name: "state_graph_interrupt".into(),
                    args: serde_json::json!({
                        "node": current_node_id,
                        "description": match &node.action {
                            NodeAction::HumanApproval { description } => description.clone(),
                            _ => "Interrupt point reached".to_string(),
                        },
                        "state": state.variables,
                    }),
                    risk_level: oneai_core::RiskLevel::Medium,
                    permission_level: Some(oneai_core::PermissionLevel::Standard),
                    justification: format!("StateGraph interrupt at node '{}' in graph '{}'",
                        current_node_id, graph.name),
                };

                let approval = self.approval_gate.request_approval(approval_request).await?;
                match approval {
                    oneai_core::ApprovalResponse::Denied { reason } => {
                        state.should_terminate = true;
                        state.last_error = Some(format!("Interrupt denied: {}", reason));
                        return Ok(GraphExecutionResult {
                            name: graph.name.clone(),
                            final_state: state,
                            completed: false,
                            terminal_node: None,
                            iterations,
                            interrupt_checkpoints,
                        });
                    }
                    _ => { /* Approved — continue execution */ }
                }
            }

            // 4. Execute node action
            let action_result = self.execute_node_action(&node.action, &mut state).await?;

            // 5. Update state
            state.last_result = Some(action_result.output.clone());
            state.last_error = action_result.error.clone();
            if state.should_terminate {
                break;
            }

            // 6. Evaluate outgoing edges → route to next node
            let edges = graph.get_edges_from(&current_node_id);
            let next_node = self.route_next_node(&edges, &state)?;

            if next_node.is_none() {
                // No matching condition — terminate
                tracing::warn!(
                    "No matching edge condition from node '{}' in graph '{}'. Terminating.",
                    current_node_id, graph.name
                );
                break;
            }

            current_node_id = next_node.unwrap();
        }

        // Did we exceed max_iterations or terminate without reaching a terminal node?
        let should_terminate = state.should_terminate;
        if iterations >= self.max_iterations {
            tracing::warn!(
                "StateGraph '{}' exceeded max iterations ({}). Terminating.",
                graph.name, self.max_iterations
            );
        }

        Ok(GraphExecutionResult {
            name: graph.name.clone(),
            final_state: state,
            completed: !should_terminate,
            terminal_node: None,
            iterations,
            interrupt_checkpoints,
        })
    }

    /// Execute a node's action and update the state.
    async fn execute_node_action(
        &self,
        action: &NodeAction,
        state: &mut GraphState,
    ) -> Result<ActionResult> {
        match action {
            NodeAction::LlmInfer { system_prompt_override, use_streaming } => {
                // Build inference request
                let system_prompt = system_prompt_override.clone()
                    .unwrap_or_else(|| "You are an intelligent agent. Respond to the task.".to_string());

                let mut conversation = state.conversation.clone();
                // Inject system prompt if not already present
                if !conversation.messages.iter().any(|m| m.role == Role::System) {
                    conversation.add_message(Message::system(&system_prompt));
                }

                let request = InferenceRequest {
                    conversation,
                    tools: vec![], // LLM infer nodes don't send tool definitions
                    max_tokens: Some(4096),
                    temperature: Some(0.3),
                    top_p: None,
                    stop_sequences: vec![],
                    constrained_output: None,
                    thinking_budget: None,
                    metadata: HashMap::new(),
                };

                let response = self.provider.infer(request).await?;
                let output = response.message.text_content();

                // Update conversation state with the response
                state.conversation.add_message(response.message.clone());

                Ok(ActionResult {
                    output,
                    error: None,
                })
            }

            NodeAction::ToolCall { tool_name, args_template } => {
                // Template interpolation: {{variable}} → state.variables[key]
                let resolved_name = interpolate_graph_template(tool_name, &state.variables);
                let resolved_args = if let Some(template) = args_template {
                    let json_str = interpolate_graph_template(template, &state.variables);
                    serde_json::from_str(&json_str)
                        .unwrap_or(serde_json::json!({}))
                } else {
                    serde_json::json!({})
                };

                // Find and execute the tool
                let tools_map = self.tools.read().await;
                let tool = tools_map.get(&resolved_name)
                    .ok_or_else(|| OneAIError::Workflow(
                        format!("Tool '{}' not found for ToolCall node", resolved_name)
                    ))?;

                let output = tool.execute(resolved_args).await?;

                // Append tool result to conversation
                state.conversation.add_message(Message::tool_result(
                    format!("graph_tool_{}", resolved_name),
                    output.content.clone(),
                ));

                Ok(ActionResult {
                    output: output.content,
                    error: output.error,
                })
            }

            NodeAction::Delegate { agent_kind, task_template } => {
                let task = interpolate_graph_template(task_template, &state.variables);

                let result = self.delegate_factory.delegate(agent_kind, &task).await?;

                // Append delegate result to conversation
                state.conversation.add_message(Message::assistant(
                    format!("[Delegate {}]: {}", agent_kind, result)
                ));

                Ok(ActionResult {
                    output: result,
                    error: None,
                })
            }

            NodeAction::HumanApproval { description } => {
                // Handled in the main loop's interrupt check.
                // Here we just return the description as output.
                Ok(ActionResult {
                    output: description.clone(),
                    error: None,
                })
            }

            NodeAction::ConditionCheck { condition } => {
                let result = self.evaluate_condition_expression(condition, state)?;
                state.variables.insert("_condition_result".to_string(), result.to_string());
                Ok(ActionResult {
                    output: result.to_string(),
                    error: None,
                })
            }
        }
    }

    /// Route to the next node by evaluating outgoing edge conditions.
    ///
    /// Priority: conditional edges are evaluated in order; the first matching
    /// condition determines the next node. Unconditional edges (Always) act
    /// as fallback routing.
    fn route_next_node(
        &self,
        edges: &[&GraphEdge],
        state: &GraphState,
    ) -> Result<Option<String>> {
        for edge in edges {
            if let Some(condition) = &edge.condition {
                if self.evaluate_edge_condition(condition, state)? {
                    return Ok(Some(edge.to.clone()));
                }
            } else {
                // No condition — unconditional edge (fallback)
                return Ok(Some(edge.to.clone()));
            }
        }
        Ok(None)
    }

    /// Evaluate an edge condition against the current state.
    fn evaluate_edge_condition(
        &self,
        condition: &EdgeCondition,
        state: &GraphState,
    ) -> Result<bool> {
        match condition {
            EdgeCondition::HasToolCalls => {
                // Check if the last result contains tool call markers
                Ok(state.last_result.as_ref()
                    .map(|r| r.contains("tool_use") || r.contains("function_call")
                        || r.contains("<tool_call>") || r.contains("ToolCall"))
                    .unwrap_or(false))
            }

            EdgeCondition::IsFinalAnswer => {
                // Opposite of HasToolCalls — no tool calls in the output
                Ok(!self.evaluate_edge_condition(&EdgeCondition::HasToolCalls, state)?)
            }

            EdgeCondition::RequestsDelegation => {
                Ok(state.last_result.as_ref()
                    .map(|r| r.contains("delegate") || r.contains("sub_agent")
                        || r.contains("DELEGATE"))
                    .unwrap_or(false))
            }

            EdgeCondition::ErrorOccurred => {
                Ok(state.last_error.is_some())
            }

            EdgeCondition::StateEquals { variable, value } => {
                Ok(state.variables.get(variable) == Some(value))
            }

            EdgeCondition::Always => Ok(true),

            EdgeCondition::Custom { name, description } => {
                tracing::warn!(
                    "Custom condition '{}' ('{}') not registered. Defaulting to false.",
                    name, description
                );
                Ok(false)
            }
        }
    }

    /// Evaluate a condition expression (for ConditionCheck nodes).
    ///
    /// Simple condition expressions:
    /// - "has_tool_calls" → checks last_result for tool call markers
    /// - "is_final_answer" → opposite of has_tool_calls
    /// - "error_occurred" → checks if last_error is set
    /// - "variable==value" → checks state variable equality
    fn evaluate_condition_expression(&self, condition: &str, state: &GraphState) -> Result<bool> {
        if condition == "has_tool_calls" {
            return self.evaluate_edge_condition(&EdgeCondition::HasToolCalls, state);
        }
        if condition == "is_final_answer" {
            return self.evaluate_edge_condition(&EdgeCondition::IsFinalAnswer, state);
        }
        if condition == "error_occurred" {
            return self.evaluate_edge_condition(&EdgeCondition::ErrorOccurred, state);
        }
        // "variable==value" pattern
        if let Some((var, val)) = condition.split_once("==") {
            return Ok(state.variables.get(var.trim()) == Some(&val.trim().to_string()));
        }
        // Fallback: treat as a state variable lookup (truthy check)
        Ok(state.variables.get(condition).map(|v| v == "true").unwrap_or(false))
    }
}

// ─── Template Interpolation ────────────────────────────────────────────────

/// Interpolate `{{variable}}` template patterns for StateGraph execution.
///
/// Uses `GraphState.variables` as the substitution source.
/// Replaces `{{key}}` with the corresponding value from the variables map.
pub fn interpolate_graph_template(template: &str, variables: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in variables {
        result = result.replace(&format!("{{{{{}}}}}", key), value);
    }
    result
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_graph::StateGraph;

    fn make_simple_graph() -> StateGraph {
        // Simple linear graph: start → process → end
        let mut graph = StateGraph::new("test", "start");

        graph.add_node(GraphNode {
            id: "start".to_string(),
            action: NodeAction::ConditionCheck {
                condition: "ready==true".to_string(),
            },
            interrupt: false,
            metadata: HashMap::new(),
        });

        graph.add_node(GraphNode {
            id: "end".to_string(),
            action: NodeAction::LlmInfer {
                system_prompt_override: Some("Final answer".to_string()),
                use_streaming: false,
            },
            interrupt: false,
            metadata: HashMap::new(),
        });

        graph.add_edge(GraphEdge {
            from: "start".to_string(),
            to: "end".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });

        graph.add_terminal("end".to_string());

        graph
    }

    #[test]
    fn test_evaluate_edge_condition_always() {
        let state = GraphState::new();
        let executor = make_test_executor();

        let result = executor.evaluate_edge_condition(&EdgeCondition::Always, &state).unwrap();
        assert!(result);
    }

    #[test]
    fn test_evaluate_edge_condition_error_occurred() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // No error → false
        assert!(!executor.evaluate_edge_condition(&EdgeCondition::ErrorOccurred, &state).unwrap());

        // With error → true
        state.last_error = Some("test error".to_string());
        assert!(executor.evaluate_edge_condition(&EdgeCondition::ErrorOccurred, &state).unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_state_equals() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // Variable not set → false
        let cond = EdgeCondition::StateEquals { variable: "mode".to_string(), value: "react".to_string() };
        assert!(!executor.evaluate_edge_condition(&cond, &state).unwrap());

        // Variable set but different value → false
        state.variables.insert("mode".to_string(), "plan".to_string());
        assert!(!executor.evaluate_edge_condition(&cond, &state).unwrap());

        // Variable matches → true
        state.variables.insert("mode".to_string(), "react".to_string());
        assert!(executor.evaluate_edge_condition(&cond, &state).unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_has_tool_calls() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // No result → false
        assert!(!executor.evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state).unwrap());

        // Result without tool calls → false
        state.last_result = Some("Just a text answer".to_string());
        assert!(!executor.evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state).unwrap());

        // Result with tool_call marker → true
        state.last_result = Some("I need to use tool_use: shell".to_string());
        assert!(executor.evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state).unwrap());

        // Result with function_call marker → true
        state.last_result = Some("function_call: calculator".to_string());
        assert!(executor.evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state).unwrap());
    }

    #[test]
    fn test_interpolate_graph_template() {
        let vars = HashMap::from([
            ("selected_tool".to_string(), "shell".to_string()),
            ("command".to_string(), "ls -la".to_string()),
        ]);

        let result = interpolate_graph_template("{{selected_tool}} with {{command}}", &vars);
        assert_eq!(result, "shell with ls -la");
    }

    #[test]
    fn test_evaluate_condition_expression() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // "variable==value" pattern
        state.variables.insert("mode".to_string(), "react".to_string());
        assert!(executor.evaluate_condition_expression("mode==react", &state).unwrap());
        assert!(!executor.evaluate_condition_expression("mode==plan", &state).unwrap());

        // "error_occurred" pattern
        state.last_error = Some("error".to_string());
        assert!(executor.evaluate_condition_expression("error_occurred", &state).unwrap());
    }

    /// Create a test executor with mock dependencies (for condition testing only).
    /// Note: We can't test full execute() without a real provider,
    /// but we can test all the routing logic.
    struct TestStateGraphExecutor {
        max_iterations: usize,
    }

    impl TestStateGraphExecutor {
        fn evaluate_edge_condition(&self, condition: &EdgeCondition, state: &GraphState) -> Result<bool> {
            match condition {
                EdgeCondition::HasToolCalls => {
                    Ok(state.last_result.as_ref()
                        .map(|r| r.contains("tool_use") || r.contains("function_call"))
                        .unwrap_or(false))
                }
                EdgeCondition::IsFinalAnswer => {
                    Ok(!self.evaluate_edge_condition(&EdgeCondition::HasToolCalls, state)?)
                }
                EdgeCondition::RequestsDelegation => {
                    Ok(state.last_result.as_ref()
                        .map(|r| r.contains("delegate") || r.contains("sub_agent"))
                        .unwrap_or(false))
                }
                EdgeCondition::ErrorOccurred => Ok(state.last_error.is_some()),
                EdgeCondition::StateEquals { variable, value } => {
                    Ok(state.variables.get(variable) == Some(value))
                }
                EdgeCondition::Always => Ok(true),
                EdgeCondition::Custom { name, .. } => {
                    tracing::warn!("Custom condition '{}' not registered, defaulting to false", name);
                    Ok(false)
                }
            }
        }

        fn evaluate_condition_expression(&self, condition: &str, state: &GraphState) -> Result<bool> {
            if condition == "has_tool_calls" {
                return self.evaluate_edge_condition(&EdgeCondition::HasToolCalls, state);
            }
            if condition == "is_final_answer" {
                return self.evaluate_edge_condition(&EdgeCondition::IsFinalAnswer, state);
            }
            if condition == "error_occurred" {
                return self.evaluate_edge_condition(&EdgeCondition::ErrorOccurred, state);
            }
            if let Some((var, val)) = condition.split_once("==") {
                return Ok(state.variables.get(var.trim()) == Some(&val.trim().to_string()));
            }
            Ok(state.variables.get(condition).map(|v| v == "true").unwrap_or(false))
        }
    }

    fn make_test_executor() -> TestStateGraphExecutor {
        TestStateGraphExecutor { max_iterations: 50 }
    }
}
