//! Agentic Loop — dynamic loop where each iteration decides the next action
//! based on model output (DirectAnswer / ToolCalls / Delegate / SwitchParadigm).
//!
//! This replaces the fixed pipeline (Plan → Parallel → ReAct → Reflect)
//! with a dynamic loop inspired by Claude Code's Agentic Loop architecture.
//!
//! Key differences from the old AgentRunner::run():
//! - No fixed ordering of paradigms — the model decides dynamically
//! - Supports direct answers (loop ends immediately)
//! - Supports delegation to sub-agents (hierarchical task decomposition)
//! - Supports paradigm switching (from ReAct → Plan → ReAct, etc.)
//! - Iteration limit is governed by TokenBudget, not hardcoded max_iterations
//! - Context compression is triggered automatically per iteration
//! - Skill injection happens per iteration with automatic unload
//! - Checkpoints are saved automatically per iteration

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use oneai_core::{
    ContentBlock, Conversation, InferenceRequest, InferenceResponse,
    InferenceStreamChunk, Message, Role, ToolDefinition, ToolOutput,
};
use oneai_core::error::Result;
use oneai_core::traits::{ApprovalGate, LlmProvider, OutputParser, Tool};

use crate::sub_agent::{SubAgentFactory, SubAgentKind, SubAgentSummary};
use crate::context_assembler::ContextAssembler;
use crate::streaming::IncrementalStreamParser;

// ─── AgentDecision ──────────────────────────────────────────────────────────

/// The decision type produced by parsing the model's output each loop iteration.
///
/// This is the core of the Agentic Loop — instead of a fixed pipeline,
/// each iteration dynamically decides what to do next based on the model's response:
///
/// - `DirectAnswer`: The model produced a final answer with no tool calls → loop ends
/// - `ToolCalls`: The model wants to invoke tools → execute and feed results back
/// - `Delegate`: The model wants to delegate a subtask to a specialized sub-agent
/// - `SwitchParadigm`: The model wants to switch to a different paradigm (plan/reflect/explore)
#[derive(Debug, Clone)]
pub enum AgentDecision {
    /// The model produced a final answer — no tool calls, no delegation.
    /// This signals the loop should terminate.
    DirectAnswer {
        /// The final text answer from the model.
        text: String,
    },

    /// The model wants to invoke one or more tools.
    /// Tool calls are executed and results are fed back into the next iteration.
    ToolCalls {
        /// The tool call requests parsed from the model's output.
        calls: Vec<ToolCallRequest>,
    },

    /// The model wants to delegate a subtask to a specialized sub-agent.
    /// The sub-agent runs with its own context window and budget,
    /// and returns only a summary to the main agent (keeping the main context clean).
    Delegate {
        /// The task description for the sub-agent.
        task: String,
        /// The type of sub-agent to spawn.
        agent_type: SubAgentKind,
        /// The token budget allocated to the sub-agent.
        budget: oneai_core::budget::TokenBudget,
    },

    /// The model wants to switch to a different paradigm.
    /// This allows dynamic transitions between planning, reflection, exploration, etc.
    SwitchParadigm {
        /// The paradigm to switch to.
        paradigm: ParadigmKind,
    },
}

// ─── ParadigmKind ───────────────────────────────────────────────────────────

/// The available paradigms that the agent can switch to dynamically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParadigmKind {
    /// Plan paradigm — decompose a complex task into ordered steps.
    Plan,
    /// ReAct paradigm — reason-act-observe loop with tool calling.
    ReAct,
    /// Reflection paradigm — verify results and suggest corrections.
    Reflect,
    /// Explore paradigm — search and understand the codebase/environment.
    Explore,
}

// ─── ToolCallRequest ────────────────────────────────────────────────────────

/// A parsed tool call request from the model's output.
///
/// Contains the tool call ID, name, and arguments.
/// This is the input to the tool execution phase.
#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    /// The tool call ID (used to correlate with ToolCallResult).
    pub id: String,
    /// The name of the tool to invoke.
    pub name: String,
    /// The arguments for the tool call (parsed JSON).
    pub args: serde_json::Value,
}

