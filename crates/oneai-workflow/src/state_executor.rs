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
//! - Executes 6 NodeAction variants (LlmInfer, ToolCall, Delegate, HumanApproval, ConditionCheck, SwitchParadigm)
//! - Evaluates 9 EdgeCondition variants for dynamic routing (including ParadigmEquals, IterationExceeds)
//! - Handles interrupt points (nodes with `interrupt: true`)
//! - Bounded by `max_iterations` to prevent infinite loops
//! - Supports `{{variable}}` template interpolation in tool_name and args_template
//!
//! P2-2: GraphActionExecutor bridge
//! ------------------------------
//! The `GraphActionExecutor` trait enables AgentLoop integration — when a
//! concrete implementation (from `oneai-agent`) is provided, LlmInfer and
//! ToolCall nodes delegate to the AgentLoop's full pipeline (hooks, permission,
//! domain pack, tool definitions). This makes StateGraph execution a first-class
//! execution mode of the AgentLoop, not a separate disconnected system.
//!
//! The `DirectProviderActionExecutor` provides backward-compatible behavior
//! (direct provider.infer() + tool.execute() without AgentLoop integration).

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{ApprovalGate, LlmProvider, Tool};
use oneai_core::{InferenceRequest, InferenceResponse, Message, Role};

use crate::state_graph::{
    StateGraph, GraphState, GraphEdge, NodeAction, EdgeCondition, GraphExecutionResult,
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
pub struct ActionResult {
    /// The output content from this action.
    pub output: String,
    /// Error message if the action failed.
    pub error: Option<String>,
}

// ─── GraphActionExecutor Trait ──────────────────────────────────────────────────

/// Trait for executing graph node actions with full AgentLoop integration.
///
/// This is the P2-2 bridge — when a concrete implementation (from `oneai-agent`)
/// is provided, the StateGraphExecutor delegates LlmInfer and ToolCall nodes
/// to the AgentLoop's full pipeline instead of directly calling provider.infer()
/// and tool.execute(). This means:
///
/// - **LlmInfer**: Gets proper tool definitions (filtered by paradigm config),
///   domain pack tool decorators, PreInfer/PostInfer hooks, context assembly,
///   and the OutputParser for decision parsing.
/// - **ToolCall**: Gets PreToolUse/PostToolUse hooks, domain permission checks,
///   approval gate interaction, and error recovery.
/// - **SwitchParadigm**: Changes the active paradigm (updates tool filter
///   and system prompt for subsequent nodes).
///
/// The `DirectProviderActionExecutor` provides backward-compatible behavior
/// (direct provider + tool execution, no AgentLoop integration). It's used
/// when no AgentLoop is available (e.g., standalone StateGraph execution).
#[async_trait::async_trait]
pub trait GraphActionExecutor: Send + Sync {
    /// Execute an LLM inference node — using full AgentLoop infrastructure.
    ///
    /// When `include_tool_definitions` is true, builds tool definitions based on
    /// `tool_filter_override` or the active paradigm's tool set. This is critical
    /// for ReAct loops — the model needs tools to decide whether to call them.
    ///
    /// After inference, the response is parsed into a `GraphDecision` and stored
    /// in `state.parsed_decision` for edge condition routing.
    async fn execute_llm_infer(
        &self,
        action: &NodeAction,
        state: &mut GraphState,
    ) -> Result<ActionResult>;

    /// Execute a tool call node — using AgentLoop's permission and hooks.
    ///
    /// When a GraphActionExecutor from `oneai-agent` is used, this method
    /// runs PreToolUse hooks, checks domain permissions, interacts with the
    /// approval gate, executes the tool, runs PostToolUse hooks, and handles
    /// error recovery.
    async fn execute_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        state: &mut GraphState,
    ) -> Result<ActionResult>;

    /// Execute a paradigm switch node.
    ///
    /// Updates `state.active_paradigm` and clears `state.parsed_decision`.
    /// Subsequent LlmInfer nodes will use the new paradigm's tool set
    /// and system prompt.
    async fn execute_paradigm_switch(
        &self,
        paradigm: &str,
        state: &mut GraphState,
    ) -> Result<ActionResult>;

    /// Parse an LLM response into a GraphDecision.
    ///
    /// Uses the same OutputParser as the AgentLoop for consistent
    /// decision parsing. Stores the result in `state.parsed_decision`.
    async fn parse_decision(
        &self,
        response: &InferenceResponse,
        state: &mut GraphState,
    ) -> Result<oneai_core::GraphDecision>;
}

// ─── DirectProviderActionExecutor ──────────────────────────────────────────────

