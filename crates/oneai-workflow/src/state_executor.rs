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

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures::future::join_all;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{InteractionGate, LlmProvider, PermissionResolver, Tool};
use oneai_core::{InferenceRequest, InferenceResponse, Message, PermissionAction, Role};

use crate::state_graph::{
    EdgeCondition, GraphEdge, GraphExecutionResult, GraphState, NodeAction, StateGraph,
};

/// User-provided evaluator for an `EdgeCondition::Custom { name, .. }` variant.
///
/// Registered on `StateGraphExecutor` via `with_custom_condition` and looked up
/// by `name` during edge routing. Receives the current `GraphState` and returns
/// whether the edge should be taken.
pub type CustomConditionFn = Arc<dyn Fn(&GraphState) -> bool + Send + Sync>;

/// Evaluate a `Custom` edge condition against a registry of user callbacks.
///
/// Looks up `name` in `registry`; on hit, invokes the callback with `state`.
/// Unregistered conditions log a warning and default to `false` (edges with no
/// registered evaluator never fire — fail-closed). Factored out so the lookup
/// can be unit-tested without constructing a full `StateGraphExecutor`.
pub(crate) fn evaluate_custom_condition(
    registry: &HashMap<String, CustomConditionFn>,
    name: &str,
    description: &str,
    state: &GraphState,
) -> bool {
    match registry.get(name) {
        Some(evaluator) => evaluator(state),
        None => {
            tracing::warn!(
                "Custom condition '{}' ('{}') not registered. Defaulting to false.",
                name,
                description
            );
            false
        }
    }
}

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
            "Delegate action '{}' not supported — no DelegateFactory configured",
            agent_kind
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

/// Outcome of running one node in a parallel frontier fork (see
/// [`StateGraphExecutor::execute_frontier_parallel`]).
///
/// Each branch owns its isolated state clone (so concurrent branches never
/// race on shared mutable state) plus the targets it routed to. The caller
/// merges branches back deterministically by `node_id` (BTreeSet) order.
struct BranchResult {
    /// The frontier node this branch ran.
    node_id: String,
    /// The action's output/error for this branch.
    action_result: ActionResult,
    /// This branch's final isolated state (clone + its own writes).
    branch_state: GraphState,
    /// Targets this branch's outgoing edges routed to (computed against
    /// `branch_state` — consuming this branch's own `parsed_decision`).
    next_targets: Vec<String>,
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
    /// Optional domain permission resolver. When present, a `ToolCall` node is
    /// checked against domain policy before `tool.execute()` — closing the
    /// stateless-graph bypass of gap-analysis P1. `None` = pre-existing
    /// "no permission, no hooks" behaviour.
    permission_resolver: Option<Arc<dyn PermissionResolver>>,
}

impl DirectProviderActionExecutor {
    /// Create a new direct executor with provider and tools.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
    ) -> Self {
        Self {
            provider,
            tools,
            permission_resolver: None,
        }
    }

    /// Attach a domain permission resolver so `ToolCall` nodes honour DomainPack
    /// `deny_by_default` policy on this stateless path too.
    pub fn with_permission_resolver(mut self, resolver: Arc<dyn PermissionResolver>) -> Self {
        self.permission_resolver = Some(resolver);
        self
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
        let (
            system_prompt_override,
            _use_streaming,
            include_tool_definitions,
            tool_filter_override,
            thinking_budget,
            temperature,
            max_tokens,
        ) = match action {
            NodeAction::LlmInfer {
                system_prompt_override,
                use_streaming,
                include_tool_definitions,
                tool_filter_override,
                thinking_budget,
                temperature,
                max_tokens,
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
                tools_map
                    .values()
                    .filter(|t| filter.contains(&t.name().to_string()))
                    .map(|t| oneai_core::ToolDefinition {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters_schema: t.parameters_schema(),
                    })
                    .collect()
            } else {
                // No filter — include all tools
                tools_map
                    .values()
                    .map(|t| oneai_core::ToolDefinition {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters_schema: t.parameters_schema(),
                    })
                    .collect()
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
        // Domain permission gate (highest priority). Closes the stateless-graph
        // bypass of gap-analysis P1: this path used to call `tool.execute()`
        // directly with no permission check, ignoring DomainPack
        // `deny_by_default`.
        if let Some(resolver) = self.permission_resolver.as_ref() {
            match resolver.resolve(tool_name, args) {
                PermissionAction::Deny { reason } => {
                    tracing::warn!(
                        "ToolCall node '{}' denied by domain policy: {}",
                        tool_name,
                        reason
                    );
                    return Ok(ActionResult {
                        output: format!("Denied by domain policy: {}", reason),
                        error: Some(format!("Denied by domain policy: {}", reason)),
                    });
                }
                PermissionAction::AutoApprove => {
                    tracing::info!(
                        "ToolCall node '{}' auto-approved by domain policy",
                        tool_name
                    );
                }
                // No interaction gate on this stateless path — RequireConfirmation
                // and UseDefaultPermission both fall through to direct execution
                // (the stateless executor is the "no AgentLoop" fallback; full
                // confirmation gating requires the AgentLoopGraphActionExecutor).
                _ => {}
            }
        }

        let tools_map = self.tools.read().await;
        let tool = tools_map.get(tool_name).ok_or_else(|| {
            OneAIError::Workflow(format!("Tool '{}' not found for ToolCall node", tool_name))
        })?;

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
                        let args_value: serde_json::Value =
                            serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({}));
                        let agent_kind = args_value
                            .get("agent_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Explore")
                            .to_string();
                        let task = args_value
                            .get("task")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let decision = oneai_core::GraphDecision::Delegate { agent_kind, task };
                        state.parsed_decision = Some(decision.clone());
                        return Ok(decision);
                    }
                    if name == "switch_paradigm" {
                        let args_value: serde_json::Value =
                            serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({}));
                        let paradigm = args_value
                            .get("paradigm")
                            .and_then(|v| v.as_str())
                            .unwrap_or("react")
                            .to_string();
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
    interaction_gate: Arc<dyn InteractionGate>,
    /// Maximum iterations through the graph (prevents infinite loops).
    /// Default: 50.
    max_iterations: usize,
    /// Registered evaluators for `EdgeCondition::Custom { name, .. }`, keyed by
    /// condition name. Unregistered custom conditions warn and default to false.
    custom_conditions: HashMap<String, CustomConditionFn>,
}

impl StateGraphExecutor {
    /// Create a new StateGraphExecutor with a GraphActionExecutor.
    ///
    /// This is the P2-2 constructor — when an `AgentLoopGraphActionExecutor`
    /// is provided, LlmInfer/ToolCall nodes get full AgentLoop integration.
    pub fn new(
        action_executor: Arc<dyn GraphActionExecutor>,
        delegate_factory: Arc<dyn DelegateFactory>,
        interaction_gate: Arc<dyn InteractionGate>,
        max_iterations: usize,
    ) -> Self {
        Self {
            action_executor,
            delegate_factory,
            interaction_gate,
            max_iterations,
            custom_conditions: HashMap::new(),
        }
    }