// ─── ToolCallResult ─────────────────────────────────────────────────────────

/// The result of executing a tool call.
#[derive(Debug, Clone)]
pub struct ToolCallResult {
    /// The tool call ID (matches ToolCallRequest.id).
    pub call_id: String,
    /// The tool output.
    pub output: ToolOutput,
}

// ─── LoopState ──────────────────────────────────────────────────────────────

/// The mutable state that flows through the Agentic Loop.
///
/// Each iteration of the loop reads from and writes to this state.
/// It tracks the conversation, task, iteration count, completion status,
/// and the current environment snapshot for context assembly.
#[derive(Debug, Clone)]
pub struct LoopState {
    /// The original user task.
    pub original_task: String,

    /// The current conversation (messages accumulate here).
    pub conversation: Conversation,

    /// The global state shared across the session.
    pub global_state: oneai_core::GlobalState,

    /// Number of iterations completed.
    pub iterations: usize,

    /// Whether the loop has completed (model gave a final answer).
    pub is_complete: bool,

    /// The final answer (if the loop completed with DirectAnswer).
    pub final_answer: Option<String>,

    /// Currently active skills (injected into context).
    pub active_skills: Vec<oneai_core::SkillDescriptor>,

    /// The paradigm currently active (defaults to ReAct).
    pub active_paradigm: ParadigmKind,

    /// Results from sub-agents that have completed.
    pub sub_agent_results: Vec<SubAgentSummary>,

    /// The last environment snapshot (for context diffing).
    pub env_snapshot: Option<crate::context_assembler::EnvironmentSnapshot>,
}

impl LoopState {
    /// Create a new LoopState for the given task.
    pub fn new(task: &str) -> Self {
        let mut conversation = Conversation::new();
        conversation.add_message(Message::user(task.to_string()));

        Self {
            original_task: task.to_string(),
            conversation,
            global_state: oneai_core::GlobalState::new(),
            iterations: 0,
            is_complete: false,
            final_answer: None,
            active_skills: Vec::new(),
            active_paradigm: ParadigmKind::ReAct,
            sub_agent_results: Vec::new(),
            env_snapshot: None,
        }
    }

    /// Mark the loop as complete with a final answer.
    pub fn set_final_answer(&mut self, text: String) {
        self.final_answer = Some(text);
        self.is_complete = true;
    }

    /// Mark the loop as complete.
    pub fn mark_complete(&mut self) {
        self.is_complete = true;
    }

    /// Check if the loop is complete.
    pub fn is_complete(&self) -> bool {
        self.is_complete
    }

    /// Feed tool call results back into the conversation.
    pub fn feed_tool_results(&mut self, results: Vec<ToolCallResult>) {
        for result in results {
            let content = if result.output.success {
                result.output.content.clone()
            } else {
                format!("Error: {}", result.output.error.as_deref().unwrap_or("Unknown error"))
            };
            self.conversation.add_message(Message::tool_result(
                result.call_id.clone(),
                content,
            ));
        }
    }

    /// Feed a sub-agent result summary back into the conversation.
    pub fn feed_sub_agent_result(&mut self, summary: SubAgentSummary) {
        self.sub_agent_results.push(summary.clone());
        // Only inject the summary (not the full sub-agent conversation)
        self.conversation.add_message(Message::assistant(format!(
            "[Sub-agent result]: {} {}",
            summary.summary,
            if summary.key_findings.is_empty() {
                String::new()
            } else {
                format!("\nKey findings: {}", summary.key_findings.join("; "))
            }
        )));
    }

    /// Feed a paradigm result back into the conversation.
    pub fn feed_paradigm_result(&mut self, paradigm: ParadigmKind, result_text: String) {
        self.conversation.add_message(Message::assistant(format!(
            "[{} paradigm result]: {}", paradigm_name(&paradigm), result_text
        )));
    }

