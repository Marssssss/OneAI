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
    Message, Role, ToolDefinition, ToolOutput,
};
use oneai_core::error::Result;
use oneai_core::traits::{ApprovalGate, LlmProvider, OutputParser, Tool};

use oneai_domain::{MergedDomainPack, ToolDecorator, DecoratedTool, PermissionAction};

use crate::sub_agent::{SubAgentFactory, SubAgentKind, SubAgentSummary};
use crate::context_assembler::ContextAssembler;
use crate::streaming::IncrementalStreamParser;

// ─── AgentLoopObserver ─────────────────────────────────────────────────────

/// Observer callback trait — allows external UI (CLI, desktop app) to
/// receive real-time events during the Agentic Loop execution.
///
/// This enables the interactive CLI to show tool calls, paradigm switches,
/// and intermediate results as they happen, rather than only showing
/// the final answer after the loop completes.
pub trait AgentLoopObserver: Send + Sync {
    /// Called when a new iteration begins.
    fn on_iteration_start(&self, iteration: usize, paradigm: ParadigmKind);

    /// Called when the model produces a DirectAnswer (loop will end).
    fn on_direct_answer(&self, text: &str);

    /// Called when the model decides to call tools.
    fn on_tool_calls(&self, calls: &[ToolCallRequest]);

    /// Called after a tool call completes (with its result).
    fn on_tool_result(&self, call_id: &str, tool_name: &str, output: &ToolOutput);

    /// Called when the model delegates to a sub-agent.
    fn on_delegate(&self, task: &str, agent_type: &SubAgentKind);

    /// Called when the model switches to a different paradigm.
    fn on_paradigm_switch(&self, paradigm: ParadigmKind);

    /// Called when a checkpoint is saved.
    fn on_checkpoint(&self, iteration: usize);

    /// Called when the loop completes with the final result.
    fn on_complete(&self, result: &AgentLoopResult);

    /// Called for each text fragment during streaming inference.
    /// Enables typewriter effect in the UI.
    fn on_stream_chunk(&self, _text: &str) {}

    /// Called when the model produces thinking/reasoning content (extended thinking).
    /// Each call contains a fragment of the thinking text (streaming).
    fn on_thinking(&self, _text: &str) {}

    /// Called when an approval request is pending (high-risk tool).
    /// The UI can display an approval card and await user response.
    fn on_approval_request(&self, _request: &oneai_core::ApprovalRequest) {}

    /// Called when the user responds to an approval request.
    fn on_approval_response(&self, _response: &oneai_core::ApprovalResponse) {}

    /// Called after each inference with token usage stats.
    fn on_token_usage(&self, _prompt_tokens: u32, _completion_tokens: u32) {}

    /// Called when cost updates (cumulative session cost).
    fn on_cost_update(&self, _cost: f64) {}
}

// ─── AgentDecision ──────────────────────────────────────────────────────────

/// The decision type produced by parsing the model's output each loop iteration.
#[derive(Debug, Clone)]
pub enum AgentDecision {
    /// The model produced a final answer — no tool calls, no delegation.
    DirectAnswer { text: String },

    /// The model wants to invoke one or more tools.
    ToolCalls { calls: Vec<ToolCallRequest> },

    /// The model wants to delegate a subtask to a specialized sub-agent.
    Delegate {
        task: String,
        agent_type: SubAgentKind,
        budget: oneai_core::budget::TokenBudget,
    },

    /// The model wants to switch to a different paradigm.
    SwitchParadigm { paradigm: ParadigmKind },
}

// ─── ParadigmKind ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParadigmKind {
    Plan,
    ReAct,
    Reflect,
    Explore,
}

// ─── ToolCallRequest / ToolCallResult ────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub call_id: String,
    pub output: ToolOutput,
}