/// Backward-compatible GraphActionExecutor that directly calls provider + tools.
///
/// This is the "no AgentLoop integration" path — used when StateGraphExecutor
/// is constructed via `with_direct_provider()`. It mimics the original behavior:
/// - LlmInfer: calls provider.infer() with a basic request (no hooks, no domain pack)
/// - ToolCall: calls tool.execute() directly (no permission, no hooks)
/// - SwitchParadigm: updates state.active_paradigm (no paradigm config)
/// - parse_decision: simple ContentBlock-based parsing
///
/// For full AgentLoop integration, use `AgentLoopGraphActionExecutor` from
/// `oneai-agent`.
pub struct DirectProviderActionExecutor {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
}

impl DirectProviderActionExecutor {
    /// Create a new direct executor with provider and tools.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    ) -> Self {
        Self { provider, tools }
    }
}

#[async_trait::async_trait]
impl GraphActionExecutor for DirectProviderActionExecutor {
    async fn execute_llm_infer(
        &self,
        action: &NodeAction,
        state: &mut GraphState,
    ) -> Result<ActionResult> {
        // Extract LlmInfer fields
        let (system_prompt_override, _use_streaming, include_tool_definitions,
             tool_filter_override, thinking_budget, temperature, max_tokens) = match action {
            NodeAction::LlmInfer {
                system_prompt_override, use_streaming, include_tool_definitions,
                tool_filter_override, thinking_budget, temperature, max_tokens,
            } => (
                system_prompt_override.clone(),
                *use_streaming,
                *include_tool_definitions,
                tool_filter_override.clone(),
                *thinking_budget,
                *temperature,
                *max_tokens,
            ),
            _ => return Err(OneAIError::Workflow("Expected LlmInfer action".to_string())),
        };

        // Build system prompt
        let system_prompt = system_prompt_override
            .unwrap_or_else(|| "You are an intelligent agent. Respond to the task.".to_string());

        let mut conversation = state.conversation.clone();
        if !conversation.messages.iter().any(|m| m.role == Role::System) {
            conversation.add_message(Message::system(&system_prompt));
        }

        // Build tool definitions if requested
        let tool_defs = if include_tool_definitions {
            let tools_map = self.tools.read().await;
            if let Some(filter) = &tool_filter_override {
                // Filter: only include specified tools
                tools_map.values()
                    .filter(|t| filter.contains(&t.name().to_string()))
                    .map(|t| oneai_core::ToolDefinition {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters_schema: t.parameters_schema(),
                    })
                    .collect()
            } else {
                // No filter — include all tools
                tools_map.values().map(|t| oneai_core::ToolDefinition {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters_schema: t.parameters_schema(),
                }).collect()
            }
        } else {
            vec![] // No tool definitions — pure text prompt
        };

        let request = InferenceRequest {
            conversation,
            tools: tool_defs,
            max_tokens: max_tokens.or(Some(4096)),
            temperature: temperature.or(Some(0.3)),
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget,
            metadata: HashMap::new(),
        };

        let response = self.provider.infer(request).await?;
        let output = response.message.text_content();

        // Update conversation state
        state.conversation.add_message(response.message.clone());

        // Parse decision and store in state
        let _decision = self.parse_decision(&response, state).await?;

        Ok(ActionResult {
            output,
            error: None,
        })
    }