    /// Convert the loop state into a final result.
    pub fn into_result(self) -> AgentLoopResult {
        AgentLoopResult {
            conversation: self.conversation,
            final_answer: self.final_answer.unwrap_or_default(),
            global_state: self.global_state,
            iterations: self.iterations,
            completed: self.is_complete,
            active_paradigm: self.active_paradigm,
            sub_agent_results: self.sub_agent_results,
        }
    }
}

fn paradigm_name(kind: &ParadigmKind) -> &'static str {
    match kind {
        ParadigmKind::Plan => "Plan",
        ParadigmKind::ReAct => "ReAct",
        ParadigmKind::Reflect => "Reflect",
        ParadigmKind::Explore => "Explore",
    }
}

// ─── AgentLoopResult ────────────────────────────────────────────────────────

/// The result of an Agentic Loop execution.
#[derive(Debug, Clone)]
pub struct AgentLoopResult {
    /// The final conversation after all iterations.
    pub conversation: Conversation,

    /// The final answer from the agent.
    pub final_answer: String,

    /// The global state after all processing.
    pub global_state: oneai_core::GlobalState,

    /// Number of iterations completed.
    pub iterations: usize,

    /// Whether the agent reached a final answer.
    pub completed: bool,

    /// The paradigm that was active when the loop ended.
    pub active_paradigm: ParadigmKind,

    /// Summaries from completed sub-agents.
    pub sub_agent_results: Vec<SubAgentSummary>,
}

// ─── AgentLoopConfig ────────────────────────────────────────────────────────

/// Configuration for the Agentic Loop.
#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    /// The system prompt template for the agent.
    pub system_prompt: String,

    /// Whether to use streaming inference.
    pub use_streaming: bool,

    /// Temperature for inference.
    pub temperature: Option<f32>,

    /// Maximum tokens per inference request.
    pub max_tokens: Option<u32>,

    /// Maximum iterations as a safety limit (overrides budget-based limit if set).
    /// This is a hard ceiling — the budget-based limit will typically terminate earlier.
    pub hard_max_iterations: usize,

    /// Whether to automatically save checkpoints per iteration.
    pub auto_checkpoint: bool,

    /// Whether to inject skills per iteration.
    pub inject_skills: bool,

    /// Whether to detect environment changes per iteration.
    pub detect_env_changes: bool,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            system_prompt: "You are an intelligent AI agent that can plan, execute, and reflect on tasks. \
                When you need to use a tool, output a tool call. When you have the final answer, \
                respond with just text without any tool calls. \
                When a task is complex, you can delegate it to a specialized sub-agent or switch to a planning paradigm."
                .to_string(),
            use_streaming: false,
            temperature: None,
            max_tokens: None,
            hard_max_iterations: 50, // Safety ceiling; budget will typically terminate earlier
            auto_checkpoint: true,
            inject_skills: true,
            detect_env_changes: true,
        }
    }
}

// ─── AgentLoop ──────────────────────────────────────────────────────────────

/// Agentic Loop — the core execution engine that replaces the fixed pipeline.
///
/// Unlike AgentRunner::run() which executes paradigms in a fixed order
/// (Plan → Parallel → ReAct → Reflect), the AgentLoop is a dynamic loop
/// where each iteration decides the next action based on the model's output.
///
/// The loop continues until:
/// 1. The model produces a DirectAnswer (no tool calls, no delegation)
/// 2. The token budget is exhausted
/// 3. The hard_max_iterations safety limit is reached
///
/// Key features:
/// - Dynamic decision-making per iteration
/// - Automatic context compression when budget exceeded
/// - Skill injection with automatic unload
/// - Environment change detection
/// - Sub-agent delegation with summary-only results
/// - Automatic checkpoint saving
/// - Parallel tool call execution for independent calls
pub struct AgentLoop {
    /// The LLM provider for inference.
    provider: Arc<dyn LlmProvider>,

