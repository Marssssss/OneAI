//! StateGraph — directed graph that supports cyclic edges for agent workflows.
//!
//! Unlike the existing `WorkflowDag` which is a DAG (no cycles),
//! `StateGraph` supports cyclic edges, making it suitable for modeling
//! ReAct loops and other iterative agent patterns as explicit graph structures.
//!
//! This is inspired by LangGraph's core innovation: cyclic graphs enable
//! ReAct loops (Think → Act → Observe → Think) as explicit cycles
//! rather than implicit while loops. This makes state visible, inspectable,
//! and interruptable.
//!
//! Key differences from WorkflowDag:
//! - Supports conditional edges (edges with conditions that determine routing)
//! - Supports cyclic edges (edges that form loops)
//! - Supports interrupt points (nodes where execution can be paused)
//! - State is explicitly passed through nodes rather than accumulated
//!
//! The WorkflowDag is retained for pure declaration-style DAG workflows
//! (parallel step orchestration). StateGraph is used for agent flows
//! that need iteration and dynamic routing.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use oneai_core::error::Result;

// ─── NodeAction ─────────────────────────────────────────────────────────────

/// The action performed by a graph node.
///
/// Each node can perform one of several action types:
/// - LLM inference (the core reasoning step)
/// - Tool execution (calling a specific tool)
/// - Sub-agent delegation (spawning a specialized sub-agent)
/// - Human approval (a checkpoint requiring human intervention)
/// - Condition check (a routing node that evaluates a condition)
/// - Paradigm switch (changing the active paradigm for subsequent nodes)
///
/// When a `GraphActionExecutor` (from `oneai-agent`) is configured,
/// the LlmInfer and ToolCall nodes delegate to the AgentLoop's full
/// inference and execution pipeline (context assembly, tool definitions,
/// hooks, permission, domain pack). This is the P2-2 "闭环" mechanism.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeAction {
    /// LLM inference node — sends conversation to the model and gets a response.
    ///
    /// When `include_tool_definitions` is true, the GraphActionExecutor will
    /// build tool definitions for this node (filtered by `tool_filter_override`
    /// or the active paradigm's tool set). This is critical for ReAct loops —
    /// the model needs to see available tools to decide whether to call them.
    ///
    /// When `include_tool_definitions` is false, the node sends a pure inference
    /// request without tool definitions (for final answer nodes, condition checks, etc).
    LlmInfer {
        /// System prompt override for this node (if any).
        system_prompt_override: Option<String>,
        /// Whether to use streaming inference.
        #[serde(default)]
        use_streaming: bool,
        /// Whether to include tool definitions in the inference request.
        /// When true, the GraphActionExecutor builds tool definitions based on
        /// `tool_filter_override` or the active paradigm config.
        /// When false (default), the LLM receives a pure text prompt without tools.
        #[serde(default)]
        include_tool_definitions: bool,
        /// Override the tool filter for this node (if any).
        /// When Some, only these tools are included in the inference request.
        /// When None, the active paradigm's tool_filter is used (or all tools
        /// if no paradigm is active).
        #[serde(default)]
        tool_filter_override: Option<Vec<String>>,
        /// Token budget for extended thinking/reasoning (Anthropic budget_tokens).
        /// None = thinking disabled; Some(N) = enable thinking with N token budget.
        #[serde(default)]
        thinking_budget: Option<u32>,
        /// Temperature for sampling (0.0 = deterministic, 1.0 = creative).
        #[serde(default)]
        temperature: Option<f32>,
        /// Maximum tokens to generate.
        #[serde(default)]
        max_tokens: Option<u32>,
    },

    /// Tool execution node — calls a specific tool with arguments.
    ToolCall {
        /// The tool name to call.
        tool_name: String,
        /// Template for constructing tool arguments from state.
        /// Uses {{variable}} syntax for state variable interpolation.
        args_template: Option<String>,
    },

    /// Sub-agent delegation node — spawns a specialized sub-agent.
    Delegate {
        /// The sub-agent kind to spawn.
        agent_kind: String,
        /// Template for constructing the task description from state.
        task_template: String,
    },

    /// Human approval node — pauses execution for human review.
    /// This is an interrupt point where the user can inspect state
    /// and decide to continue, modify, or abort.
    HumanApproval {
        /// The description of what requires approval.
        description: String,
    },

    /// Condition check node — evaluates a condition and routes to different edges.
    /// This is the mechanism for dynamic routing in the graph.
    ConditionCheck {
        /// The condition expression to evaluate.
        /// Examples: "has_tool_calls", "is_final_answer", "error_occurred"
        condition: String,
    },

    /// Paradigm switch node — changes the active paradigm for subsequent nodes.
    ///
    /// When executed, this node updates `GraphState.active_paradigm` and
    /// clears `GraphState.parsed_decision`. Subsequent LlmInfer nodes
    /// (with `include_tool_definitions = true`) will use the new paradigm's
    /// tool set and system prompt. This enables multi-paradigm workflows
    /// as explicit graph paths (e.g., Plan → ReAct → Reflect).
    ///
    /// The paradigm string maps to ParadigmKind in `oneai-agent`:
    /// "plan", "react", "reflect", "explore".
    SwitchParadigm {
        /// The target paradigm to switch to.
        paradigm: String,
    },
}