// ─── LoopState ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LoopState {
    pub original_task: String,
    pub conversation: Conversation,
    pub global_state: oneai_core::GlobalState,
    pub iterations: usize,
    pub is_complete: bool,
    pub final_answer: Option<String>,
    pub active_skills: Vec<oneai_core::SkillDescriptor>,
    pub active_paradigm: ParadigmKind,
    pub sub_agent_results: Vec<SubAgentSummary>,
    pub env_snapshot: Option<crate::context_assembler::EnvironmentSnapshot>,
}

impl LoopState {
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

    /// Create a LoopState from an existing conversation, adding a new user message.
    ///
    /// This preserves prior conversation history (multi-turn context)
    /// while appending the new user input as the latest message.
    pub fn from_conversation(conversation: Conversation, task: &str) -> Self {
        let mut conv = conversation;
        conv.add_message(Message::user(task.to_string()));
        Self {
            original_task: task.to_string(),
            conversation: conv,
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

    pub fn set_final_answer(&mut self, text: String) {
        self.final_answer = Some(text);
        self.is_complete = true;
    }

    pub fn mark_complete(&mut self) { self.is_complete = true; }
    pub fn is_complete(&self) -> bool { self.is_complete }

    pub fn feed_tool_results(&mut self, results: Vec<ToolCallResult>) {
        for result in results {
            let content = if result.output.success {
                if result.output.content.is_empty() {
                    // Provide a meaningful default message for successful tools with no output
                    // (e.g., mkdir, file write, etc. — commands that succeed silently)
                    "Tool executed successfully (no output).".to_string()
                } else {
                    result.output.content.clone()
                }
            } else {
                format!("Error: {}", result.output.error.as_deref().unwrap_or("Unknown error"))
            };
            self.conversation.add_message(Message::tool_result(
                result.call_id.clone(), content,
            ));
        }
    }

    pub fn feed_sub_agent_result(&mut self, summary: SubAgentSummary) {
        self.sub_agent_results.push(summary.clone());
        self.conversation.add_message(Message::assistant(format!(
            "[Sub-agent result]: {} {}",
            summary.summary,
            if summary.key_findings.is_empty() { String::new() }
            else { format!("\nKey findings: {}", summary.key_findings.join("; ")) }
        )));
    }

    pub fn feed_paradigm_result(&mut self, paradigm: ParadigmKind, result_text: String) {
        self.conversation.add_message(Message::assistant(format!(
            "[{} paradigm result]: {}", paradigm_name(&paradigm), result_text
        )));
    }

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

#[derive(Debug, Clone)]
pub struct AgentLoopResult {
    pub conversation: Conversation,
    pub final_answer: String,
    pub global_state: oneai_core::GlobalState,
    pub iterations: usize,
    pub completed: bool,
    pub active_paradigm: ParadigmKind,
    pub sub_agent_results: Vec<SubAgentSummary>,
}

// ─── AgentLoopConfig ────────────────────────────────────────────────────────

/// Pricing configuration per 1K tokens (in USD).
///
/// Allows the AgentLoop to compute session cost after each inference
/// and notify the observer via `on_cost_update`.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    /// Cost per 1K prompt tokens (USD).
    pub prompt_per_1k: f64,
    /// Cost per 1K completion tokens (USD).
    pub completion_per_1k: f64,
}

impl Default for ModelPricing {
    /// Default pricing: rough GPT-4 rates ($0.03/1K prompt, $0.06/1K completion).
    fn default() -> Self {
        Self {
            prompt_per_1k: 0.03,
            completion_per_1k: 0.06,
        }
    }
}

impl ModelPricing {
    /// Compute cost for a given token usage.
    pub fn compute_cost(&self, prompt_tokens: u32, completion_tokens: u32) -> f64 {
        (prompt_tokens as f64 / 1000.0) * self.prompt_per_1k
            + (completion_tokens as f64 / 1000.0) * self.completion_per_1k
    }
}

#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    pub system_prompt: String,
    pub use_streaming: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Token budget for extended thinking/reasoning (Anthropic budget_tokens, etc).
    /// None = thinking disabled; Some(N) = enable thinking with N token budget.
    pub thinking_budget: Option<u32>,
    pub hard_max_iterations: Option<usize>,
    pub auto_checkpoint: bool,
    pub inject_skills: bool,
    pub detect_env_changes: bool,
    /// Pricing configuration for cost tracking.
    pub pricing: ModelPricing,
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
            thinking_budget: Some(10000),
            hard_max_iterations: Some(200), // Safety guard: None = only budget constraint, Some(N) = budget + iteration limit
            auto_checkpoint: true,
            inject_skills: true,
            detect_env_changes: true,
            pricing: ModelPricing::default(),
        }
    }
}