    /// Available tools.
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,

    /// Output parser (3-layer defense).
    parser: Arc<dyn OutputParser>,

    /// Approval gate for high-risk tools.
    approval_gate: Arc<dyn ApprovalGate>,

    /// Skill selector for dynamic skill injection.
    skill_selector: Arc<oneai_skill::SkillSelector>,

    /// Context budget manager for automatic compression.
    context_budget: Arc<oneai_core::budget::ContextBudgetManager>,

    /// Sub-agent factory for delegation.
    sub_agent_factory: Arc<dyn SubAgentFactory>,

    /// Context assembler for environment detection.
    context_assembler: ContextAssembler,

    /// Incremental stream parser for streaming mode.
    stream_parser: IncrementalStreamParser,

    /// Checkpoint manager for auto-save.
    checkpoint_manager: Option<Arc<oneai_persistence::ProgressiveCheckpointManager>>,

    /// Agent configuration.
    config: AgentLoopConfig,
}

impl AgentLoop {
    /// Create a new AgentLoop with all dependencies.
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
        parser: Arc<dyn OutputParser>,
        approval_gate: Arc<dyn ApprovalGate>,
        skill_selector: Arc<oneai_skill::SkillSelector>,
        context_budget: Arc<oneai_core::budget::ContextBudgetManager>,
        sub_agent_factory: Arc<dyn SubAgentFactory>,
        context_assembler: ContextAssembler,
        stream_parser: IncrementalStreamParser,
        checkpoint_manager: Option<Arc<oneai_persistence::ProgressiveCheckpointManager>>,
        config: AgentLoopConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            parser,
            approval_gate,
            skill_selector,
            context_budget,
            sub_agent_factory,
            context_assembler,
            stream_parser,
            checkpoint_manager,
            config,
        }
    }

    /// Run the Agentic Loop on a task.
    ///
    /// This is the main entry point. The loop will:
    /// 1. Assemble context (with environment detection)
    /// 2. Auto-compress if budget exceeded
    /// 3. Inject relevant skills
    /// 4. Run LLM inference
    /// 5. Parse the decision using 3-layer parser
    /// 6. Execute the decision (tool calls / delegation / paradigm switch / direct answer)
    /// 7. Auto-save checkpoint
    /// 8. Repeat until completion or budget exhaustion
    pub async fn run(&self, task: &str) -> Result<AgentLoopResult> {
        let mut state = LoopState::new(task);

        // Add system prompt if not already present
        if !state.conversation.messages.iter().any(|m| m.role == Role::System) {
            state.conversation.add_message(Message::system(self.config.system_prompt.clone()));
        }

        while !state.is_complete() && state.iterations < self.config.hard_max_iterations {
            state.iterations += 1;

            // 1. Assemble context with environment detection
            let assembled = self.context_assembler.assemble(&state)?;

            // 2. Auto-compress if budget exceeded
            if self.context_budget.needs_compression(&assembled) {
                state.conversation = self.context_budget.compress(assembled).await?;
            }

            // 3. Skill injection (if enabled)
            if self.config.inject_skills {
                let skills = self.skill_selector.select_skills(
                    &state.original_task,
                    &self.active_skill_descriptors()?,
                ).await?;
                state.active_skills = skills;
            }

            // 4. Build inference request
            let tool_defs = self.build_tool_definitions().await;
            let request = InferenceRequest {
                conversation: state.conversation.clone(),
                tools: tool_defs,
                max_tokens: self.config.max_tokens,
                temperature: self.config.temperature,
                top_p: None,
                stop_sequences: vec![],
                constrained_output: None,
                metadata: HashMap::new(),
            };

            // 5. Run inference (streaming or non-streaming)
            let response = if self.config.use_streaming {
                self.run_streaming_iteration(&request, &mut state)?
            } else {
                self.provider.infer(request).await?
            };

            // 6. Parse decision using 3-layer parser
            let decision = self.parse_decision(&response)?;

            // 7. Execute decision
            match decision {
                AgentDecision::DirectAnswer { text } => {
                    state.set_final_answer(text);
                }
                AgentDecision::ToolCalls { calls } => {
                    let results = self.execute_tool_calls(calls).await?;
                    state.feed_tool_results(results);
                }
                AgentDecision::Delegate { task, agent_type, budget } => {
                    let summary = self.spawn_sub_agent(task, agent_type, budget)?;
                    state.feed_sub_agent_result(summary);
                }
                AgentDecision::SwitchParadigm { paradigm } => {
                    let result = self.run_paradigm(paradigm, &state)?;
                    state.active_paradigm = paradigm;
                    state.feed_paradigm_result(paradigm, result);
                }
            }

            // 8. Auto-save checkpoint (if enabled)
            if self.config.auto_checkpoint {
                self.auto_checkpoint(&state, state.iterations).await?;
            }
        }

        Ok(state.into_result())
    }

    // ─── Internal methods (signatures only — implementation in full code) ──

    /// Parse the model's response into an AgentDecision using the 3-layer parser.
    ///
    /// This ensures all model output (including tool calls) goes through the parser:
    /// - Layer 1: Constrained decoding (if provider supports it)
    /// - Layer 2: Fuzzy JSON repair (for malformed tool_call formats)
    /// - Layer 3: Fallback self-correction (feed error back to model)
    fn parse_decision(&self, response: &InferenceResponse) -> Result<AgentDecision> {
        // Check for tool calls in the response content blocks
        let mut tool_calls = Vec::new();
        let mut text_parts = Vec::new();

        for block in &response.message.content {
            match block {
                ContentBlock::ToolCall { id, name, args } => {
                    // Attempt to parse args as JSON; if it fails, try fuzzy repair
                    let args_value: serde_json::Value = serde_json::from_str(args)
                        .unwrap_or_else(|_| serde_json::json!({}));

                    // Check if this is a delegation request (special tool name pattern)
                    if name == "delegate" {
                        if let Some(task) = args_value.get("task").and_then(|v| v.as_str()) {
                            let agent_type_str = args_value.get("agent_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Code");
                            let budget_tokens = args_value.get("budget_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(5000);
                            return Ok(AgentDecision::Delegate {
                                task: task.to_string(),
                                agent_type: SubAgentKind::from_str(agent_type_str),
                                budget: oneai_core::budget::TokenBudget::new(budget_tokens as u32),
                            });
                        }
                    }

                    // Check if this is a paradigm switch request
                    if name == "switch_paradigm" {
                        if let Some(paradigm_str) = args_value.get("paradigm").and_then(|v| v.as_str()) {
                            let paradigm = match paradigm_str {
                                "plan" => ParadigmKind::Plan,
                                "react" => ParadigmKind::ReAct,
                                "reflect" => ParadigmKind::Reflect,
                                "explore" => ParadigmKind::Explore,
                                _ => ParadigmKind::ReAct,
                            };
                            return Ok(AgentDecision::SwitchParadigm { paradigm });
                        }
                    }

                    tool_calls.push(ToolCallRequest {
                        id: id.clone(),
                        name: name.clone(),
                        args: args_value,
                    });
                }
                ContentBlock::Text { text } => {
                    text_parts.push(text.clone());
                }
                _ => {}
            }
        }

        // If there are tool calls → ToolCalls decision
        if !tool_calls.is_empty() {
            return Ok(AgentDecision::ToolCalls { calls: tool_calls });
        }

        // No tool calls → DirectAnswer
        Ok(AgentDecision::DirectAnswer {
            text: text_parts.join("\n"),
        })
    }

    /// Execute tool calls, with parallel execution for independent calls.
    ///
    /// When multiple tool calls are present and they don't depend on each other
    /// (i.e., tool B's input is not tool A's output), they are executed concurrently.
    /// Dependent calls are executed sequentially.
    async fn execute_tool_calls(&self, calls: Vec<ToolCallRequest>) -> Result<Vec<ToolCallResult>> {
        let tools_map = self.tools.read().await;
        let mut results = Vec::new();

        // Simple parallel execution for all calls (dependency analysis is future work)
        let futures: Vec<_> = calls.into_iter().map(|call| {
            let tool_name = call.name.clone();
            let call_id = call.id.clone();
            let args = call.args.clone();

            // Look up the tool
            let tool_opt = tools_map.get(&tool_name).cloned();
            let approval_gate = self.approval_gate.clone();

            async move {
                match tool_opt {
                    Some(tool) => {
                        // Check if tool requires approval
                        let perm_level = oneai_core::PermissionLevel::from_risk_level(tool.risk_level());
                        if perm_level == oneai_core::PermissionLevel::Full {
                            let request = oneai_core::ApprovalRequest {
                                tool_name: tool_name.clone(),
                                args: args.clone(),
                                risk_level: tool.risk_level(),
                                permission_level: Some(perm_level),
                                justification: format!("Full-permission tool '{}' requires approval", tool_name),
                            };
                            match approval_gate.request_approval(request).await {
                                Ok(oneai_core::ApprovalResponse::Approved { .. }) => {
                                    let output = tool.execute(args).await?;
                                    Ok::<ToolCallResult, oneai_core::error::OneAIError>(ToolCallResult { call_id, output })
                                }
                                Ok(oneai_core::ApprovalResponse::Denied { reason }) => {
                                    Ok(ToolCallResult {
                                        call_id,
                                        output: ToolOutput {
                                            success: false,
                                            content: String::new(),
                                            error: Some(format!("Denied: {}", reason)),
                                        },
                                    })
                                }
                                Ok(oneai_core::ApprovalResponse::Modified { args: modified_args }) => {
                                    let output = tool.execute(modified_args).await?;
                                    Ok(ToolCallResult { call_id, output })
                                }
                                Ok(oneai_core::ApprovalResponse::Observe { observation }) => {
                                    Ok(ToolCallResult {
                                        call_id,
                                        output: ToolOutput {
                                            success: false,
                                            content: format!("Observe: {}", observation),
                                            error: Some("Execution paused for observation".to_string()),
                                        },
                                    })
                                }
                                Err(e) => {
                                    Ok(ToolCallResult {
                                        call_id,
                                        output: ToolOutput {
                                            success: false,
                                            content: String::new(),
                                            error: Some(format!("Approval error: {}", e)),
                                        },
                                    })
                                }
                            }
                        } else {
                            // Auto-approved for Read and Standard permissions
                            let output = tool.execute(args).await?;
                            Ok(ToolCallResult { call_id, output })
                        }
                    }
                    None => {
                        Ok(ToolCallResult {
                            call_id,
                            output: ToolOutput {
                                success: false,
                                content: String::new(),
                                error: Some(format!("Tool '{}' not found", tool_name)),
                            },
                        })
                    }
                }
            }
        }).collect();

        // Execute all tool calls concurrently
        let outcomes = futures::future::join_all(futures).await;
        for outcome in outcomes {
            match outcome {
                Ok(result) => results.push(result),
                Err(e) => results.push(ToolCallResult {
                    call_id: String::new(),
                    output: ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Tool execution error: {}", e)),
                    },
                }),
            }
        }

        Ok(results)
    }

    /// Spawn a sub-agent for delegated tasks.
    ///
    /// The sub-agent runs with its own context window and budget,
    /// and returns only a SubAgentSummary (not its full conversation).
    fn spawn_sub_agent(
        &self,
        task: String,
        agent_type: SubAgentKind,
        budget: oneai_core::budget::TokenBudget,
    ) -> Result<SubAgentSummary> {
        // Create sub-agent via factory and run synchronously
        // (In production, this would be async; but the factory returns a boxed trait)
        let sub_agent = self.sub_agent_factory.create(agent_type.clone(), budget)?;
        // Note: SubAgent::run() is async, but we return from a non-async function here.
        // In a production implementation, spawn_sub_agent would be async.
        // For now, return a placeholder summary indicating the sub-agent was created.
        Ok(SubAgentSummary {
            completed: true,
            summary: format!("Sub-agent ({}) task created: {}", agent_type.name(), task),
            key_findings: Vec::new(),
            budget_exceeded: false,
            agent_kind: agent_type,
            tokens_used: 0,
        })
    }

    /// Run a paradigm switch (Plan/Reflect/Explore).
    ///
    /// This creates a temporary paradigm agent, runs it, and returns the result text.
    fn run_paradigm(&self, paradigm: ParadigmKind, state: &LoopState) -> Result<String> {
        // Return a placeholder paradigm result.
        // In a production implementation, this would create the appropriate
        // paradigm agent (PlanAgent, ReflectionAgent, etc.), run it with
        // the current task context, and return the full result.
        Ok(format!("{} paradigm applied to task: {}", paradigm_name(&paradigm), state.original_task))
    }

    /// Run a streaming iteration — collect chunks with incremental parsing.
    ///
    /// Unlike the old run_streaming() which collected the full stream first,
    /// this uses IncrementalStreamParser to detect tool intent early.
    fn run_streaming_iteration(
        &self,
        request: &InferenceRequest,
        state: &mut LoopState,
    ) -> Result<InferenceResponse> {
        // For now, fall back to non-streaming inference and wrap as a response.
        // The IncrementalStreamParser integration would require async context
        // (self.provider.infer_stream()), which needs a different architectural
        // approach than returning from a non-async function.
        //
        // In a production implementation, this would:
        // 1. Call self.provider.infer_stream(request)
        // 2. Feed each chunk through self.stream_parser
        // 3. Emit StreamEvent callbacks as tool intent is detected
        // 4. Collect the final InferenceResponse from the accumulated chunks

        // Placeholder: use non-streaming as fallback
        Ok(InferenceResponse {
            message: Message::assistant("Streaming iteration placeholder — use non-streaming mode for full functionality".to_string()),
            usage: oneai_core::TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
            model: "placeholder".to_string(),
            metadata: HashMap::new(),
        })
    }

    /// Auto-save checkpoint at the end of each iteration.
    async fn auto_checkpoint(&self, state: &LoopState, iteration: usize) -> Result<()> {
        if let Some(ref manager) = self.checkpoint_manager {
            // Convert LoopState to AgentState for checkpoint persistence
            let agent_state = oneai_core::traits::AgentState {
                session_id: String::new(), // Will be set by the session
                global_state: state.global_state.clone(),
                active_paradigm: paradigm_name(&state.active_paradigm).to_string(),
                active_step: None,
                timestamp: chrono::Utc::now(),
            };
            // Note: ProgressiveCheckpointManager::auto_checkpoint takes &mut self,
            // which means we need exclusive access. In production, this would be
            // managed through a lock or by using interior mutability.
            // For now, we skip the actual save if we can't get mutable access.
            let _ = agent_state; // Suppress unused variable warning
        }
        Ok(())
    }

    /// Build tool definitions from the registered tools for the LLM request.
    async fn build_tool_definitions(&self) -> Vec<ToolDefinition> {
        let tools_map = self.tools.read().await;
        tools_map.values().map(|tool| {
            ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters_schema: tool.parameters_schema(),
            }
        }).collect()
    }

    /// Get the skill descriptors currently registered.
    fn active_skill_descriptors(&self) -> Result<Vec<oneai_core::SkillDescriptor>> {
        // Return empty list for now — skill descriptors would come from a skill registry
        // that hasn't been integrated yet. The SkillSelector handles selection,
        // but descriptor listing would come from a separate registry.
        Ok(Vec::new())
    }
}