    /// Create with default max_iterations (50).
    pub fn with_defaults(
        action_executor: Arc<dyn GraphActionExecutor>,
        delegate_factory: Arc<dyn DelegateFactory>,
        interaction_gate: Arc<dyn InteractionGate>,
    ) -> Self {
        Self::new(action_executor, delegate_factory, interaction_gate, 50)
    }

    /// Register a user-provided evaluator for `EdgeCondition::Custom { name, .. }`.
    ///
    /// When the graph routes along an edge whose condition is `Custom`, the
    /// executor looks up the registered callback by `name` and evaluates it
    /// against the current `GraphState`. Unregistered conditions warn and
    /// default to `false`. Multiple registrations may be chained since this
    /// consumes and returns `Self`.
    pub fn with_custom_condition(
        mut self,
        name: impl Into<String>,
        evaluator: CustomConditionFn,
    ) -> Self {
        self.custom_conditions.insert(name.into(), evaluator);
        self
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
        interaction_gate: Arc<dyn InteractionGate>,
        max_iterations: usize,
    ) -> Self {
        let action_executor = Arc::new(DirectProviderActionExecutor::new(provider, tools));
        Self::new(
            action_executor,
            delegate_factory,
            interaction_gate,
            max_iterations,
        )
    }

    /// Create with direct provider + default max_iterations (50).
    /// Backward-compatible with the original `with_defaults()` constructor.
    pub fn with_direct_provider_defaults(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<tokio::sync::RwLock<HashMap<String, Arc<dyn Tool>>>>,
        delegate_factory: Arc<dyn DelegateFactory>,
        interaction_gate: Arc<dyn InteractionGate>,
    ) -> Self {
        Self::with_direct_provider(provider, tools, delegate_factory, interaction_gate, 50)
    }

    /// Execute a StateGraph starting from its entry point.
    ///
    /// Walks the graph as a **frontier** of ready nodes (not a single walker).
    /// Each iteration:
    /// 1. If any frontier node is terminal, execute the deterministic-first
    ///    one alone and return `completed: true`.
    /// 2. Otherwise execute every frontier node — sequentially when the
    ///    frontier has one node (the historical ReAct/conditional case, no
    ///    clone overhead, behaviour identical to the pre-frontier loop) or any
    ///    node carries an `interrupt` point; concurrently when the frontier
    ///    fans out ≥2 non-interrupt nodes (a fork — each branch runs on an
    ///    isolated state clone, results merged back deterministically by node
    ///    ID order).
    /// 3. Route each executed node's outgoing edges (`route_next_nodes` — all
    ///    satisfiable edges, so a node with two `Always` edges forks both
    ///    targets). The union of targets is the next frontier; a target
    ///    reached by multiple branches appears once (natural join — it runs
    ///    only after the barrier since all branches completed this iteration).
    /// 4. Terminate when the frontier is empty, `should_terminate` is set, or
    ///    `max_iterations` is exceeded.
    ///
    /// Single-branch graphs (the common ReAct/conditional case) keep frontier
    /// size 1 throughout → the sequential path → identical to the old loop
    /// (regression guarantee). Forks (≥2 simultaneous satisfiable edges) are
    /// the only behaviour that differs, and they couldn't be expressed before.
    pub async fn execute(
        &self,
        graph: &StateGraph,
        initial_state: GraphState,
    ) -> Result<GraphExecutionResult> {
        // BTreeSet, not HashSet: deterministic iteration order so terminal
        // pick, merge order, and next-frontier composition are reproducible
        // across runs (戒律 #6 — invariants, not frozen values).
        let frontier: BTreeSet<String> = std::iter::once(graph.entry_point.clone()).collect();
        self.run_walk(graph, initial_state, frontier, 0, Vec::new(), None)
            .await
    }

    /// Execute a StateGraph with durable checkpointing (gap-analysis P2 #14).
    ///
    /// The walk state (frontier, iterations, [`GraphState`]) is persisted to
    /// `store` under `run_id` before the first node and after every iteration
    /// boundary. If the process dies mid-walk, [`Self::resume`] continues
    /// from the last checkpoint. Completed runs delete their checkpoint.
    pub async fn execute_with_checkpoints(
        &self,
        graph: &StateGraph,
        initial_state: GraphState,
        run_id: impl Into<String>,
        store: Arc<dyn crate::checkpoint::GraphCheckpointStore>,
    ) -> Result<GraphExecutionResult> {
        let run_id = run_id.into();
        let frontier: BTreeSet<String> = std::iter::once(graph.entry_point.clone()).collect();
        store.save(&crate::checkpoint::GraphCheckpoint {
            run_id: run_id.clone(),
            graph_name: graph.name.clone(),
            frontier: frontier.clone(),
            iterations: 0,
            state: initial_state.clone(),
            interrupt_checkpoints: Vec::new(),
            completed: false,
            saved_at: chrono::Utc::now().to_rfc3339(),
        })?;
        self.run_walk(
            graph,
            initial_state,
            frontier,
            0,
            Vec::new(),
            Some((run_id, store)),
        )
        .await
    }

    /// Resume a checkpointed StateGraph walk (gap-analysis P2 #14).
    ///
    /// Loads the checkpoint saved under `run_id`, validates it belongs to
    /// `graph`, and continues the walk from the saved frontier/iteration
    /// count with the saved [`GraphState`]. A checkpoint already marked
    /// `completed` returns its state without re-running. Errors when no
    /// checkpoint exists or the graph name mismatches.
    pub async fn resume(
        &self,
        graph: &StateGraph,
        run_id: &str,
        store: Arc<dyn crate::checkpoint::GraphCheckpointStore>,
    ) -> Result<GraphExecutionResult> {
        let checkpoint = store.load(run_id)?.ok_or_else(|| {
            OneAIError::Workflow(format!(
                "No checkpoint found for graph run '{}' — nothing to resume",
                run_id
            ))
        })?;
        if checkpoint.graph_name != graph.name {
            return Err(OneAIError::Workflow(format!(
                "Checkpoint '{}' belongs to graph '{}' but resume was requested on '{}'",
                run_id, checkpoint.graph_name, graph.name
            )));
        }
        if checkpoint.completed {
            return Ok(GraphExecutionResult {
                name: graph.name.clone(),
                final_state: checkpoint.state,
                completed: true,
                terminal_node: None,
                iterations: checkpoint.iterations,
                interrupt_checkpoints: checkpoint.interrupt_checkpoints,
            });
        }
        // Resuming is an explicit "try again" — clear the interruption flags
        // the failed walk left behind (interrupt denial sets
        // `should_terminate` + `last_error`; keeping them would re-terminate
        // the walk instantly on resume).
        let mut state = checkpoint.state;
        state.should_terminate = false;
        state.last_error = None;
        self.run_walk(
            graph,
            state,
            checkpoint.frontier,
            checkpoint.iterations,
            checkpoint.interrupt_checkpoints,
            Some((run_id.to_string(), store)),
        )
        .await
    }