// ─── EdgeCondition ──────────────────────────────────────────────────────────

/// A condition that determines whether an edge is followed.
///
/// Edges with conditions enable dynamic routing in the graph:
/// - After an LLM inference node, route to tool execution if tool calls present
/// - After a tool execution node, route back to LLM inference (ReAct loop)
/// - After an LLM inference node, route to end if no tool calls (final answer)
///
/// P2-2 improvement: HasToolCalls, IsFinalAnswer, and RequestsDelegation
/// now evaluate against `GraphState.parsed_decision` (a structured `GraphDecision`)
/// rather than unreliable string pattern matching. This makes edge routing
/// consistent with the AgentLoop's decision parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EdgeCondition {
    /// Route if the model output contains tool calls.
    /// Now evaluates `state.parsed_decision == GraphDecision::ToolCalls`.
    HasToolCalls,

    /// Route if the model output is a final answer (no tool calls).
    /// Now evaluates `state.parsed_decision == GraphDecision::DirectAnswer`.
    IsFinalAnswer,

    /// Route if the model output requests delegation.
    /// Now evaluates `state.parsed_decision == GraphDecision::Delegate`.
    RequestsDelegation,

    /// Route if an error occurred in the previous node.
    ErrorOccurred,

    /// Route if a specific state variable has a certain value.
    StateEquals {
        variable: String,
        value: String,
    },

    /// Always route (no condition — unconditional edge).
    Always,

    /// Custom condition — evaluated by a user-provided function.
    Custom {
        name: String,
        description: String,
    },

    /// Route if the active paradigm matches the specified paradigm.
    /// Evaluates `state.active_paradigm == paradigm`.
    ParadigmEquals {
        /// The paradigm to match (e.g., "plan", "react", "reflect", "explore").
        paradigm: String,
    },

    /// Route if the iteration count exceeds the specified threshold.
    /// Useful for adding safety caps to iterative loops.
    IterationExceeds {
        /// The iteration threshold — route is followed when iterations > count.
        count: usize,
    },
}

impl EdgeCondition {
    /// Check if this condition is unconditional (always routes).
    pub fn is_unconditional(&self) -> bool {
        matches!(self, Self::Always)
    }
}

// ─── GraphNode ──────────────────────────────────────────────────────────────

/// A node in the StateGraph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    /// Unique node identifier.
    pub id: String,

    /// The action this node performs.
    pub action: NodeAction,

    /// Whether this node is an interrupt point (execution can pause here).
    /// Interrupt points allow human observation and intervention.
    #[serde(default)]
    pub interrupt: bool,

    /// Metadata for this node.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

// ─── GraphEdge ──────────────────────────────────────────────────────────────

/// An edge in the StateGraph — connects two nodes with optional routing condition.
///
/// Edges can be:
/// - Unconditional: always followed after the source node completes
/// - Conditional: followed only if the condition evaluates to true
/// - Cyclic: the target node can be an ancestor (forming a loop)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    /// The source node ID.
    pub from: String,

    /// The target node ID.
    pub to: String,

    /// The condition for following this edge (None = unconditional).
    #[serde(default)]
    pub condition: Option<EdgeCondition>,

    /// Edge metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