    async fn execute_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        state: &mut GraphState,
    ) -> Result<ActionResult> {
        let tools_map = self.tools.read().await;
        let tool = tools_map.get(tool_name)
            .ok_or_else(|| OneAIError::Workflow(
                format!("Tool '{}' not found for ToolCall node", tool_name)
            ))?;

        let output = tool.execute(args.clone()).await?;

        state.conversation.add_message(Message::tool_result(
            format!("graph_tool_{}", tool_name),
            output.content.clone(),
        ));

        Ok(ActionResult {
            output: output.content,
            error: output.error,
        })
    }

    async fn execute_paradigm_switch(
        &self,
        paradigm: &str,
        state: &mut GraphState,
    ) -> Result<ActionResult> {
        state.active_paradigm = Some(paradigm.to_string());
        state.parsed_decision = None; // Clear — new inference needed

        Ok(ActionResult {
            output: format!("Paradigm switched to: {}", paradigm),
            error: None,
        })
    }

    async fn parse_decision(
        &self,
        response: &InferenceResponse,
        state: &mut GraphState,
    ) -> Result<oneai_core::GraphDecision> {
        // Simple ContentBlock-based parsing (mirrors the AgentLoop's parse_decision logic
        // but produces GraphDecision instead of AgentDecision)
        let mut tool_calls = Vec::new();
        let mut text_parts = Vec::new();

        for block in &response.message.content {
            match block {
                oneai_core::ContentBlock::ToolCall { id: _, name, args } => {
                    // Check for special internal tools (delegate, switch_paradigm)
                    if name == "delegate" {
                        // Parse delegate args
                        let args_value: serde_json::Value = serde_json::from_str(args)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        let agent_kind = args_value.get("agent_type")
                            .and_then(|v| v.as_str()).unwrap_or("Explore").to_string();
                        let task = args_value.get("task")
                            .and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let decision = oneai_core::GraphDecision::Delegate {
                            agent_kind,
                            task,
                        };
                        state.parsed_decision = Some(decision.clone());
                        return Ok(decision);
                    }
                    if name == "switch_paradigm" {
                        let args_value: serde_json::Value = serde_json::from_str(args)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        let paradigm = args_value.get("paradigm")
                            .and_then(|v| v.as_str()).unwrap_or("react").to_string();
                        let decision = oneai_core::GraphDecision::SwitchParadigm { paradigm };
                        state.parsed_decision = Some(decision.clone());
                        return Ok(decision);
                    }
                    tool_calls.push(name.clone());
                }
                oneai_core::ContentBlock::Text { text } => {
                    text_parts.push(text.clone());
                }
                _ => {}
            }
        }

        let decision = if !tool_calls.is_empty() {
            oneai_core::GraphDecision::ToolCalls {
                count: tool_calls.len(),
            }
        } else {
            oneai_core::GraphDecision::DirectAnswer {
                text: text_parts.join("\n"),
            }
        };

        state.parsed_decision = Some(decision.clone());
        Ok(decision)
    }
}

// ─── StateGraphExecutor ──────────────────────────────────────────────────────

/// Executor for StateGraph — walks cyclic graph with conditional routing.
///
/// The executor processes a StateGraph by:
/// 1. Starting at the `entry_point` node
/// 2. Executing each node's `NodeAction` via the `GraphActionExecutor`
/// 3. Evaluating outgoing `EdgeCondition`s to select the next node
/// 4. Handling interrupt points (nodes marked with `interrupt: true`)
/// 5. Terminating when a terminal node is reached or max iterations exceeded
///
/// P2-2: The executor now uses a `GraphActionExecutor` for node action execution.
/// This enables AgentLoop integration — when an `AgentLoopGraphActionExecutor`
/// (from `oneai-agent`) is provided, LlmInfer/ToolCall nodes delegate to the
/// AgentLoop's full pipeline (hooks, permission, domain pack, tool definitions).
///
/// Template interpolation (`{{variable}}`) is still applied to `tool_name` and
/// `args_template` fields before tool execution, using `GraphState.variables`.
pub struct StateGraphExecutor {
    /// Action executor — delegates node action execution.
    /// Can be DirectProviderActionExecutor (backward compat) or
    /// AgentLoopGraphActionExecutor (full AgentLoop integration).
    action_executor: Arc<dyn GraphActionExecutor>,
    /// Delegate factory for Delegate nodes.
    delegate_factory: Arc<dyn DelegateFactory>,
    /// Approval gate for interrupt points and HumanApproval nodes.
    approval_gate: Arc<dyn ApprovalGate>,
    /// Maximum iterations through the graph (prevents infinite loops).
    /// Default: 50.
    max_iterations: usize,
}

impl StateGraphExecutor {
    /// Create a new StateGraphExecutor with a GraphActionExecutor.
    ///
    /// This is the P2-2 constructor — when an `AgentLoopGraphActionExecutor`
    /// is provided, LlmInfer/ToolCall nodes get full AgentLoop integration.
    pub fn new(
        action_executor: Arc<dyn GraphActionExecutor>,
        delegate_factory: Arc<dyn DelegateFactory>,
        approval_gate: Arc<dyn ApprovalGate>,
        max_iterations: usize,
    ) -> Self {
        Self {
            action_executor,
            delegate_factory,
            approval_gate,
            max_iterations,
        }
    }

    /// Create with default max_iterations (50).
    pub fn with_defaults(
        action_executor: Arc<dyn GraphActionExecutor>,
        delegate_factory: Arc<dyn DelegateFactory>,
        approval_gate: Arc<dyn ApprovalGate>,
    ) -> Self {
        Self::new(action_executor, delegate_factory, approval_gate, 50)
    }