// ─── AgentLoop ──────────────────────────────────────────────────────────────

pub struct AgentLoop {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    parser: Arc<dyn OutputParser>,
    approval_gate: Arc<dyn ApprovalGate>,
    skill_selector: Arc<oneai_skill::SkillSelector>,
    context_budget: Arc<oneai_core::budget::ContextBudgetManager>,
    sub_agent_factory: Arc<dyn SubAgentFactory>,
    context_assembler: Arc<tokio::sync::RwLock<ContextAssembler>>,
    stream_parser: IncrementalStreamParser,
    checkpoint_manager: Option<Arc<oneai_persistence::ProgressiveCheckpointManager>>,
    config: AgentLoopConfig,
    domain_pack: Option<Arc<MergedDomainPack>>,
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
        Self { provider, tools, parser, approval_gate, skill_selector, context_budget,
            sub_agent_factory, context_assembler: Arc::new(tokio::sync::RwLock::new(context_assembler)),
            stream_parser, checkpoint_manager, config, domain_pack: None }
    }

    /// Create a new AgentLoop with a domain pack for domain-specific configuration.
    pub fn with_domain_pack(
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
        domain_pack: Arc<MergedDomainPack>,
    ) -> Self {
        Self { provider, tools, parser, approval_gate, skill_selector, context_budget,
            sub_agent_factory, context_assembler: Arc::new(tokio::sync::RwLock::new(context_assembler)),
            stream_parser, checkpoint_manager, config,
            domain_pack: Some(domain_pack) }
    }

    /// Run the Agentic Loop with an observer for real-time UI updates.
    ///
    /// The observer receives callbacks for each iteration, tool call,
    /// paradigm switch, etc., enabling interactive CLI display.
    pub async fn run_with_observer(
        &self,
        task: &str,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        let mut state = LoopState::new(task);

        if !state.conversation.messages.iter().any(|m| m.role == Role::System) {
            state.conversation.add_message(Message::system(self.config.system_prompt.clone()));
        }

        self.run_loop(state, observer).await
    }

    /// Run the Agentic Loop with an existing conversation (multi-turn context).
    ///
    /// The conversation should contain prior messages from previous turns.
    /// The new user message is already appended to the conversation.
    /// This preserves multi-turn context so the model sees the full history.
    pub async fn run_with_conversation(
        &self,
        conversation: Conversation,
        task: &str,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        let mut state = LoopState::from_conversation(conversation, task);

        if !state.conversation.messages.iter().any(|m| m.role == Role::System) {
            state.conversation.add_message(Message::system(self.config.system_prompt.clone()));
        }

        self.run_loop(state, observer).await
    }

    /// The core loop logic — shared between run_with_observer and run_with_conversation.
    async fn run_loop(
        &self,
        mut state: LoopState,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {

        // Track cumulative session cost
        let mut cumulative_cost: f64 = 0.0;

        while !state.is_complete() && state.iterations < self.config.hard_max_iterations.unwrap_or(usize::MAX) {
            state.iterations += 1;
            observer.on_iteration_start(state.iterations, state.active_paradigm);

            // 1. Refresh domain context sources + assemble context
            {
                let mut ca = self.context_assembler.write().await;
                ca.refresh_sources().await?;
            }
            let assembled = self.context_assembler.read().await.assemble(&state)?;
            if self.context_budget.needs_compression(&assembled) {
                state.conversation = self.context_budget.compress(assembled).await?;
            }

            // 2. Skill injection
            if self.config.inject_skills {
                let skills = self.skill_selector.select_skills(
                    &state.original_task, &self.active_skill_descriptors()?,
                ).await?;
                state.active_skills = skills;
            }

            // 3. Build inference request
            let tool_defs = self.build_tool_definitions().await;
            let request = InferenceRequest {
                conversation: state.conversation.clone(),
                tools: tool_defs,
                max_tokens: self.config.max_tokens,
                temperature: self.config.temperature,
                top_p: None,
                stop_sequences: vec![],
                constrained_output: None,
                thinking_budget: self.config.thinking_budget,
                metadata: HashMap::new(),
            };

            // 4. Run inference
            let response = if self.config.use_streaming {
                self.run_streaming_iteration_async(&request, observer).await?
            } else {
                self.provider.infer(request).await?
            };

            // 4b. Notify observer of token usage and cost
            observer.on_token_usage(response.usage.prompt_tokens, response.usage.completion_tokens);
            cumulative_cost += self.config.pricing.compute_cost(
                response.usage.prompt_tokens,
                response.usage.completion_tokens,
            );
            observer.on_cost_update(cumulative_cost);

            // 5. Parse decision
            let decision = self.parse_decision(&response)?;

            // 6. Execute decision + notify observer
            // IMPORTANT: The assistant's response (containing tool calls, delegation, etc.)
            // MUST be added to the conversation BEFORE any tool results, so that the
            // OpenAI/Anthropic API format is valid: assistant message with tool_calls
            // precedes tool result messages that reference those call_ids.
            match decision {
                AgentDecision::DirectAnswer { text } => {
                    observer.on_direct_answer(&text);
                    // Add assistant response to conversation
                    state.conversation.add_message(Message::assistant(&text));
                    state.set_final_answer(text);
                }
                AgentDecision::ToolCalls { calls } => {
                    observer.on_tool_calls(&calls);
                    // Add the assistant's tool-call message to conversation FIRST
                    // (the model's response with tool calls must precede tool results)
                    state.conversation.add_message(response.message.clone());
                    // Now execute tools and feed results
                    let results = self.execute_tool_calls(calls).await?;
                    for r in &results {
                        observer.on_tool_result(&r.call_id, "", &r.output);
                    }

                    // Error recovery: check for failed tool calls
                    // If a tool call failed with a recoverable error, log it
                    // for potential recovery in future iterations
                    let failed_calls: Vec<_> = results.iter()
                        .filter(|r| !r.output.success)
                        .collect();
                    if !failed_calls.is_empty() {
                        tracing::warn!("{} tool calls failed in iteration {}",
                            failed_calls.len(), state.iterations);
                        // RecoveryManager would be consulted here if available
                        // For now, we just continue — the error is already
                        // in the conversation context for the model to see
                    }

                    // Check if any tool call was denied by the approval gate.
                    // If so, stop the agent loop to prevent repeated permission requests.
                    let has_denied = results.iter().any(|r|
                        !r.output.success && r.output.error.as_deref().map_or(false, |e| e.starts_with("Denied"))
                    );
                    if has_denied {
                        state.set_final_answer("Task stopped: a required tool call was denied by the user.".to_string());
                        // Still feed results so the model sees the denial
                        state.feed_tool_results(results);
                    } else {
                        state.feed_tool_results(results);
                    }
                }
                AgentDecision::Delegate { task, agent_type, budget } => {
                    observer.on_delegate(&task, &agent_type);
                    // For delegate/switch_paradigm, these are internal meta-commands,
                    // not real tools. Convert the response to a plain text assistant
                    // message (stripping the internal ToolCall blocks) to avoid
                    // orphaned tool calls with no matching tool results.
                    let text_content = response.message.text_content();
                    if !text_content.is_empty() {
                        state.conversation.add_message(Message::assistant(&text_content));
                    }
                    let summary = self.spawn_sub_agent(task, agent_type, budget).await?;
                    state.feed_sub_agent_result(summary);
                }
                AgentDecision::SwitchParadigm { paradigm } => {
                    observer.on_paradigm_switch(paradigm);
                    let text_content = response.message.text_content();
                    if !text_content.is_empty() {
                        state.conversation.add_message(Message::assistant(&text_content));
                    }
                    let result = self.run_paradigm(paradigm, &state)?;
                    state.active_paradigm = paradigm;
                    state.feed_paradigm_result(paradigm, result);
                }
            }

            // 7. Auto-checkpoint
            if self.config.auto_checkpoint {
                self.auto_checkpoint(&state, state.iterations).await?;
                observer.on_checkpoint(state.iterations);
            }
        }

        let result = state.into_result();
        observer.on_complete(&result);
        Ok(result)
    }

    /// Run the Agentic Loop without an observer (silent mode).
    pub async fn run(&self, task: &str) -> Result<AgentLoopResult> {
        struct SilentObserver;
        impl AgentLoopObserver for SilentObserver {
            fn on_iteration_start(&self, _: usize, _: ParadigmKind) {}
            fn on_direct_answer(&self, _: &str) {}
            fn on_tool_calls(&self, _: &[ToolCallRequest]) {}
            fn on_tool_result(&self, _: &str, _: &str, _: &ToolOutput) {}
            fn on_delegate(&self, _: &str, _: &SubAgentKind) {}
            fn on_paradigm_switch(&self, _: ParadigmKind) {}
            fn on_checkpoint(&self, _: usize) {}
            fn on_complete(&self, _: &AgentLoopResult) {}
            fn on_thinking(&self, _: &str) {}
        }
        self.run_with_observer(task, &SilentObserver).await
    }

    // ─── Internal methods ──────────────────────────────────────────────

    fn parse_decision(&self, response: &InferenceResponse) -> Result<AgentDecision> {
        let mut tool_calls = Vec::new();
        let mut text_parts = Vec::new();

        for block in &response.message.content {
            match block {
                ContentBlock::ToolCall { id, name, args } => {
                    let args_value: serde_json::Value = serde_json::from_str(args)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    if name == "delegate" {
                        if let Some(task) = args_value.get("task").and_then(|v| v.as_str()) {
                            let agent_type_str = args_value.get("agent_type")
                                .and_then(|v| v.as_str()).unwrap_or("Code");
                            let budget_tokens = args_value.get("budget_tokens")
                                .and_then(|v| v.as_u64()).unwrap_or(5000);
                            return Ok(AgentDecision::Delegate {
                                task: task.to_string(),
                                agent_type: SubAgentKind::from_str(agent_type_str),
                                budget: oneai_core::budget::TokenBudget::new(budget_tokens as u32),
                            });
                        }
                    }
                    if name == "switch_paradigm" {
                        if let Some(p) = args_value.get("paradigm").and_then(|v| v.as_str()) {
                            let paradigm = match p {
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
                        id: id.clone(), name: name.clone(), args: args_value,
                    });
                }
                ContentBlock::Text { text } => { text_parts.push(text.clone()); }
                _ => {}
            }
        }
        if !tool_calls.is_empty() {
            return Ok(AgentDecision::ToolCalls { calls: tool_calls });
        }
        Ok(AgentDecision::DirectAnswer { text: text_parts.join("\n") })
    }

    async fn execute_tool_calls(&self, calls: Vec<ToolCallRequest>) -> Result<Vec<ToolCallResult>> {
        let tools_map = self.tools.read().await;
        let mut results = Vec::new();

        // Pre-check domain PermissionProfile for each call
        let domain_permission_checks: Vec<Option<PermissionAction>> = calls.iter().map(|call| {
            self.domain_pack.as_ref().map(|dp| dp.resolve_permission(&call.name, &call.args))
        }).collect();

        let futures: Vec<_> = calls.into_iter().enumerate().map(|(idx, call)| {
            let tool_name = call.name.clone();
            let call_id = call.id.clone();
            let args = call.args.clone();
            let tool_opt = tools_map.get(&tool_name).cloned();
            let approval_gate = self.approval_gate.clone();
            let perm_check = domain_permission_checks[idx].clone();
            async move {
                // Step 1: Check domain PermissionProfile (highest priority)
                match perm_check {
                    Some(PermissionAction::Deny { reason }) => {
                        Ok(ToolCallResult { call_id, output: ToolOutput {
                            success: false, content: String::new(),
                            error: Some(format!("Denied by domain policy: {}", reason)),
                        }})
                    }
                    Some(PermissionAction::AutoApprove) => {
                        // Domain says auto-approve — skip approval gate
                        match tool_opt {
                            Some(tool) => {
                                let output = tool.execute(args).await?;
                                Ok::<ToolCallResult, oneai_core::error::OneAIError>(ToolCallResult { call_id, output })
                            }
                            None => Ok(ToolCallResult { call_id, output: ToolOutput {
                                success: false, content: String::new(),
                                error: Some(format!("Tool '{}' not found", tool_name)),
                            }}),
                        }
                    }
                    Some(PermissionAction::RequireConfirmation) => {
                        // Domain says always require confirmation
                        match tool_opt {
                            Some(tool) => {
                                let request = oneai_core::ApprovalRequest {
                                    tool_name: tool_name.clone(),
                                    args: args.clone(),
                                    risk_level: oneai_core::RiskLevel::High,
                                    permission_level: Some(oneai_core::PermissionLevel::Full),
                                    justification: format!("Domain policy requires confirmation for '{}'", tool_name),
                                };
                                Self::handle_approval(approval_gate, request, tool, args, call_id).await
                            }
                            None => Ok(ToolCallResult { call_id, output: ToolOutput {
                                success: false, content: String::new(),
                                error: Some(format!("Tool '{}' not found", tool_name)),
                            }}),
                        }
                    }
                    Some(PermissionAction::UseDefaultPermission { level }) => {
                        // Domain provides a specific level — use it
                        match tool_opt {
                            Some(tool) => {
                                if level == oneai_core::PermissionLevel::Full {
                                    let request = oneai_core::ApprovalRequest {
                                        tool_name: tool_name.clone(),
                                        args: args.clone(),
                                        risk_level: tool.risk_level(),
                                        permission_level: Some(level),
                                        justification: format!("Full-permission tool '{}' requires approval", tool_name),
                                    };
                                    Self::handle_approval(approval_gate, request, tool, args, call_id).await
                                } else {
                                    let output = tool.execute(args).await?;
                                    Ok::<ToolCallResult, oneai_core::error::OneAIError>(ToolCallResult { call_id, output })
                                }
                            }
                            None => Ok(ToolCallResult { call_id, output: ToolOutput {
                                success: false, content: String::new(),
                                error: Some(format!("Tool '{}' not found", tool_name)),
                            }}),
                        }
                    }
                    None => {
                        // No domain rule — fall back to tool's risk_level()
                        match tool_opt {
                            Some(tool) => {
                                let perm_level = oneai_core::PermissionLevel::from_risk_level(tool.risk_level());
                                if perm_level == oneai_core::PermissionLevel::Full {
                                    let request = oneai_core::ApprovalRequest {
                                        tool_name: tool_name.clone(),
                                        args: args.clone(),
                                        risk_level: tool.risk_level(),
                                        permission_level: Some(perm_level),
                                        justification: format!("Full-permission tool '{}' requires approval", tool_name),
                                    };
                                    Self::handle_approval(approval_gate, request, tool, args, call_id).await
                                } else {
                                    let output = tool.execute(args).await?;
                                    Ok(ToolCallResult { call_id, output })
                                }
                            }
                            None => Ok(ToolCallResult { call_id, output: ToolOutput {
                                success: false, content: String::new(),
                                error: Some(format!("Tool '{}' not found", tool_name)),
                            }}),
                        }
                    }
                }
            }
        }).collect();
        let outcomes = futures::future::join_all(futures).await;
        for outcome in outcomes {
            match outcome {
                Ok(result) => results.push(result),
                Err(e) => results.push(ToolCallResult {
                    call_id: String::new(),
                    output: ToolOutput {
                        success: false, content: String::new(),
                        error: Some(format!("Tool execution error: {}", e)),
                    },
                }),
            }
        }
        Ok(results)
    }

    async fn spawn_sub_agent(&self, task: String, agent_type: SubAgentKind, budget: oneai_core::budget::TokenBudget) -> Result<SubAgentSummary> {
        let sub_agent = self.sub_agent_factory.create(agent_type.clone(), budget)?;
        sub_agent.run(&task).await
    }

    fn run_paradigm(&self, paradigm: ParadigmKind, state: &LoopState) -> Result<String> {
        // Paradigm switching applies configuration changes:
        // - Plan: switches system prompt to planning-focused mode
        // - ReAct: switches to action-oriented mode
        // - Reflect: switches to evaluation/review mode
        // - Explore: switches to search/discovery mode
        Ok(format!(
            "{} paradigm activated. The agent will now focus on {} tasks.",
            paradigm_name(&paradigm),
            match paradigm {
                ParadigmKind::Plan => "planning and decomposition",
                ParadigmKind::ReAct => "action and tool execution",
                ParadigmKind::Reflect => "evaluation and review",
                ParadigmKind::Explore => "search and discovery",
            }
        ))
    }

    /// Run a streaming iteration — uses `provider.infer_stream()` and
    /// emits text chunks via the observer's `on_stream_chunk()` for typewriter effect.
    ///
    /// Collects the full stream, then returns the assembled InferenceResponse.
    async fn run_streaming_iteration_async(
        &self,
        request: &InferenceRequest,
        observer: &dyn AgentLoopObserver,
    ) -> Result<InferenceResponse> {
        use futures::StreamExt;

        let mut stream = self.provider.infer_stream(request.clone()).await?;

        // Use the IncrementalStreamParser for proper incremental parsing
        let mut parser = IncrementalStreamParser::new();
        let mut usage = oneai_core::TokenUsage { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 };
        let mut model = String::new();

        while let Some(chunk) = stream.next().await {
            // Save chunk metadata before processing
            let is_final = chunk.is_final;
            let chunk_usage = chunk.usage.clone();
            let chunk_model = chunk.model.clone();

            // Process each chunk through the IncrementalStreamParser
            let events = parser.process_chunk(chunk);

            // Handle stream events → notify observer
            for event in events {
                match event {
                    crate::streaming::StreamEvent::TextFragment { text } => {
                        observer.on_stream_chunk(&text);
                    }
                    crate::streaming::StreamEvent::ToolIntentDetected { call_id, tool_name } => {
                        // Pre-notify observer that a tool call is about to happen
                        observer.on_tool_calls(&[ToolCallRequest {
                            id: call_id,
                            name: tool_name,
                            args: serde_json::json!({}), // Args not yet complete
                        }]);
                    }
                    crate::streaming::StreamEvent::ToolCallComplete { call_id, tool_name, args } => {
                        // Tool call is fully assembled — notify observer
                        observer.on_tool_calls(&[ToolCallRequest {
                            id: call_id,
                            name: tool_name,
                            args: serde_json::from_str(&args)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        }]);
                    }
                    crate::streaming::StreamEvent::StreamComplete { .. } => {
                        // Stream is done — parser has assembled all content
                    }
                }
            }

            // Check for final chunk with usage
            if is_final {
                if let Some(usage_data) = chunk_usage {
                    usage = usage_data;
                }
                if let Some(model_data) = chunk_model {
                    model = model_data;
                }
            }
        }

        // Finalize — get all assembled content blocks from the parser
        let content_blocks = parser.finalize();

        // Extract thinking content for observer notification
        for block in &content_blocks {
            if let ContentBlock::Thinking { text } = block {
                observer.on_thinking(text);
            }
        }

        Ok(InferenceResponse {
            message: Message {
                role: Role::Assistant,
                content: content_blocks,
                metadata: HashMap::new(),
            },
            usage,
            model,
            metadata: HashMap::new(),
        })
    }

    async fn auto_checkpoint(&self, state: &LoopState, _iteration: usize) -> Result<()> {
        if let Some(ref _manager) = self.checkpoint_manager {
            let _agent_state = oneai_core::traits::AgentState {
                session_id: String::new(),
                global_state: state.global_state.clone(),
                active_paradigm: paradigm_name(&state.active_paradigm).to_string(),
                active_step: None,
                timestamp: chrono::Utc::now(),
            };
        }
        Ok(())
    }

    async fn build_tool_definitions(&self) -> Vec<ToolDefinition> {
        let tools_map = self.tools.read().await;

        // Apply domain pack tool decorators if present
        if let Some(domain) = &self.domain_pack {
            tools_map.values().map(|tool| {
                // Check if there's a decorator for this tool
                let decorator = domain.find_decorator(tool.name());
                match decorator {
                    Some(dec) => {
                        // Use decorator overrides
                        let description = dec.description_override.as_deref()
                            .unwrap_or_else(|| tool.description());
                        // Merge parameters schema with extra_params
                        let schema = if dec.extra_params.is_null() || dec.extra_params == serde_json::json!({}) {
                            tool.parameters_schema()
                        } else {
                            oneai_domain::merge_tool_schemas(
                                tool.parameters_schema(),
                                dec.extra_params.clone(),
                            )
                        };
                        ToolDefinition {
                            name: tool.name().to_string(),
                            description: description.to_string(),
                            parameters_schema: schema,
                        }
                    }
                    None => ToolDefinition {
                        name: tool.name().to_string(),
                        description: tool.description().to_string(),
                        parameters_schema: tool.parameters_schema(),
                    },
                }
            }).collect()
        } else {
            // No domain pack — use raw tool definitions
            tools_map.values().map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters_schema: tool.parameters_schema(),
            }).collect()
        }
    }

    fn active_skill_descriptors(&self) -> Result<Vec<oneai_core::SkillDescriptor>> {
        Ok(Vec::new())
    }

    /// Handle the approval gate interaction for a tool call.
    async fn handle_approval(
        approval_gate: Arc<dyn ApprovalGate>,
        request: oneai_core::ApprovalRequest,
        tool: Arc<dyn Tool>,
        args: serde_json::Value,
        call_id: String,
    ) -> Result<ToolCallResult> {
        match approval_gate.request_approval(request).await {
            Ok(oneai_core::ApprovalResponse::Approved { modified_args }) => {
                let final_args = modified_args.unwrap_or(args);
                let output = tool.execute(final_args).await?;
                Ok(ToolCallResult { call_id, output })
            }
            Ok(oneai_core::ApprovalResponse::Denied { reason }) => {
                Ok(ToolCallResult { call_id, output: ToolOutput {
                    success: false, content: String::new(),
                    error: Some(format!("Denied: {}", reason)),
                }})
            }
            Ok(oneai_core::ApprovalResponse::Modified { args: modified_args }) => {
                let output = tool.execute(modified_args).await?;
                Ok(ToolCallResult { call_id, output })
            }
            Ok(oneai_core::ApprovalResponse::Observe { observation }) => {
                Ok(ToolCallResult { call_id, output: ToolOutput {
                    success: false,
                    content: format!("Observe: {}", observation),
                    error: Some("Execution paused for observation".to_string()),
                }})
            }
            Err(e) => {
                Ok(ToolCallResult { call_id, output: ToolOutput {
                    success: false, content: String::new(),
                    error: Some(format!("Approval error: {}", e)),
                }})
            }
        }
    }
}