    /// The shared frontier walk used by [`Self::execute`],
    /// [`Self::execute_with_checkpoints`] and [`Self::resume`]. When
    /// `checkpoint` is `Some((run_id, store))`, the walk state is persisted
    /// after every iteration boundary and cleaned up (or finalized) on exit.
    async fn run_walk(
        &self,
        graph: &StateGraph,
        mut state: GraphState,
        mut frontier: BTreeSet<String>,
        mut iterations: usize,
        mut interrupt_checkpoints: Vec<String>,
        checkpoint: Option<(String, Arc<dyn crate::checkpoint::GraphCheckpointStore>)>,
    ) -> Result<GraphExecutionResult> {
        while iterations < self.max_iterations {
            iterations += 1;
            state.iteration_count = iterations;

            if frontier.is_empty() {
                break;
            }

            // 1. Terminal: if any frontier node is terminal, execute the
            //    deterministic-first one alone and complete. Terminals don't
            //    participate in forks (a fork into a terminal is an edge case;
            //    we pick one deterministically).
            if let Some(term_id) = frontier
                .iter()
                .find(|id| graph.terminal_nodes.contains(id))
                .cloned()
            {
                let node = graph.get_node(&term_id).ok_or_else(|| {
                    OneAIError::Workflow(format!(
                        "Terminal node '{}' not found in graph '{}'",
                        term_id, graph.name
                    ))
                })?;
                let action_result = self.execute_node_action(&node.action, &mut state).await?;
                state.last_result = Some(action_result.output.clone());
                state.last_error = action_result.error.clone();

                // Completed — the checkpoint (if any) has served its purpose.
                if let Some((run_id, store)) = &checkpoint {
                    if let Err(e) = store.delete(run_id) {
                        tracing::warn!("failed to delete completed checkpoint '{}': {}", run_id, e);
                    }
                }

                return Ok(GraphExecutionResult {
                    name: graph.name.clone(),
                    final_state: state,
                    completed: true,
                    terminal_node: Some(term_id),
                    iterations,
                    interrupt_checkpoints,
                });
            }

            // 2. Decide path: sequential (single-node frontier OR any interrupt)
            //    vs parallel fork (≥2 nodes, none interrupt).
            let has_interrupt = frontier
                .iter()
                .any(|id| graph.get_node(id).map(|n| n.interrupt).unwrap_or(false));

            let next_targets: Vec<String> = if frontier.len() == 1 || has_interrupt {
                self.execute_frontier_sequential(
                    graph,
                    &frontier,
                    &mut state,
                    &mut interrupt_checkpoints,
                )
                .await?
            } else {
                self.execute_frontier_parallel(graph, &frontier, &mut state)
                    .await?
            };

            if state.should_terminate {
                break;
            }

            if next_targets.is_empty() {
                tracing::warn!(
                    "No matching edge condition from frontier {:?} in graph '{}'. Terminating.",
                    frontier,
                    graph.name
                );
                break;
            }

            frontier = next_targets.into_iter().collect();

            // Durable execution (gap P2 #14): persist the walk state at the
            // iteration boundary so a crash/restart resumes from here.
            if let Some((run_id, store)) = &checkpoint {
                store.save(&crate::checkpoint::GraphCheckpoint {
                    run_id: run_id.clone(),
                    graph_name: graph.name.clone(),
                    frontier: frontier.clone(),
                    iterations,
                    state: state.clone(),
                    interrupt_checkpoints: interrupt_checkpoints.clone(),
                    completed: false,
                    saved_at: chrono::Utc::now().to_rfc3339(),
                })?;
            }
        }

        // Did we exceed max_iterations or terminate without reaching a terminal node?
        let should_terminate = state.should_terminate;
        if iterations >= self.max_iterations {
            tracing::warn!(
                "StateGraph '{}' exceeded max iterations ({}). Terminating.",
                graph.name,
                self.max_iterations
            );
        }

        let result = GraphExecutionResult {
            name: graph.name.clone(),
            final_state: state,
            completed: !should_terminate,
            terminal_node: None,
            iterations,
            interrupt_checkpoints,
        };

        // Checkpoint finalization: completed walks drop their checkpoint;
        // interrupted/budget-exhausted walks persist the final state so a
        // later `resume` sees how far the walk got.
        if let Some((run_id, store)) = &checkpoint {
            if result.completed {
                if let Err(e) = store.delete(run_id) {
                    tracing::warn!("failed to delete completed checkpoint '{}': {}", run_id, e);
                }
            } else {
                store.save(&crate::checkpoint::GraphCheckpoint {
                    run_id: run_id.clone(),
                    graph_name: graph.name.clone(),
                    frontier: frontier.clone(),
                    iterations: result.iterations,
                    state: result.final_state.clone(),
                    interrupt_checkpoints: result.interrupt_checkpoints.clone(),
                    completed: false,
                    saved_at: chrono::Utc::now().to_rfc3339(),
                })?;
            }
        }

        Ok(result)
    }