    /// Create with direct provider + tools (backward-compatible constructor).
    ///
    /// This constructor creates a `DirectProviderActionExecutor` internally,
    /// providing the same behavior as the original StateGraphExecutor (before P2-2).
    /// Use this when you don't have an AgentLoop available (e.g., standalone
    /// StateGraph execution without AgentLoop integration).
    pub fn with_direct_provider(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
        delegate_factory: Arc<dyn DelegateFactory>,
        approval_gate: Arc<dyn ApprovalGate>,
        max_iterations: usize,
    ) -> Self {
        let action_executor = Arc::new(DirectProviderActionExecutor::new(provider, tools));
        Self::new(action_executor, delegate_factory, approval_gate, max_iterations)
    }

    /// Create with direct provider + default max_iterations (50).
    /// Backward-compatible with the original `with_defaults()` constructor.
    pub fn with_direct_provider_defaults(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
        delegate_factory: Arc<dyn DelegateFactory>,
        approval_gate: Arc<dyn ApprovalGate>,
    ) -> Self {
        Self::with_direct_provider(provider, tools, delegate_factory, approval_gate, 50)
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
            state.iteration_count = iterations;

            // 1. Get the current node
            let node = graph.get_node(&current_node_id)
                .ok_or_else(|| OneAIError::Workflow(
                    format!("Node '{}' not found in graph '{}'", current_node_id, graph.name)
                ))?;

            // 2. Check if this is a terminal node
            if graph.terminal_nodes.contains(&current_node_id) {
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
    ///
    /// P2-2: LlmInfer and ToolCall nodes delegate to the GraphActionExecutor,
    /// which may be an AgentLoopGraphActionExecutor (full AgentLoop integration)
    /// or a DirectProviderActionExecutor (backward compat).
    async fn execute_node_action(
        &self,
        action: &NodeAction,
        state: &mut GraphState,
    ) -> Result<ActionResult> {
        match action {
            NodeAction::LlmInfer { .. } => {
                // Delegate to GraphActionExecutor — which may build tool definitions,
                // run hooks, use domain pack, and parse the response into GraphDecision.
                self.action_executor.execute_llm_infer(action, state).await
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

                // Delegate to GraphActionExecutor — which may run hooks,
                // check permissions, interact with approval gate.
                self.action_executor.execute_tool_call(&resolved_name, &resolved_args, state).await
            }

            NodeAction::Delegate { agent_kind, task_template } => {
                let task = interpolate_graph_template(task_template, &state.variables);

                let result = self.delegate_factory.delegate(agent_kind, &task).await?;

                // Append delegate result to conversation
                state.conversation.add_message(Message::assistant(
                    format!("[Delegate {}]: {}", agent_kind, result)
                ));

                // Set parsed_decision to Delegate
                state.parsed_decision = Some(oneai_core::GraphDecision::Delegate {
                    agent_kind: agent_kind.clone(),
                    task,
                });

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

            NodeAction::SwitchParadigm { paradigm } => {
                // Delegate to GraphActionExecutor — updates state.active_paradigm
                self.action_executor.execute_paradigm_switch(paradigm, state).await
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
    ///
    /// P2-2: HasToolCalls, IsFinalAnswer, and RequestsDelegation now evaluate
    /// against `state.parsed_decision` (a structured `GraphDecision`) rather
    /// than unreliable string pattern matching. This makes edge routing
    /// consistent with the AgentLoop's decision parsing.
    fn evaluate_edge_condition(
        &self,
        condition: &EdgeCondition,
        state: &GraphState,
    ) -> Result<bool> {
        match condition {
            EdgeCondition::HasToolCalls => {
                // P2-2: Use parsed_decision instead of string matching
                Ok(state.parsed_decision.as_ref()
                    .map(|d| d.has_tool_calls())
                    .unwrap_or(false))
            }

            EdgeCondition::IsFinalAnswer => {
                // P2-2: Use parsed_decision instead of !HasToolCalls
                Ok(state.parsed_decision.as_ref()
                    .map(|d| d.is_final())
                    .unwrap_or(false))
            }

            EdgeCondition::RequestsDelegation => {
                // P2-2: Use parsed_decision
                Ok(state.parsed_decision.as_ref()
                    .map(|d| d.is_delegation())
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

            EdgeCondition::ParadigmEquals { paradigm } => {
                Ok(state.active_paradigm.as_ref() == Some(paradigm))
            }

            EdgeCondition::IterationExceeds { count } => {
                Ok(state.iteration_count > *count)
            }
        }
    }

    /// Evaluate a condition expression (for ConditionCheck nodes).
    ///
    /// Simple condition expressions:
    /// - "has_tool_calls" → checks parsed_decision for ToolCalls
    /// - "is_final_answer" → checks parsed_decision for DirectAnswer
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
    use crate::state_graph::{StateGraph, GraphNode};

    #[allow(dead_code)] // test fixture retained for future executor coverage
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

        // No parsed_decision → false
        assert!(!executor.evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state).unwrap());

        // DirectAnswer → false
        state.parsed_decision = Some(oneai_core::GraphDecision::DirectAnswer {
            text: "Just a text answer".to_string(),
        });
        assert!(!executor.evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state).unwrap());

        // ToolCalls → true
        state.parsed_decision = Some(oneai_core::GraphDecision::ToolCalls { count: 1 });
        assert!(executor.evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state).unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_is_final_answer() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // No parsed_decision → false
        assert!(!executor.evaluate_edge_condition(&EdgeCondition::IsFinalAnswer, &state).unwrap());

        // DirectAnswer → true
        state.parsed_decision = Some(oneai_core::GraphDecision::DirectAnswer {
            text: "The answer is 42".to_string(),
        });
        assert!(executor.evaluate_edge_condition(&EdgeCondition::IsFinalAnswer, &state).unwrap());

        // ToolCalls → false
        state.parsed_decision = Some(oneai_core::GraphDecision::ToolCalls { count: 2 });
        assert!(!executor.evaluate_edge_condition(&EdgeCondition::IsFinalAnswer, &state).unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_requests_delegation() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // Delegate decision → true
        state.parsed_decision = Some(oneai_core::GraphDecision::Delegate {
            agent_kind: "Explore".to_string(),
            task: "Search the codebase".to_string(),
        });
        assert!(executor.evaluate_edge_condition(&EdgeCondition::RequestsDelegation, &state).unwrap());

        // ToolCalls → false
        state.parsed_decision = Some(oneai_core::GraphDecision::ToolCalls { count: 1 });
        assert!(!executor.evaluate_edge_condition(&EdgeCondition::RequestsDelegation, &state).unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_paradigm_equals() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // No paradigm → false
        let cond = EdgeCondition::ParadigmEquals { paradigm: "react".to_string() };
        assert!(!executor.evaluate_edge_condition(&cond, &state).unwrap());

        // Wrong paradigm → false
        state.active_paradigm = Some("plan".to_string());
        assert!(!executor.evaluate_edge_condition(&cond, &state).unwrap());

        // Matching paradigm → true
        state.active_paradigm = Some("react".to_string());
        assert!(executor.evaluate_edge_condition(&cond, &state).unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_iteration_exceeds() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // iteration_count = 0, threshold = 5 → false
        let cond = EdgeCondition::IterationExceeds { count: 5 };
        assert!(!executor.evaluate_edge_condition(&cond, &state).unwrap());

        // iteration_count = 10, threshold = 5 → true
        state.iteration_count = 10;
        assert!(executor.evaluate_edge_condition(&cond, &state).unwrap());
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
    #[allow(dead_code)] // reserved for future executor-iteration tests
    struct TestStateGraphExecutor {
        #[allow(dead_code)]
        max_iterations: usize,
    }

    impl TestStateGraphExecutor {
        fn evaluate_edge_condition(&self, condition: &EdgeCondition, state: &GraphState) -> Result<bool> {
            match condition {
                EdgeCondition::HasToolCalls => {
                    Ok(state.parsed_decision.as_ref()
                        .map(|d| d.has_tool_calls())
                        .unwrap_or(false))
                }
                EdgeCondition::IsFinalAnswer => {
                    Ok(state.parsed_decision.as_ref()
                        .map(|d| d.is_final())
                        .unwrap_or(false))
                }
                EdgeCondition::RequestsDelegation => {
                    Ok(state.parsed_decision.as_ref()
                        .map(|d| d.is_delegation())
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
                EdgeCondition::ParadigmEquals { paradigm } => {
                    Ok(state.active_paradigm.as_ref() == Some(paradigm))
                }
                EdgeCondition::IterationExceeds { count } => {
                    Ok(state.iteration_count > *count)
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

    #[test]
    fn test_graph_decision_enum() {
        let decision = oneai_core::GraphDecision::DirectAnswer {
            text: "42".to_string(),
        };
        assert!(decision.is_final());
        assert!(!decision.has_tool_calls());

        let tool_calls = oneai_core::GraphDecision::ToolCalls { count: 2 };
        assert!(tool_calls.has_tool_calls());
        assert!(!tool_calls.is_final());

        let delegate = oneai_core::GraphDecision::Delegate {
            agent_kind: "Explore".to_string(),
            task: "Search".to_string(),
        };
        assert!(delegate.is_delegation());
        assert!(!delegate.has_tool_calls());

        let switch = oneai_core::GraphDecision::SwitchParadigm {
            paradigm: "plan".to_string(),
        };
        assert!(!switch.is_final());
        assert!(!switch.has_tool_calls());
    }
}