// ─── StateGraph ─────────────────────────────────────────────────────────────

/// A directed graph that supports cyclic edges for agent workflows.
///
/// Unlike DAGs, StateGraphs can contain cycles, enabling ReAct loops
/// and other iterative patterns to be modeled as explicit graph structures.
/// This makes the state machine visible, inspectable, and interruptable.
///
/// Example: ReAct as a StateGraph (diagram):
///
/// think_node (LLMInfer) → [HasToolCalls] → tool_node (ToolCall) → think_node (cycle!)
/// think_node (LLMInfer) → [IsFinalAnswer] → end_node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateGraph {
    /// The graph name/identifier.
    pub name: String,

    /// All nodes in the graph, keyed by node ID.
    pub nodes: HashMap<String, GraphNode>,

    /// All edges in the graph, keyed by source node ID.
    /// Each source node can have multiple outgoing edges (with conditions).
    pub edges: HashMap<String, Vec<GraphEdge>>,

    /// The entry point node ID (where execution starts).
    pub entry_point: String,

    /// Terminal node IDs (where execution ends).
    #[serde(default)]
    pub terminal_nodes: Vec<String>,
}

impl StateGraph {
    /// Create a new StateGraph with a name and entry point.
    pub fn new(name: impl Into<String>, entry_point: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: HashMap::new(),
            edges: HashMap::new(),
            entry_point: entry_point.into(),
            terminal_nodes: Vec::new(),
        }
    }

    /// Add a node to the graph.
    pub fn add_node(&mut self, node: GraphNode) {
        self.nodes.insert(node.id.clone(), node);
    }

    /// Add an edge to the graph.
    pub fn add_edge(&mut self, edge: GraphEdge) {
        self.edges
            .entry(edge.from.clone())
            .or_default()
            .push(edge);
    }

    /// Add a terminal node (execution ends here).
    pub fn add_terminal(&mut self, node_id: impl Into<String>) {
        self.terminal_nodes.push(node_id.into());
    }

    /// Get a node by ID.
    pub fn get_node(&self, id: &str) -> Option<&GraphNode> {
        self.nodes.get(id)
    }

    /// Get outgoing edges from a node.
    pub fn get_edges_from(&self, node_id: &str) -> Vec<&GraphEdge> {
        self.edges.get(node_id)
            .map(|edges| edges.iter().collect())
            .unwrap_or_default()
    }

    /// Get the number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Get the number of edges.
    pub fn edge_count(&self) -> usize {
        self.edges.values().map(|v| v.len()).sum()
    }

    /// Check if the graph contains any cycles.
    ///
    /// Unlike DAGs, cycles are ALLOWED in StateGraphs (they're the mechanism
    /// for ReAct loops). This method is for diagnostics, not validation.
    pub fn has_cycles(&self) -> bool {
        // Use DFS to detect back edges
        let mut visited = std::collections::HashSet::new();
        let mut recursion_stack = std::collections::HashSet::new();

        fn dfs(
            node_id: &str,
            graph: &StateGraph,
            visited: &mut std::collections::HashSet<String>,
            recursion_stack: &mut std::collections::HashSet<String>,
        ) -> bool {
            visited.insert(node_id.to_string());
            recursion_stack.insert(node_id.to_string());

            for edge in graph.get_edges_from(node_id) {
                if !visited.contains(&edge.to) {
                    if dfs(&edge.to, graph, visited, recursion_stack) {
                        return true;
                    }
                } else if recursion_stack.contains(&edge.to) {
                    return true; // Cycle detected
                }
            }

            recursion_stack.remove(node_id);
            false
        }

        for node_id in self.nodes.keys() {
            if !visited.contains(node_id) {
                if dfs(node_id, self, &mut visited, &mut recursion_stack) {
                    return true;
                }
            }
        }
        false
    }
}

// ─── GraphState ─────────────────────────────────────────────────────────────