    /// Execute a frontier sequentially on the shared state.
    ///
    /// Used when the frontier is a single node (the historical single-walker
    /// case — no clone, behaviour identical to the pre-frontier loop) or when
    /// any frontier node carries an interrupt point (approval gating is
    /// conservative under sequential execution; fork-with-interrupt is an edge
    /// case we don't parallelize). Returns the union of each node's routed
    /// next-targets. Sets `should_terminate`/`last_result`/`last_error` on the
    /// shared state exactly as the historical loop did.
    async fn execute_frontier_sequential(
        &self,
        graph: &StateGraph,
        frontier: &BTreeSet<String>,
        state: &mut GraphState,
        interrupt_checkpoints: &mut Vec<String>,
    ) -> Result<Vec<String>> {
        let mut next = Vec::new();
        for node_id in frontier {
            let node = graph.get_node(node_id).ok_or_else(|| {
                OneAIError::Workflow(format!(
                    "Node '{}' not found in graph '{}'",
                    node_id, graph.name
                ))
            })?;

            // Interrupt point — request human approval (same shape as the
            // historical loop, run in frontier-node order).
            if node.interrupt {
                let checkpoint_id = format!("interrupt_{}_{}", graph.name, node_id);
                interrupt_checkpoints.push(checkpoint_id.clone());
                let approval_request = oneai_core::ApprovalRequest {
                    tool_name: "state_graph_interrupt".into(),
                    args: serde_json::json!({
                        "node": node_id,
                        "description": match &node.action {
                            NodeAction::HumanApproval { description } => description.clone(),
                            _ => "Interrupt point reached".to_string(),
                        },
                        "state": state.variables,
                    }),
                    risk_level: oneai_core::RiskLevel::Medium,
                    permission_level: Some(oneai_core::PermissionLevel::Standard),
                    justification: format!(
                        "StateGraph interrupt at node '{}' in graph '{}'",
                        node_id, graph.name
                    ),
                };
                let approval = self
                    .interaction_gate
                    .request(oneai_core::InteractionRequest::ToolApproval {
                        approval: approval_request,
                    })
                    .await?;
                match approval {
                    oneai_core::InteractionResponse::Abort { reason } => {
                        state.should_terminate = true;
                        state.last_error = Some(format!("Interrupt denied: {}", reason));
                        return Ok(next);
                    }
                    oneai_core::InteractionResponse::Revise { feedback } => {
                        state.should_terminate = true;
                        state.last_error = Some(format!("Interrupt denied: {}", feedback));
                        return Ok(next);
                    }
                    _ => { /* Proceed / ProceedWith / Choose — continue */ }
                }
            }

            let action_result = self.execute_node_action(&node.action, state).await?;
            state.last_result = Some(action_result.output.clone());
            state.last_error = action_result.error.clone();
            if state.should_terminate {
                return Ok(next);
            }

            let edges = graph.get_edges_from(node_id);
            next.extend(self.route_next_nodes(&edges, state)?);
        }
        Ok(next)
    }

    /// Execute a parallel fork frontier concurrently on isolated state clones.
    ///
    /// Precondition: `frontier.len() >= 2` and **no** node has `interrupt`
    /// (the caller guarantees this). Each branch runs `execute_node_action` on
    /// a clone of the shared state (so concurrent branches never race on
    /// shared mutable state — the MVI/Redux isolation pattern), then routes its
    /// own outgoing edges against its own clone (consuming its own
    /// `parsed_decision` — no cross-branch decision leakage). Results are
    /// merged back deterministically by node-ID (BTreeSet) order.
    async fn execute_frontier_parallel(
        &self,
        graph: &StateGraph,
        frontier: &BTreeSet<String>,
        state: &mut GraphState,
    ) -> Result<Vec<String>> {
        // Snapshot the incoming conversation length so each branch's appended
        // messages can be sliced out for deterministic merge.
        let baseline_msgs = state.conversation.messages.len();
        // Owned snapshot of the incoming state. Each branch clones THIS (via a
        // shared immutable borrow) so no branch captures `&mut state` — the
        // concurrent futures only ever share `&self`, `&graph`, and `&incoming`.
        let incoming = state.clone();

        // Build one future per frontier node. Each future borrows `&self`
        // (immutable — execute_node_action/route_next_nodes take &self),
        // `&graph`, and `&incoming` (shared), and owns its state clone, so the
        // futures are independent and safe to poll concurrently. join_all
        // interleaves them on this task: LLM-await branches yield and let
        // siblings progress — real concurrency for the in-flight provider
        // calls without needing Send+'static spawns.
        let branch_futs: Vec<_> = frontier
            .iter()
            .map(|node_id| {
                // Each branch gets its own owned starting state cloned from the
                // shared snapshot. (`&self`/`&graph`/`node_id` are shared refs —
                // Copy — so `async move` can capture them by-move harmlessly;
                // `seed` is the only owned value moved in.)
                let seed = incoming.clone();
                async move {
                    let node = match graph.get_node(node_id) {
                        Some(n) => n,
                        None => {
                            return BranchResult {
                                node_id: node_id.clone(),
                                action_result: ActionResult {
                                    output: String::new(),
                                    error: Some(format!(
                                        "Node '{}' not found in graph '{}'",
                                        node_id, graph.name
                                    )),
                                },
                                branch_state: seed,
                                next_targets: Vec::new(),
                            };
                        }
                    };
                    let mut branch_state = seed;
                    let action_result = match self
                        .execute_node_action(&node.action, &mut branch_state)
                        .await
                    {
                        Ok(ar) => ar,
                        Err(e) => ActionResult {
                            output: String::new(),
                            error: Some(e.to_string()),
                        },
                    };
                    let edges = graph.get_edges_from(node_id);
                    // Routing uses this branch's own parsed_decision (set by the
                    // action it just ran). Consumed locally — never leaks to siblings.
                    let next_targets = self
                        .route_next_nodes(&edges, &branch_state)
                        .unwrap_or_default();
                    BranchResult {
                        node_id: node_id.clone(),
                        action_result,
                        branch_state,
                        next_targets,
                    }
                }
            })
            .collect();

        let mut branches: Vec<BranchResult> = join_all(branch_futs).await;
        // join_all preserves submission order (frontier's BTreeSet order), but
        // sort explicitly by node_id so the merge below is deterministically
        // ordered regardless of which branch's provider responded first — a
        // hard invariant (戒律 #6), not a coincidence of completion order.
        branches.sort_by(|a, b| a.node_id.cmp(&b.node_id));

        // Deterministic merge by node-ID order (branches came from a BTreeSet,
        // so iteration order is already deterministic; the vec preserves it).
        let mut joined_output = Vec::new();
        let mut first_error: Option<String> = None;
        let mut any_terminate = false;
        for b in &branches {
            joined_output.push(b.action_result.output.clone());
            if first_error.is_none() {
                first_error = b.action_result.error.clone();
            }
            any_terminate |= b.branch_state.should_terminate;
        }
        state.last_result = Some(joined_output.join("\n---\n"));
        state.last_error = first_error;
        state.should_terminate = any_terminate;

        // Conversation: append each branch's appended messages (those beyond
        // the shared baseline) in node-ID order — deterministic regardless of
        // which branch's provider responded first.
        for b in &branches {
            let extra_from = baseline_msgs.min(b.branch_state.conversation.messages.len());
            for m in &b.branch_state.conversation.messages[extra_from..] {
                state.conversation.add_message(m.clone());
            }
        }
        // Variables: union; conflict resolved by node-ID order (last wins) —
        // deterministic. Baseline values are rewritten with identical values
        // by earlier branches, a harmless no-op.
        for b in &branches {
            for (k, v) in &b.branch_state.variables {
                state.variables.insert(k.clone(), v.clone());
            }
        }
        // parsed_decision: take the last branch's (node-ID order) for
        // debugging/observability only. Edge routing already consumed each
        // branch's own decision above; the next frontier is computed from
        // those per-branch results, not from this shared field.
        if let Some(last) = branches.last() {
            state.parsed_decision = last.branch_state.parsed_decision.clone();
        }

        let mut next = Vec::new();
        for b in &branches {
            next.extend(b.next_targets.iter().cloned());
        }
        Ok(next)
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

            NodeAction::ToolCall {
                tool_name,
                args_template,
            } => {
                // Template interpolation: {{variable}} → state.variables[key]
                let resolved_name = interpolate_graph_template(tool_name, &state.variables);
                let resolved_args = if let Some(template) = args_template {
                    let json_str = interpolate_graph_template(template, &state.variables);
                    serde_json::from_str(&json_str).unwrap_or(serde_json::json!({}))
                } else {
                    serde_json::json!({})
                };

                // Delegate to GraphActionExecutor — which may run hooks,
                // check permissions, interact with approval gate.
                self.action_executor
                    .execute_tool_call(&resolved_name, &resolved_args, state)
                    .await
            }

            NodeAction::Delegate {
                agent_kind,
                task_template,
            } => {
                let task = interpolate_graph_template(task_template, &state.variables);

                let result = self.delegate_factory.delegate(agent_kind, &task).await?;

                // Append delegate result to conversation
                state.conversation.add_message(Message::assistant(format!(
                    "[Delegate {}]: {}",
                    agent_kind, result
                )));

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
                state
                    .variables
                    .insert("_condition_result".to_string(), result.to_string());
                Ok(ActionResult {
                    output: result.to_string(),
                    error: None,
                })
            }

            NodeAction::SwitchParadigm { paradigm } => {
                // Delegate to GraphActionExecutor — updates state.active_paradigm
                self.action_executor
                    .execute_paradigm_switch(paradigm, state)
                    .await
            }
        }
    }

    /// Route to the next node(s) by evaluating outgoing edge conditions.
    ///
    /// Returns the targets of **all** satisfiable outgoing edges (conditional
    /// edges whose condition holds, plus every unconditional `Always` edge).
    /// For the historical single-branch case — mutually-exclusive conditional
    /// edges (HasToolCalls vs IsFinalAnswer) where only one holds — this yields
    /// 0 or 1 targets, identical to the old "first match" behaviour. When a
    /// node fans out multiple `Always` edges or multiple simultaneously-true
    /// conditions, all targets are returned, and the caller runs them as a
    /// parallel frontier (fork). Conditional edges are still evaluated in
    /// declared order; `Always` edges act as unconditional fan-out.
    fn route_next_nodes(&self, edges: &[&GraphEdge], state: &GraphState) -> Result<Vec<String>> {
        let mut next = Vec::with_capacity(edges.len());
        for edge in edges {
            let satisfied = match &edge.condition {
                Some(condition) => self.evaluate_edge_condition(condition, state)?,
                None => true, // unconditional edge
            };
            if satisfied {
                next.push(edge.to.clone());
            }
        }
        Ok(next)
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
                Ok(state
                    .parsed_decision
                    .as_ref()
                    .map(|d| d.has_tool_calls())
                    .unwrap_or(false))
            }

            EdgeCondition::IsFinalAnswer => {
                // P2-2: Use parsed_decision instead of !HasToolCalls
                Ok(state
                    .parsed_decision
                    .as_ref()
                    .map(|d| d.is_final())
                    .unwrap_or(false))
            }

            EdgeCondition::RequestsDelegation => {
                // P2-2: Use parsed_decision
                Ok(state
                    .parsed_decision
                    .as_ref()
                    .map(|d| d.is_delegation())
                    .unwrap_or(false))
            }

            EdgeCondition::ErrorOccurred => Ok(state.last_error.is_some()),

            EdgeCondition::StateEquals { variable, value } => {
                Ok(state.variables.get(variable) == Some(value))
            }

            EdgeCondition::Always => Ok(true),

            EdgeCondition::Custom { name, description } => Ok(evaluate_custom_condition(
                &self.custom_conditions,
                name,
                description,
                state,
            )),

            EdgeCondition::ParadigmEquals { paradigm } => {
                Ok(state.active_paradigm.as_ref() == Some(paradigm))
            }

            EdgeCondition::IterationExceeds { count } => Ok(state.iteration_count > *count),
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
        Ok(state
            .variables
            .get(condition)
            .map(|v| v == "true")
            .unwrap_or(false))
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
    use crate::checkpoint::GraphCheckpointStore;
    use crate::state_graph::{GraphNode, StateGraph};

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

        let result = executor
            .evaluate_edge_condition(&EdgeCondition::Always, &state)
            .unwrap();
        assert!(result);
    }

    #[test]
    fn test_evaluate_edge_condition_error_occurred() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // No error → false
        assert!(!executor
            .evaluate_edge_condition(&EdgeCondition::ErrorOccurred, &state)
            .unwrap());

        // With error → true
        state.last_error = Some("test error".to_string());
        assert!(executor
            .evaluate_edge_condition(&EdgeCondition::ErrorOccurred, &state)
            .unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_state_equals() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // Variable not set → false
        let cond = EdgeCondition::StateEquals {
            variable: "mode".to_string(),
            value: "react".to_string(),
        };
        assert!(!executor.evaluate_edge_condition(&cond, &state).unwrap());

        // Variable set but different value → false
        state
            .variables
            .insert("mode".to_string(), "plan".to_string());
        assert!(!executor.evaluate_edge_condition(&cond, &state).unwrap());

        // Variable matches → true
        state
            .variables
            .insert("mode".to_string(), "react".to_string());
        assert!(executor.evaluate_edge_condition(&cond, &state).unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_has_tool_calls() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // No parsed_decision → false
        assert!(!executor
            .evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state)
            .unwrap());

        // DirectAnswer → false
        state.parsed_decision = Some(oneai_core::GraphDecision::DirectAnswer {
            text: "Just a text answer".to_string(),
        });
        assert!(!executor
            .evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state)
            .unwrap());

        // ToolCalls → true
        state.parsed_decision = Some(oneai_core::GraphDecision::ToolCalls { count: 1 });
        assert!(executor
            .evaluate_edge_condition(&EdgeCondition::HasToolCalls, &state)
            .unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_is_final_answer() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // No parsed_decision → false
        assert!(!executor
            .evaluate_edge_condition(&EdgeCondition::IsFinalAnswer, &state)
            .unwrap());

        // DirectAnswer → true
        state.parsed_decision = Some(oneai_core::GraphDecision::DirectAnswer {
            text: "The answer is 42".to_string(),
        });
        assert!(executor
            .evaluate_edge_condition(&EdgeCondition::IsFinalAnswer, &state)
            .unwrap());

        // ToolCalls → false
        state.parsed_decision = Some(oneai_core::GraphDecision::ToolCalls { count: 2 });
        assert!(!executor
            .evaluate_edge_condition(&EdgeCondition::IsFinalAnswer, &state)
            .unwrap());
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
        assert!(executor
            .evaluate_edge_condition(&EdgeCondition::RequestsDelegation, &state)
            .unwrap());

        // ToolCalls → false
        state.parsed_decision = Some(oneai_core::GraphDecision::ToolCalls { count: 1 });
        assert!(!executor
            .evaluate_edge_condition(&EdgeCondition::RequestsDelegation, &state)
            .unwrap());
    }

    #[test]
    fn test_evaluate_edge_condition_paradigm_equals() {
        let mut state = GraphState::new();
        let executor = make_test_executor();

        // No paradigm → false
        let cond = EdgeCondition::ParadigmEquals {
            paradigm: "react".to_string(),
        };
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
        state
            .variables
            .insert("mode".to_string(), "react".to_string());
        assert!(executor
            .evaluate_condition_expression("mode==react", &state)
            .unwrap());
        assert!(!executor
            .evaluate_condition_expression("mode==plan", &state)
            .unwrap());

        // "error_occurred" pattern
        state.last_error = Some("error".to_string());
        assert!(executor
            .evaluate_condition_expression("error_occurred", &state)
            .unwrap());
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
        fn evaluate_edge_condition(
            &self,
            condition: &EdgeCondition,
            state: &GraphState,
        ) -> Result<bool> {
            match condition {
                EdgeCondition::HasToolCalls => Ok(state
                    .parsed_decision
                    .as_ref()
                    .map(|d| d.has_tool_calls())
                    .unwrap_or(false)),
                EdgeCondition::IsFinalAnswer => Ok(state
                    .parsed_decision
                    .as_ref()
                    .map(|d| d.is_final())
                    .unwrap_or(false)),
                EdgeCondition::RequestsDelegation => Ok(state
                    .parsed_decision
                    .as_ref()
                    .map(|d| d.is_delegation())
                    .unwrap_or(false)),
                EdgeCondition::ErrorOccurred => Ok(state.last_error.is_some()),
                EdgeCondition::StateEquals { variable, value } => {
                    Ok(state.variables.get(variable) == Some(value))
                }
                EdgeCondition::Always => Ok(true),
                EdgeCondition::Custom { name, .. } => {
                    tracing::warn!(
                        "Custom condition '{}' not registered, defaulting to false",
                        name
                    );
                    Ok(false)
                }
                EdgeCondition::ParadigmEquals { paradigm } => {
                    Ok(state.active_paradigm.as_ref() == Some(paradigm))
                }
                EdgeCondition::IterationExceeds { count } => Ok(state.iteration_count > *count),
            }
        }

        fn evaluate_condition_expression(
            &self,
            condition: &str,
            state: &GraphState,
        ) -> Result<bool> {
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
            Ok(state
                .variables
                .get(condition)
                .map(|v| v == "true")
                .unwrap_or(false))
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

    #[test]
    fn test_custom_condition_registry_lookup() {
        use std::sync::Arc;
        let mut registry: HashMap<String, CustomConditionFn> = HashMap::new();
        let mut state = GraphState::new();
        state
            .variables
            .insert("attempts".to_string(), "3".to_string());

        // Unregistered condition → fail-closed (false), no panic.
        assert!(!evaluate_custom_condition(
            &registry,
            "not_registered",
            "missing",
            &state
        ));

        // Register a condition that inspects a state variable.
        registry.insert(
            "attempts_exhausted".to_string(),
            Arc::new(|s: &GraphState| {
                s.variables
                    .get("attempts")
                    .map(|v| v == "3")
                    .unwrap_or(false)
            }),
        );
        assert!(evaluate_custom_condition(
            &registry,
            "attempts_exhausted",
            "tries >= 3",
            &state
        ));

        // Same condition, different state → false.
        let other_state = GraphState::new();
        assert!(!evaluate_custom_condition(
            &registry,
            "attempts_exhausted",
            "tries >= 3",
            &other_state
        ));
    }

    // ─── Frontier parallel-execution tests ─────────────────────────────────
    //
    // These exercise the `execute()` frontier loop (B2). Delegate nodes are
    // ideal fixtures: they route through `delegate_factory` (NOT the
    // `GraphActionExecutor`), so a fork of Delegate nodes needs only a trivial
    // mock DelegateFactory + a null action executor (purely a construction
    // requirement — never called for Delegate/ConditionCheck nodes).

    use async_trait::async_trait;
    use oneai_core::InferenceResponse;

    /// A no-op `GraphActionExecutor` for tests that only exercise Delegate /
    /// ConditionCheck / HumanApproval nodes (which don't touch the action
    /// executor). Its methods are never called; they return an error if they
    /// ever are, failing the test loudly rather than silently.
    struct NullGraphActionExecutor;

    #[async_trait]
    impl GraphActionExecutor for NullGraphActionExecutor {
        async fn execute_llm_infer(
            &self,
            _action: &NodeAction,
            _state: &mut GraphState,
        ) -> Result<ActionResult> {
            Err(OneAIError::Workflow(
                "NullGraphActionExecutor::execute_llm_infer \
                called — this test fixture only supports Delegate/ConditionCheck nodes"
                    .into(),
            ))
        }
        async fn execute_tool_call(
            &self,
            _tool_name: &str,
            _args: &serde_json::Value,
            _state: &mut GraphState,
        ) -> Result<ActionResult> {
            Err(OneAIError::Workflow(
                "NullGraphActionExecutor::execute_tool_call called".into(),
            ))
        }
        async fn execute_paradigm_switch(
            &self,
            _paradigm: &str,
            state: &mut GraphState,
        ) -> Result<ActionResult> {
            // Mirrors the direct executor's no-op behaviour so a SwitchParadigm
            // node could still be exercised if a future test wants it.
            state.active_paradigm = Some(_paradigm.to_string());
            state.parsed_decision = None;
            Ok(ActionResult {
                output: format!("Paradigm switched to: {}", _paradigm),
                error: None,
            })
        }
        async fn parse_decision(
            &self,
            _response: &InferenceResponse,
            _state: &mut GraphState,
        ) -> Result<oneai_core::GraphDecision> {
            Ok(oneai_core::GraphDecision::DirectAnswer {
                text: String::new(),
            })
        }
    }

    /// A `DelegateFactory` that returns a deterministic result per agent_kind,
    /// so each Delegate branch appends a distinct `[Delegate <kind>: result:<kind>]`
    /// message to the conversation — letting fork/diamond tests assert which
    /// branches ran and in what merge order.
    struct RecordingDelegateFactory;

    #[async_trait]
    impl DelegateFactory for RecordingDelegateFactory {
        async fn delegate(&self, agent_kind: &str, _task: &str) -> Result<String> {
            Ok(format!("result:{agent_kind}"))
        }
    }

    /// A noop `InteractionGate` for tests that exercise no interrupt nodes.
    /// `StubPlatformInteractionGate` (oneai-core) always proceeds and is
    /// `enabled()==false` — zero-latency, no UI.
    fn make_frontier_executor() -> StateGraphExecutor {
        use oneai_core::platform::StubPlatformInteractionGate;
        StateGraphExecutor::new(
            Arc::new(NullGraphActionExecutor),
            Arc::new(RecordingDelegateFactory),
            Arc::new(StubPlatformInteractionGate::macos()) as Arc<dyn InteractionGate>,
            50,
        )
    }

    /// Build a Delegate node with two unconditional (`Always`) edges forking
    /// to `a` and `b`, then both converging on a terminal `join`.
    fn make_fork_graph(
        entry_kind: &str,
        a_kind: &str,
        b_kind: &str,
        join_kind: &str,
    ) -> StateGraph {
        let mut graph = StateGraph::new("fork", "entry");
        graph.add_node(GraphNode {
            id: "entry".to_string(),
            action: NodeAction::Delegate {
                agent_kind: entry_kind.to_string(),
                task_template: "t".to_string(),
            },
            interrupt: false,
            metadata: HashMap::new(),
        });
        graph.add_node(GraphNode {
            id: "a".to_string(),
            action: NodeAction::Delegate {
                agent_kind: a_kind.to_string(),
                task_template: "t".to_string(),
            },
            interrupt: false,
            metadata: HashMap::new(),
        });
        graph.add_node(GraphNode {
            id: "b".to_string(),
            action: NodeAction::Delegate {
                agent_kind: b_kind.to_string(),
                task_template: "t".to_string(),
            },
            interrupt: false,
            metadata: HashMap::new(),
        });
        graph.add_node(GraphNode {
            id: "join".to_string(),
            action: NodeAction::Delegate {
                agent_kind: join_kind.to_string(),
                task_template: "t".to_string(),
            },
            interrupt: false,
            metadata: HashMap::new(),
        });
        // entry forks to both a and b (two Always edges → route_next_nodes
        // returns both → frontier {a, b}).
        graph.add_edge(GraphEdge {
            from: "entry".to_string(),
            to: "a".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });
        graph.add_edge(GraphEdge {
            from: "entry".to_string(),
            to: "b".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });
        // Both branches converge on `join`.
        graph.add_edge(GraphEdge {
            from: "a".to_string(),
            to: "join".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });
        graph.add_edge(GraphEdge {
            from: "b".to_string(),
            to: "join".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });
        graph.add_terminal("join");
        graph
    }

    /// Helper: the `[Delegate <kind>: result:<kind>]` message text a branch
    /// appends (mirrors `execute_node_action`'s Delegate arm formatting).
    fn delegate_msg(kind: &str) -> String {
        format!("[Delegate {}]: result:{}", kind, kind)
    }

    #[tokio::test]
    async fn frontier_fork_runs_both_branches_and_merges() {
        // entry(S) → {a, b} → join(J). Both branches must run; their messages
        // must both reach the merged conversation, in deterministic node-ID
        // order (a before b), regardless of which branch's delegate resolved
        // first.
        let graph = make_fork_graph("S", "A", "B", "J");
        let executor = make_frontier_executor();
        let result = executor.execute(&graph, GraphState::new()).await.unwrap();

        assert!(result.completed, "should reach terminal join");
        assert_eq!(result.terminal_node.as_deref(), Some("join"));
        // 4 delegate messages appended: S, A, B, J.
        let texts: Vec<String> = result
            .final_state
            .conversation
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect();
        assert_eq!(
            texts.len(),
            4,
            "expected 4 delegate messages, got {texts:?}"
        );
        assert_eq!(texts[0], delegate_msg("S"));
        assert_eq!(texts[1], delegate_msg("A"));
        assert_eq!(texts[2], delegate_msg("B"));
        assert_eq!(texts[3], delegate_msg("J"));
    }

    #[tokio::test]
    async fn frontier_join_runs_only_after_both_branches_complete() {
        // Same fork graph, but verify `join` runs exactly once (not twice) —
        // i.e. the natural-join dedup in `next_frontier` collapsed a+ b→join
        // into a single frontier entry, and the barrier (join_all) ensured
        // both a and b finished before join executed.
        let graph = make_fork_graph("S", "A", "B", "J");
        let executor = make_frontier_executor();
        let result = executor.execute(&graph, GraphState::new()).await.unwrap();

        // join's message appears exactly once.
        let join_count = result
            .final_state
            .conversation
            .messages
            .iter()
            .filter(|m| m.text_content() == delegate_msg("J"))
            .count();
        assert_eq!(
            join_count, 1,
            "join must run exactly once (natural-join dedup)"
        );
        // iterations: entry(1) + fork{a,b}(1) + join(1) = 3.
        assert_eq!(result.iterations, 3);
    }

    #[tokio::test]
    async fn frontier_single_branch_unchanged_from_legacy_behaviour() {
        // A linear single-branch graph (entry → join, one Always edge) must
        // behave exactly as the pre-frontier single-walker loop: frontier
        // stays size 1, sequential path, 2 iterations, terminal reached.
        // This is the regression guard (戒律 #7).
        let mut graph = StateGraph::new("linear", "entry");
        graph.add_node(GraphNode {
            id: "entry".to_string(),
            action: NodeAction::Delegate {
                agent_kind: "S".to_string(),
                task_template: "t".to_string(),
            },
            interrupt: false,
            metadata: HashMap::new(),
        });
        graph.add_node(GraphNode {
            id: "join".to_string(),
            action: NodeAction::Delegate {
                agent_kind: "J".to_string(),
                task_template: "t".to_string(),
            },
            interrupt: false,
            metadata: HashMap::new(),
        });
        graph.add_edge(GraphEdge {
            from: "entry".to_string(),
            to: "join".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });
        graph.add_terminal("join");

        let executor = make_frontier_executor();
        let result = executor.execute(&graph, GraphState::new()).await.unwrap();
        assert!(result.completed);
        assert_eq!(result.terminal_node.as_deref(), Some("join"));
        assert_eq!(result.iterations, 2);
        let texts: Vec<String> = result
            .final_state
            .conversation
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect();
        assert_eq!(texts, vec![delegate_msg("S"), delegate_msg("J")]);
    }

    // ─── Checkpoint + resume (gap P2 #14) ───────────────────────────────

    /// Interaction gate with a scripted ToolApproval answer — lets tests
    /// interrupt (Abort) or pass (Proceed) a StateGraph interrupt node.
    struct ScriptedApprovalGate {
        approve: bool,
    }

    #[async_trait::async_trait]
    impl InteractionGate for ScriptedApprovalGate {
        async fn request(
            &self,
            _req: oneai_core::InteractionRequest,
        ) -> Result<oneai_core::InteractionResponse> {
            if self.approve {
                Ok(oneai_core::InteractionResponse::Proceed)
            } else {
                Ok(oneai_core::InteractionResponse::Abort {
                    reason: "denied for test".to_string(),
                })
            }
        }

        fn enabled(&self, _point: oneai_core::InteractionPoint) -> bool {
            true
        }
    }

    fn make_executor_with_gate(
        gate: Arc<dyn InteractionGate>,
        max_iterations: usize,
    ) -> StateGraphExecutor {
        StateGraphExecutor::new(
            Arc::new(NullGraphActionExecutor),
            Arc::new(RecordingDelegateFactory),
            gate,
            max_iterations,
        )
    }

    /// entry(S) → gate(G, interrupt) → join(J, terminal).
    fn make_interrupt_graph() -> StateGraph {
        let mut graph = StateGraph::new("interrupt-graph", "entry");
        for (id, kind, interrupt) in [
            ("entry", "S", false),
            ("gate", "G", true),
            ("join", "J", false),
        ] {
            graph.add_node(GraphNode {
                id: id.to_string(),
                action: NodeAction::Delegate {
                    agent_kind: kind.to_string(),
                    task_template: "t".to_string(),
                },
                interrupt,
                metadata: HashMap::new(),
            });
        }
        graph.add_edge(GraphEdge {
            from: "entry".to_string(),
            to: "gate".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });
        graph.add_edge(GraphEdge {
            from: "gate".to_string(),
            to: "join".to_string(),
            condition: Some(EdgeCondition::Always),
            metadata: HashMap::new(),
        });
        graph.add_terminal("join");
        graph
    }

    #[tokio::test]
    async fn interrupted_walk_persists_checkpoint_and_resumes_to_completion() {
        // Durable execution (gap P2 #14): an interrupt denial stops the walk
        // mid-graph; the walk state is checkpointed; a later resume (approve
        // gate) continues from the saved frontier and reaches the terminal.
        let store = Arc::new(crate::checkpoint::InMemoryCheckpointStore::new());
        let graph = make_interrupt_graph();

        // Run 1 — interrupt denied → walk stops at the gate node.
        let exec_abort =
            make_executor_with_gate(Arc::new(ScriptedApprovalGate { approve: false }), 50);
        let result = exec_abort
            .execute_with_checkpoints(&graph, GraphState::new(), "run-1", store.clone())
            .await
            .unwrap();
        assert!(!result.completed, "aborted interrupt must not complete");
        assert_eq!(result.iterations, 2, "entry + gate(interrupt) ran");
        // Only entry's message landed — the gate node never executed.
        let texts: Vec<String> = result
            .final_state
            .conversation
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect();
        assert_eq!(texts, vec![delegate_msg("S")]);
        // Checkpoint persisted (walk incomplete).
        assert_eq!(store.len(), 1);
        let cp = store.load("run-1").unwrap().expect("checkpoint saved");
        assert_eq!(cp.graph_name, "interrupt-graph");
        assert!(cp.frontier.contains("gate"));

        // Run 2 — resume with an approving gate (a "later process").
        let exec_ok = make_executor_with_gate(Arc::new(ScriptedApprovalGate { approve: true }), 50);
        let resumed = exec_ok
            .resume(&graph, "run-1", store.clone())
            .await
            .unwrap();
        assert!(resumed.completed);
        assert_eq!(resumed.terminal_node.as_deref(), Some("join"));
        // entry's message survived the restart; gate + join ran on resume —
        // each exactly once (no re-execution of the already-done entry).
        let texts: Vec<String> = resumed
            .final_state
            .conversation
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect();
        assert_eq!(
            texts,
            vec![delegate_msg("S"), delegate_msg("G"), delegate_msg("J")]
        );
        // Completed run deletes its checkpoint.
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn checkpoint_survives_store_instance_restart() {
        // FileCheckpointStore simulates a process restart: the checkpoint
        // written by one store instance is resumed through a fresh one.
        let dir =
            std::env::temp_dir().join(format!("oneai-graph-resume-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let graph = make_interrupt_graph();

        {
            let store = Arc::new(crate::checkpoint::FileCheckpointStore::new(&dir).unwrap());
            let exec_abort =
                make_executor_with_gate(Arc::new(ScriptedApprovalGate { approve: false }), 50);
            let result = exec_abort
                .execute_with_checkpoints(&graph, GraphState::new(), "run-file", store)
                .await
                .unwrap();
            assert!(!result.completed);
        }

        // "Restart": a brand-new store instance over the same directory.
        let store2 = Arc::new(crate::checkpoint::FileCheckpointStore::new(&dir).unwrap());
        assert!(
            store2.load("run-file").unwrap().is_some(),
            "checkpoint must survive the restart"
        );
        let exec_ok = make_executor_with_gate(Arc::new(ScriptedApprovalGate { approve: true }), 50);
        let resumed = exec_ok
            .resume(&graph, "run-file", store2.clone())
            .await
            .unwrap();
        assert!(resumed.completed);
        assert_eq!(resumed.terminal_node.as_deref(), Some("join"));
        // Completed → checkpoint file removed.
        assert!(store2.load("run-file").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn completed_walk_leaves_no_checkpoint() {
        let store = Arc::new(crate::checkpoint::InMemoryCheckpointStore::new());
        let graph = make_interrupt_graph();
        let exec_ok = make_executor_with_gate(Arc::new(ScriptedApprovalGate { approve: true }), 50);
        let result = exec_ok
            .execute_with_checkpoints(&graph, GraphState::new(), "run-c", store.clone())
            .await
            .unwrap();
        assert!(result.completed);
        assert!(store.is_empty(), "completed runs delete their checkpoint");
    }

    #[tokio::test]
    async fn resume_errors_on_missing_checkpoint_or_graph_mismatch() {
        let store = Arc::new(crate::checkpoint::InMemoryCheckpointStore::new());
        let graph = make_interrupt_graph();
        let exec_ok = make_executor_with_gate(Arc::new(ScriptedApprovalGate { approve: true }), 50);

        // No checkpoint at all.
        let err = exec_ok
            .resume(&graph, "never-ran", store.clone())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No checkpoint"));

        // Checkpoint for one graph, resumed against another.
        let mut other = StateGraph::new("other-graph", "entry");
        other.add_node(GraphNode {
            id: "entry".to_string(),
            action: NodeAction::Delegate {
                agent_kind: "X".to_string(),
                task_template: "t".to_string(),
            },
            interrupt: false,
            metadata: HashMap::new(),
        });
        other.add_terminal("entry");
        let exec_abort =
            make_executor_with_gate(Arc::new(ScriptedApprovalGate { approve: false }), 50);
        exec_abort
            .execute_with_checkpoints(&graph, GraphState::new(), "run-m", store.clone())
            .await
            .unwrap();
        let err = exec_ok
            .resume(&other, "run-m", store.clone())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("belongs to graph"));
    }
}