/// The state that flows through the StateGraph during execution.
///
/// State is passed from node to node via the edges. Each node can
/// read from and write to this state. The state is serializable
/// for checkpoint persistence.
///
/// P2-2 extension: added fields for AgentLoop integration —
/// `parsed_decision` enables edge routing based on structured decisions
/// (not unreliable string pattern matching), `active_paradigm` tracks
/// the current paradigm for tool filtering, and `iteration_count`
/// enables IterationExceeds edge conditions for loop safety.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphState {
    /// The current conversation.
    pub conversation: oneai_core::Conversation,

    /// State variables (key-value pairs accessible by nodes).
    pub variables: HashMap<String, String>,

    /// The last action result (output from the most recently executed node).
    pub last_result: Option<String>,

    /// Error message (if the last node failed).
    pub last_error: Option<String>,

    /// Whether the graph execution should terminate.
    pub should_terminate: bool,

    /// Metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    // ─── P2-2: AgentLoop integration fields ──────────────────────────────────

    /// The parsed decision from the most recent LLM inference.
    ///
    /// After each LlmInfer node, the `GraphActionExecutor` parses the LLM
    /// response into a `GraphDecision` and stores it here. Edge conditions
    /// (HasToolCalls, IsFinalAnswer, RequestsDelegation) evaluate against
    /// this field instead of raw string content, making routing reliable
    /// and consistent with the AgentLoop's decision parsing.
    ///
    /// Cleared when a SwitchParadigm node is executed (new inference needed).
    #[serde(default)]
    pub parsed_decision: Option<oneai_core::GraphDecision>,

    /// The active paradigm for tool filtering and system prompts.
    ///
    /// When set, subsequent LlmInfer nodes (with `include_tool_definitions = true`)
    /// use the paradigm's tool_filter to build tool definitions. Paradigm-specific
    /// system prompts are also injected into the conversation.
    ///
    /// Set by SwitchParadigm nodes, or initialized from the AgentLoop's
    /// active paradigm when the graph starts executing.
    #[serde(default)]
    pub active_paradigm: Option<String>,

    /// The current iteration count in the graph walk.
    ///
    /// Incremented each time the executor processes a node. Enables
    /// IterationExceeds edge conditions for loop safety (e.g., "route
    /// to error_handler if iterations > 20").
    #[serde(default)]
    pub iteration_count: usize,

    /// Remaining token budget (in tokens).
    ///
    /// Decremented after each LLM inference. When it reaches 0,
    /// the graph should terminate (budget exhaustion).
    #[serde(default)]
    pub token_budget_remaining: u32,

    /// Accumulated interrupt points during execution.
    ///
    /// When a HumanApproval node is encountered (with `interrupt: true`),
    /// the executor records the interrupt point here. These are also
    /// included in `GraphExecutionResult.interrupt_checkpoints`.
    #[serde(default)]
    pub interrupt_points: Vec<oneai_core::InterruptPoint>,
}

impl GraphState {
    /// Create a new empty graph state.
    pub fn new() -> Self {
        Self {
            conversation: oneai_core::Conversation::new(),
            variables: HashMap::new(),
            last_result: None,
            last_error: None,
            should_terminate: false,
            metadata: HashMap::new(),
            parsed_decision: None,
            active_paradigm: None,
            iteration_count: 0,
            token_budget_remaining: 0, // 0 means "no budget limit set"
            interrupt_points: Vec::new(),
        }
    }

    /// Create a graph state with a token budget.
    pub fn with_budget(budget: u32) -> Self {
        Self {
            token_budget_remaining: budget,
            ..Self::new()
        }
    }
}

impl Default for GraphState {
    fn default() -> Self {
        Self::new()
    }
}

// ─── GraphExecutionResult ───────────────────────────────────────────────────

/// The result of executing a StateGraph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphExecutionResult {
    /// The graph name.
    pub name: String,

    /// The final state after execution.
    pub final_state: GraphState,

    /// Whether execution completed successfully (reached a terminal node).
    pub completed: bool,

    /// The terminal node reached (if completed).
    pub terminal_node: Option<String>,

    /// Number of node executions (iterations through the graph).
    pub iterations: usize,

    /// Checkpoint ID at each interrupt point (for resuming).
    pub interrupt_checkpoints: Vec<String>,
}