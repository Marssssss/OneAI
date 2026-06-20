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
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;

use oneai_core::{
    ContentBlock, Conversation, InferenceRequest, InferenceResponse,
    Message, Role, ToolDefinition, ToolOutput,
    HookPoint, HookContext, InterruptPoint, InterruptReason,
    ResumeSignal, ResumeAction, StructuredOutputConfig,
};
use oneai_core::error::Result;
use oneai_core::traits::{ApprovalGate, LlmProvider, OutputParser, Tool};

use oneai_domain::{MergedDomainPack, PermissionAction};

use crate::sub_agent::{SubAgentFactory, SubAgentKind, SubAgentSummary};
use crate::context_assembler::ContextAssembler;
use crate::streaming::IncrementalStreamParser;
use crate::hooks::{HookRegistry, ResolvedHookAction};
use crate::structured_output::{validate_json_schema, build_retry_prompt};
use oneai_trace::{TraceContext, SpanKind, SpanStatus, EventKind};

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

    /// Called after assembling the context for each iteration, with a breakdown
    /// of how the context window is occupied. This includes the full assembled
    /// conversation (system prompt, tool defs, context sources, messages),
    /// not just the bare session conversation.
    fn on_context_accounting(&self, _accounting: &oneai_core::ContextAccounting) {}

    /// Called when the loop is interrupted (paused at an iteration boundary).
    /// The UI can display the interrupt reason and await human feedback.
    fn on_interrupt(&self, _point: &InterruptPoint) {}

    /// Called when the loop resumes from an interrupt with human feedback.
    fn on_resume(&self, _signal: &ResumeSignal) {}
}

// ─── AgentDecision ──────────────────────────────────────────────────────────

/// The decision type produced by parsing the model's output each loop iteration.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum ParadigmKind {
    Plan,
    ReAct,
    Reflect,
    Explore,
}

// ─── ParadigmConfig ──────────────────────────────────────────────────────────

/// Configuration for a specific paradigm — defines how the agent behaves
/// when this paradigm is active.
///
/// Each paradigm changes three things:
/// 1. **System prompt**: A paradigm-specific prompt that defines the agent's
///    role and behavioral constraints for this mode.
/// 2. **Tool filter**: The set of tools available in this paradigm.
///    Plan paradigm doesn't need execution tools; Explore doesn't need edit tools.
/// 3. **Decision hint**: A brief description injected into context telling the
///    model what kind of decisions to make in this paradigm.
///
/// This addresses the "范式切换语义空洞" gap — previously, `run_paradigm()`
/// just returned a text string like "Plan paradigm activated" without any
/// actual behavioral change. Now, paradigm switching produces real, observable
/// effects: system prompt changes, tool filtering, and decision guidance.
///
/// Inspired by Aider's Architect/Editor dual-model pattern where each "role"
/// has its own prompt and tool set. OneAI extends this to 4 paradigms.
#[derive(Debug, Clone)]
pub struct ParadigmConfig {
    /// The paradigm this config applies to.
    pub paradigm: ParadigmKind,

    /// System prompt for this paradigm — replaces the default system prompt
    /// when this paradigm is active.
    pub system_prompt: String,

    /// Tools available in this paradigm — only these tools are sent to the
    /// model as tool definitions. Other tools are hidden from the model.
    pub tool_filter: Vec<String>,

    /// Decision hint — injected into context as a system message when
    /// this paradigm becomes active. Tells the model what kind of
    /// decisions to make (plan vs execute vs review vs explore).
    pub decision_hint: String,
}

impl ParadigmConfig {
    /// Get the default configuration for each paradigm kind.
    ///
    /// These defaults are modeled after Aider's Architect/Editor pattern:
    /// - Plan: No execution tools, focus on decomposition
    /// - ReAct: Full tool set, focus on action
    /// - Reflect: Read-only tools, focus on review
    /// - Explore: Read + search tools, focus on discovery
    pub fn defaults() -> Vec<ParadigmConfig> {
        vec![
            ParadigmConfig {
                paradigm: ParadigmKind::Plan,
                system_prompt: "You are a planning agent. Your ONLY job is to decompose the given \
                    task into a structured plan with ordered steps and dependencies. \
                    Do NOT execute any tools — produce only a plan as a numbered list. \
                    Each step should be specific, actionable, and identify which tool would be needed. \
                    Focus on: understanding the task scope, identifying dependencies, \
                    ordering steps logically, and flagging risks or unknowns."
                    .to_string(),
                tool_filter: vec![
                    "read_file".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "environment".into(),
                ],
                decision_hint: "You are in PLAN mode — focus on decomposing the task into ordered steps. \
                    Do NOT execute any tools. Produce only a plan.".to_string(),
            },
            ParadigmConfig {
                paradigm: ParadigmKind::ReAct,
                system_prompt: "You are a ReAct agent — you reason about what to do, then act using \
                    available tools, observe the results, and iterate. This is the default \
                    execution mode. Use tools to accomplish the task, and report the final \
                    answer when done. If you encounter errors, try to fix them in subsequent iterations. \
                    Focus on: executing actions efficiently, verifying results, and iterating until complete."
                    .to_string(),
                tool_filter: vec![
                    "read_file".into(), "edit_file".into(), "apply_patch".into(),
                    "shell".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "environment".into(),
                    "web_fetch".into(), "notebook_edit".into(),
                ],
                decision_hint: "You are in REACT mode — reason about what to do, then act using tools, \
                    observe results, and iterate.".to_string(),
            },
            ParadigmConfig {
                paradigm: ParadigmKind::Reflect,
                system_prompt: "You are a reflection agent. Your job is to review the current state \
                    of work, identify errors, improvements, and missing steps. You have \
                    read-only access — you can examine files and search the codebase, but \
                    you cannot make changes. Your output should be a structured review \
                    with: (1) issues found, (2) improvements suggested, (3) next steps recommended. \
                    Focus on: correctness, completeness, quality, and potential risks."
                    .to_string(),
                tool_filter: vec![
                    "read_file".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "environment".into(),
                ],
                decision_hint: "You are in REFLECT mode — review the current state, identify errors \
                    and improvements, and suggest next steps.".to_string(),
            },
            ParadigmConfig {
                paradigm: ParadigmKind::Explore,
                system_prompt: "You are an exploration agent. Your job is to search and understand \
                    the codebase/environment. You can read files, search patterns, and list \
                    directories, but you cannot modify anything. Return a comprehensive \
                    summary of your findings including: file paths, function signatures, \
                    key patterns, relevant dependencies, and any important observations. \
                    Focus on: thoroughness, accuracy, and providing useful context for \
                    subsequent planning or execution."
                    .to_string(),
                tool_filter: vec![
                    "read_file".into(), "grep".into(), "glob".into(),
                    "list_directory".into(), "environment".into(),
                    "web_fetch".into(),
                ],
                decision_hint: "You are in EXPLORE mode — search and understand the environment. \
                    Report findings without modifying anything.".to_string(),
            },
        ]
    }

    /// Get the ParadigmConfig for a specific paradigm kind from the defaults.
    pub fn for_paradigm(kind: ParadigmKind) -> ParadigmConfig {
        Self::defaults().into_iter()
            .find(|c| c.paradigm == kind)
            .unwrap_or_else(|| ParadigmConfig {
                paradigm: kind,
                system_prompt: String::new(),
                tool_filter: vec![], // Empty filter = all tools available
                decision_hint: String::new(),
            })
    }
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
    /// The tool name — used by TUI observer to identify which tool produced this result,
    /// enabling it to find and update the corresponding ToolInvocation message.
    pub tool_name: String,
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
    /// The active paradigm configuration — determines system prompt,
    /// tool filter, and decision hint for the current paradigm.
    /// Updated when paradigm switching occurs.
    pub active_paradigm_config: Option<ParadigmConfig>,
    pub sub_agent_results: Vec<SubAgentSummary>,
    pub env_snapshot: Option<crate::context_assembler::EnvironmentSnapshot>,
    /// Interrupt points accumulated during the loop.
    /// Each interrupt represents a pause point where human feedback was requested.
    pub interrupt_points: Vec<InterruptPoint>,
    /// The current pending interrupt (if the loop is paused).
    /// When set, the loop will break at the next iteration boundary.
    pub pending_interrupt: Option<InterruptPoint>,
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
            active_paradigm_config: None, // Uses default system prompt until switch
            sub_agent_results: Vec::new(),
            env_snapshot: None,
            interrupt_points: Vec::new(),
            pending_interrupt: None,
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
            active_paradigm_config: None,
            sub_agent_results: Vec::new(),
            env_snapshot: None,
            interrupt_points: Vec::new(),
            pending_interrupt: None,
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

#[derive(Clone)]
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
    /// Cost tracker — records usage after each inference call.
    /// When set, the loop automatically records token usage and cost.
    pub cost_tracker: Option<Arc<dyn oneai_core::CostTracker>>,
    /// Rate limiter — checks rate before each provider call.
    /// When set, the loop waits if the rate limit is exceeded.
    pub rate_limiter: Option<Arc<dyn oneai_core::RateLimiter>>,
    /// Circuit breaker — provider failover on repeated failures.
    /// When set, the loop skips calls to providers with open circuits.
    pub circuit_breaker: Option<Arc<dyn oneai_core::CircuitBreaker>>,
    /// Model pricing catalog — per-model cost computation.
    /// When set, cost tracking uses this catalog for accurate pricing.
    /// When None, uses the `pricing` field (backward-compatible).
    pub pricing_catalog: Option<oneai_core::ModelPricingCatalog>,
    /// Structured output configuration — when set, the model's final
    /// answer is validated against a JSON Schema. If validation fails,
    /// the model is re-prompted with the error for self-correction (ModelRetry).
    pub structured_output: Option<StructuredOutputConfig>,
    /// Trace context for observability — when set, spans and events are
    /// emitted at key lifecycle points (iteration, inference, tool call,
    /// paradigm switch, delegation, approval). When None, tracing is
    /// completely disabled (zero overhead).
    pub trace_context: Option<TraceContext>,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("system_prompt", &self.system_prompt)
            .field("use_streaming", &self.use_streaming)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("thinking_budget", &self.thinking_budget)
            .field("hard_max_iterations", &self.hard_max_iterations)
            .field("auto_checkpoint", &self.auto_checkpoint)
            .field("inject_skills", &self.inject_skills)
            .field("detect_env_changes", &self.detect_env_changes)
            .field("pricing", &self.pricing)
            .field("cost_tracker", &self.cost_tracker.as_ref().map(|_| "Arc<dyn CostTracker>"))
            .field("rate_limiter", &self.rate_limiter.as_ref().map(|_| "Arc<dyn RateLimiter>"))
            .field("circuit_breaker", &self.circuit_breaker.as_ref().map(|_| "Arc<dyn CircuitBreaker>"))
            .field("pricing_catalog", &self.pricing_catalog)
            .field("structured_output", &self.structured_output)
            .field("trace_context", &self.trace_context)
            .finish()
    }
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            system_prompt: "You are an intelligent AI agent that can plan, execute, and reflect on tasks. \
                When you need to use a tool, output a tool call. When you have the final answer, \
                respond with just text without any tool calls. \
                When a task is complex, you can delegate it to a specialized sub-agent or switch to a planning paradigm.\n\n\
                **Tool Preference Rules** (IMPORTANT — always follow these):\n\
                - For reading files: use read_file (NOT shell cat/head/tail)\n\
                - For editing files: use edit_file (NOT shell sed/awk)\n\
                - For creating/writing files: use file_write (NOT shell echo/tee)\n\
                - For listing directories: use list_directory (NOT shell ls)\n\
                - For searching content: use grep (NOT shell grep/find)\n\
                - For finding files: use glob (NOT shell find)\n\
                - Use shell ONLY for: compilation, testing, git operations, package management, \
                  running scripts, or commands that have no dedicated tool equivalent\n\
                - This ensures safer, more precise, and more readable operations"
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
            cost_tracker: None,
            rate_limiter: None,
            circuit_breaker: None,
            pricing_catalog: None,
            structured_output: None,
            trace_context: None,
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
    /// Optional async task runner for parallel sub-agent delegation.
    /// When enabled, the AgentLoop can submit sub-agent tasks to the
    /// runner for background execution, continuing work while sub-agents
    /// run independently. The runner uses the same SubAgentFactory as
    /// serial delegation, ensuring consistent sub-agent creation.
    /// If None, all delegation is serial (spawn_sub_agent → wait).
    async_task_runner: Option<Arc<crate::async_task_runner::AsyncTaskRunner>>,
    context_assembler: Arc<tokio::sync::RwLock<ContextAssembler>>,
    stream_parser: Arc<tokio::sync::RwLock<IncrementalStreamParser>>,
    checkpoint_manager: Option<Arc<oneai_persistence::ProgressiveCheckpointManager>>,
    recovery_manager: Option<Arc<crate::error_recovery::RecoveryManager>>,
    hook_registry: Arc<tokio::sync::RwLock<HookRegistry>>,
    interrupt_requested: Arc<AtomicBool>,
    interrupt_reason: Arc<tokio::sync::Mutex<Option<InterruptReason>>>,
    config: AgentLoopConfig,
    domain_pack: Option<Arc<MergedDomainPack>>,
}

/// Manual Clone implementation for AgentLoop — all fields are Arc/RwLock/Arc<RwLock>,
/// so cloning is cheap (just Arc pointer cloning, no data duplication).
///
/// This enables SubAgentWrapper to clone the AgentLoop for tokio::spawn,
/// allowing sub-agents to run on separate async tasks independently.
impl Clone for AgentLoop {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            parser: self.parser.clone(),
            approval_gate: self.approval_gate.clone(),
            skill_selector: self.skill_selector.clone(),
            context_budget: self.context_budget.clone(),
            sub_agent_factory: self.sub_agent_factory.clone(),
            async_task_runner: self.async_task_runner.clone(),
            context_assembler: self.context_assembler.clone(),
            stream_parser: self.stream_parser.clone(),
            checkpoint_manager: self.checkpoint_manager.clone(),
            recovery_manager: self.recovery_manager.clone(),
            hook_registry: self.hook_registry.clone(),
            interrupt_requested: self.interrupt_requested.clone(),
            interrupt_reason: self.interrupt_reason.clone(),
            config: self.config.clone(),
            domain_pack: self.domain_pack.clone(),
        }
    }
}

impl AgentLoop {
    /// Get the provider name for cost tracking.
    fn provider_name(&self) -> String {
        let config = self.provider.config();
        match config.cloud_kind {
            Some(oneai_core::CloudProviderKind::OpenAI) => "openai".to_string(),
            Some(oneai_core::CloudProviderKind::Anthropic) => "anthropic".to_string(),
            Some(oneai_core::CloudProviderKind::Gemini) => "gemini".to_string(),
            None => match config.provider_type {
                oneai_core::ProviderType::Local => "ollama".to_string(),
                oneai_core::ProviderType::Transformers => "local".to_string(),
                oneai_core::ProviderType::Cloud => "cloud".to_string(),
            },
        }
    }

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
            sub_agent_factory, async_task_runner: None,
            context_assembler: Arc::new(tokio::sync::RwLock::new(context_assembler)),
            stream_parser: Arc::new(tokio::sync::RwLock::new(stream_parser)), checkpoint_manager, recovery_manager: None,
            hook_registry: Arc::new(tokio::sync::RwLock::new(HookRegistry::new())),
            interrupt_requested: Arc::new(AtomicBool::new(false)),
            interrupt_reason: Arc::new(tokio::sync::Mutex::new(None)),
            config, domain_pack: None }
    }

    /// Create a new AgentLoop with a domain pack and recovery manager.
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
            sub_agent_factory, async_task_runner: None,
            context_assembler: Arc::new(tokio::sync::RwLock::new(context_assembler)),
            stream_parser: Arc::new(tokio::sync::RwLock::new(stream_parser)), checkpoint_manager, recovery_manager: None,
            hook_registry: Arc::new(tokio::sync::RwLock::new(HookRegistry::new())),
            interrupt_requested: Arc::new(AtomicBool::new(false)),
            interrupt_reason: Arc::new(tokio::sync::Mutex::new(None)),
            config,
            domain_pack: Some(domain_pack) }
    }

    /// Enable parallel sub-agent delegation with the AsyncTaskRunner.
    ///
    /// When enabled, the AgentLoop can submit sub-agent tasks to the
    /// runner for background execution. The main loop continues working
    /// while sub-agents run independently, and results are collected
    /// when the sub-agents complete.
    ///
    /// The runner uses the same SubAgentFactory as serial delegation,
    /// ensuring consistent sub-agent creation across both modes.
    ///
    /// **Usage**: Call this after creating the AgentLoop, before running it.
    /// ```ignore
    /// let agent_loop = AgentLoop::new(...).with_parallel_delegation();
    /// ```
    pub fn with_parallel_delegation(self) -> Self {
        let runner = Arc::new(crate::async_task_runner::AsyncTaskRunner::new(
            self.sub_agent_factory.clone(),
        ));
        Self {
            async_task_runner: Some(runner),
            ..self
        }
    }

    /// Enable parallel sub-agent delegation with a custom budget.
    ///
    /// The custom budget applies to all background sub-agent tasks.
    pub fn with_parallel_delegation_and_budget(self, budget: oneai_core::budget::TokenBudget) -> Self {
        let runner = Arc::new(crate::async_task_runner::AsyncTaskRunner::with_budget(
            self.sub_agent_factory.clone(),
            budget,
        ));
        Self {
            async_task_runner: Some(runner),
            ..self
        }
    }

    /// Set the RecoveryManager for error recovery during the loop.
    ///
    /// When set, failed tool calls trigger recovery strategy evaluation.
    /// The RecoveryManager can apply Retry, ConditionalFallback, Rollback,
    /// ExternalFeedback, or Escalate strategies based on the error type.
    pub fn with_recovery_manager(mut self, manager: Arc<crate::error_recovery::RecoveryManager>) -> Self {
        self.recovery_manager = Some(manager);
        self
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
        // Track structured output retry count (separate from iteration count)
        let mut structured_retry_count: usize = 0;
        // Track consecutive rate limit errors — after too many, terminate the loop
        // with a clear message instead of infinitely retrying.
        let mut consecutive_rate_limit_errors: usize = 0;
        const MAX_CONSECUTIVE_RATE_LIMIT_ERRORS: usize = 10;

        // ─── Trace: start AGENT span for the entire loop ──────────────
        let loop_span_id = if let Some(ctx) = &self.config.trace_context {
            let span_id = ctx.enter_span(SpanKind::AGENT, "agent_loop", None);
            ctx.set_attribute("agent.task", serde_json::json!(state.original_task));
            ctx.set_attribute("agent.paradigm", serde_json::json!(paradigm_name(&state.active_paradigm)));
            span_id
        } else {
            String::new()
        };

        while !state.is_complete() && state.iterations < self.config.hard_max_iterations.unwrap_or(usize::MAX) {
            // ─── Check for external interrupt request ──────────────────────
            if self.interrupt_requested.load(Ordering::Relaxed) {
                self.interrupt_requested.store(false, Ordering::Relaxed);
                let reason = self.interrupt_reason.lock().await.take();
                let interrupt_point = InterruptPoint {
                    id: uuid::Uuid::new_v4().to_string(),
                    iteration: state.iterations,
                    reason: reason.unwrap_or(InterruptReason::Custom {
                        reason: "External interrupt requested".to_string(),
                    }),
                    checkpoint_id: None,
                };
                state.interrupt_points.push(interrupt_point.clone());
                state.pending_interrupt = Some(interrupt_point.clone());
                observer.on_interrupt(&interrupt_point);

                // Save checkpoint if checkpointing is enabled
                if self.config.auto_checkpoint {
                    self.auto_checkpoint(&state, state.iterations).await?;
                }

                // Return partial result — the loop is paused for human feedback
                let result = state.into_result();
                observer.on_complete(&result);
                return Ok(result);
            }

            state.iterations += 1;

            tracing::info!(
                "AgentLoop iteration {} started (paradigm: {}, messages: {}, is_complete: {})",
                state.iterations,
                paradigm_name(&state.active_paradigm),
                state.conversation.messages.len(),
                state.is_complete()
            );

            // ─── Rate limiter check (wait if rate limit exceeded) ────────────
            if let Some(rate_limiter) = &self.config.rate_limiter {
                let provider_name = self.provider_name();
                let wait_time = rate_limiter.wait_if_needed(&provider_name).await.unwrap_or(std::time::Duration::ZERO);
                if wait_time > std::time::Duration::ZERO {
                    tracing::warn!("Rate limit exceeded for provider {}, waiting {}ms",
                        provider_name, wait_time.as_millis());
                    tokio::time::sleep(wait_time).await;
                }
                let _ = rate_limiter.record_call(&provider_name).await;
            }

            // ─── Circuit breaker check (skip if provider is failing) ─────────
            if let Some(circuit_breaker) = &self.config.circuit_breaker {
                let provider_name = self.provider_name();
                let circuit_state = circuit_breaker.check(&provider_name);
                if circuit_state.is_failing() {
                    tracing::warn!("Circuit breaker is OPEN for provider {}, skipping call",
                        provider_name);
                    // Skip this iteration — the loop will continue and may exit
                    // on hard_max_iterations if all calls are blocked
                    continue;
                }
            }

            // ─── Budget enforcement check ─────────────────────────────────────
            if let Some(cost_tracker) = &self.config.cost_tracker {
                let budget_status = cost_tracker.check_budget(&state.conversation.id).await.unwrap_or(oneai_core::BudgetStatus::unlimited(state.iterations as u64));
                if budget_status.budget_exceeded {
                    tracing::warn!("Budget exceeded for session {} — terminating loop",
                        state.conversation.id);
                    observer.on_cost_update(cumulative_cost);
                    let result = state.into_result();
                    observer.on_complete(&result);
                    return Ok(result);
                }
            }
            observer.on_iteration_start(state.iterations, state.active_paradigm);

            // ─── Trace: log iteration event ──────────────────────────
            if let Some(ctx) = &self.config.trace_context {
                ctx.log_event(EventKind::WorkflowStepStart, "agent.iteration", HashMap::from([
                    ("agent.iteration".to_string(), serde_json::json!(state.iterations)),
                    ("agent.paradigm".to_string(), serde_json::json!(paradigm_name(&state.active_paradigm))),
                ]));
            }

            // 1. Refresh domain context sources + assemble context
            {
                let mut ca = self.context_assembler.write().await;
                ca.refresh_sources().await?;
            }

            // 1b. Context Epoch — take environment snapshot for diff detection.
            // This addresses the "Context Epoch 未接入 Loop" gap.
            // take_snapshot() and compute_diff() already exist in ContextAssembler,
            // but were never called from the loop. Now, each iteration takes a
            // snapshot, and the assembler injects the diff into context on the
            // next iteration (when last_snapshot differs from the current one).
            if self.config.detect_env_changes {
                let tools_map = self.tools.read().await;
                let tool_names: std::collections::HashSet<String> = tools_map.keys().cloned().collect();
                let snapshot = {
                    let ca = self.context_assembler.read().await;
                    ca.take_snapshot(&tool_names).await?
                };
                // Update the assembler's last_snapshot for next iteration's diff
                {
                    let mut ca = self.context_assembler.write().await;
                    ca.update_snapshot(snapshot.clone());
                }
                state.env_snapshot = Some(snapshot);
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

            // 3. Build inference request (with paradigm-aware tool definitions)
            let tool_defs = self.build_tool_definitions_for_paradigm(
                state.active_paradigm_config.as_ref()
            ).await;
            let mut request = InferenceRequest {
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

            // 3b. PreInfer hook — lifecycle hooks can modify the inference request
            // before it's sent to the LLM (e.g., inject context, filter tools).
            {
                let registry = self.hook_registry.read().await;
                if registry.count_at(&HookPoint::PreInfer) > 0 {
                    let hook_context = HookContext {
                        point: HookPoint::PreInfer,
                        tool_name: None,
                        tool_args: None,
                        tool_output: None,
                        inference_request: Some(request.clone()),
                        inference_response: None,
                        iteration: state.iterations,
                        paradigm: paradigm_name(&state.active_paradigm).to_string(),
                    };
                    let results = registry.run_hooks(HookPoint::PreInfer, hook_context).await;
                    let resolved = HookRegistry::resolve_results(&results);
                    if let ResolvedHookAction::Modify { modified_args } = resolved {
                        // Modified args may contain a modified conversation or extra context
                        if let Some(extra_msg) = modified_args.get("inject_system_message").and_then(|v| v.as_str()) {
                            state.conversation.add_message(Message::system(extra_msg.to_string()));
                            request.conversation = state.conversation.clone();
                        }
                    } else if let ResolvedHookAction::Deny { reason } = resolved {
                        // PreInfer Deny: skip this inference iteration
                        state.conversation.add_message(Message::system(
                            format!("Inference skipped by PreInfer hook: {}", reason)
                        ));
                        continue;
                    }
                }
            }

            // 3c. Compute context accounting from the assembled request
            // This uses HeuristicTokenCounter on the full assembled conversation + tool defs,
            // giving accurate per-category breakdown that the sidebar and /context command can display.
            //
            // IMPORTANT: Use the actual model name from provider config (e.g., "glm-5.1")
            // not the provider type name (e.g., "openai"). The model name determines:
            // - Context window size (glm-5.1 → 203K, gpt-4o → 200K, etc.)
            // - Tokenizer profile (chars-per-token ratios, overhead values)
            // - Provider-specific estimation parameters
            let model_name_for_accounting = self.provider.config().model_name
                .as_deref()
                .unwrap_or("default");
            let accounting = oneai_core::ContextAccounting::account(
                &request.conversation,
                &model_name_for_accounting,
                request.tools.len(),
            );
            observer.on_context_accounting(&accounting);

            // 4. Run inference
            // ─── Trace: start LLM span for inference ──────────────────
            let infer_span_id = if let Some(ctx) = &self.config.trace_context {
                let span_id = ctx.enter_span(SpanKind::LLM, "inference", None);
                ctx.log_event(EventKind::InferenceStart, "llm.inference.start", HashMap::from([
                    ("agent.iteration".to_string(), serde_json::json!(state.iterations)),
                ]));
                span_id
            } else {
                String::new()
            };

            // Handle RateLimit errors gracefully — don't terminate the loop,
            // just wait and retry. This handles cases where provider-level retry
            // (ProviderRetryConfig) was exhausted but the rate limit might clear
            // after waiting longer.
            let response_result = if self.config.use_streaming {
                self.run_streaming_iteration_async(&request, observer).await
            } else {
                self.provider.infer(request).await
            };

            let response = match response_result {
                Ok(resp) => {
                    // Successful inference — reset consecutive rate limit counter
                    consecutive_rate_limit_errors = 0;
                    resp
                }
                Err(oneai_core::error::OneAIError::RateLimit(msg)) => {
                    consecutive_rate_limit_errors += 1;

                    // ─── Trace: record rate limit error ──────────────
                    if let Some(ctx) = &self.config.trace_context {
                        if !infer_span_id.is_empty() {
                            ctx.log_event_in_span(&infer_span_id, EventKind::Error, "llm.rate_limit", HashMap::from([
                                ("error.message".to_string(), serde_json::json!(msg)),
                                ("error.consecutive_count".to_string(), serde_json::json!(consecutive_rate_limit_errors)),
                            ]));
                            ctx.exit_span(&infer_span_id, SpanStatus::Error);
                        }
                    }

                    if consecutive_rate_limit_errors >= MAX_CONSECUTIVE_RATE_LIMIT_ERRORS {
                        tracing::error!(
                            "AgentLoop: {} consecutive rate limit errors — terminating loop. Last error: {}",
                            consecutive_rate_limit_errors, msg
                        );
                        observer.on_interrupt(&InterruptPoint {
                            id: uuid::Uuid::new_v4().to_string(),
                            iteration: state.iterations,
                            reason: InterruptReason::Custom {
                                reason: format!("Rate limit exceeded after {} consecutive failures: {}", consecutive_rate_limit_errors, msg),
                            },
                            checkpoint_id: None,
                        });
                        // Return partial result with error info
                        state.conversation.add_message(Message::assistant(
                            format!("[Rate limit exceeded]: {}", msg)
                        ));
                        let result = state.into_result();
                        observer.on_complete(&result);
                        return Ok(result);
                    }

                    tracing::warn!(
                        "AgentLoop iteration {}: Rate limit error (consecutive: {}/{}), waiting 5s before retry. Error: {}",
                        state.iterations,
                        consecutive_rate_limit_errors,
                        MAX_CONSECUTIVE_RATE_LIMIT_ERRORS,
                        msg
                    );

                    // Wait 5 seconds before retrying — longer than provider-level backoff
                    // since this is the agent-level fallback after provider retry was exhausted
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

                    // Don't count this as a real iteration — decrement and continue
                    state.iterations -= 1;
                    continue;
                }
                Err(other_err) => {
                    // ─── Trace: record non-rate-limit error ──────────────
                    if let Some(ctx) = &self.config.trace_context {
                        if !infer_span_id.is_empty() {
                            ctx.log_event_in_span(&infer_span_id, EventKind::Error, "llm.error", HashMap::from([
                                ("error.message".to_string(), serde_json::json!(other_err.to_string())),
                            ]));
                            ctx.exit_span(&infer_span_id, SpanStatus::Error);
                        }
                    }

                    // Other errors — propagate as before (terminates the loop)
                    return Err(other_err);
                }
            };

            // ─── Trace: end LLM span and log token usage ────────────
            if let Some(ctx) = &self.config.trace_context {
                if !infer_span_id.is_empty() {
                    ctx.log_event_in_span(&infer_span_id, EventKind::InferenceEnd, "llm.inference.end", HashMap::from([
                        ("llm.prompt_tokens".to_string(), serde_json::json!(response.usage.prompt_tokens)),
                        ("llm.completion_tokens".to_string(), serde_json::json!(response.usage.completion_tokens)),
                        ("llm.total_tokens".to_string(), serde_json::json!(response.usage.prompt_tokens + response.usage.completion_tokens)),
                    ]));
                    ctx.exit_span(&infer_span_id, SpanStatus::Ok);
                }
            }

            // 4c. PostInfer hook — lifecycle hooks can modify the inference response
            // after it's received from the LLM (e.g., filter content, validate).
            {
                let registry = self.hook_registry.read().await;
                if registry.count_at(&HookPoint::PostInfer) > 0 {
                    let hook_context = HookContext {
                        point: HookPoint::PostInfer,
                        tool_name: None,
                        tool_args: None,
                        tool_output: None,
                        inference_request: None,
                        inference_response: Some(response.clone()),
                        iteration: state.iterations,
                        paradigm: paradigm_name(&state.active_paradigm).to_string(),
                    };
                    let results = registry.run_hooks(HookPoint::PostInfer, hook_context).await;
                    let resolved = HookRegistry::resolve_results(&results);
                    if let ResolvedHookAction::Modify { modified_args: _ } = resolved {
                        // PostInfer Modify: the modified_args may contain replacement content
                        // For now, we log it but don't replace the response (to keep backward compat)
                        tracing::info!("PostInfer hook modified response (logged but not applied for safety)");
                    } else if let ResolvedHookAction::Deny { reason } = resolved {
                        // PostInfer Deny: treat as "model produced disallowed content"
                        // Replace the response with a safe fallback
                        tracing::warn!("PostInfer hook denied response: {}", reason);
                    }
                }
            }

            // 4b. Notify observer of token usage and cost
            observer.on_token_usage(response.usage.prompt_tokens, response.usage.completion_tokens);

            // Use pricing catalog if available, otherwise fall back to ModelPricing
            let iteration_cost = if let Some(catalog) = &self.config.pricing_catalog {
                catalog.compute_cost(&response.model, response.usage.prompt_tokens, response.usage.completion_tokens)
            } else {
                self.config.pricing.compute_cost(
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                )
            };
            cumulative_cost += iteration_cost;
            observer.on_cost_update(cumulative_cost);

            // 4c. Record usage in cost tracker (if configured)
            if let Some(cost_tracker) = &self.config.cost_tracker {
                let session_id = state.conversation.id.clone();
                let provider_name = self.provider_name();
                let record = oneai_core::UsageRecord::new(
                    session_id,
                    response.model.clone(),
                    provider_name,
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    iteration_cost,
                );
                let _ = cost_tracker.record_usage(record).await;
            }

            // 4d. Record circuit breaker success (if configured)
            if let Some(circuit_breaker) = &self.config.circuit_breaker {
                circuit_breaker.record_success(&self.provider_name());
            }

            // 5. Parse decision
            let mut decision = self.parse_decision(&response)?;

            tracing::info!(
                "AgentLoop iteration {} decision: {} (content_blocks: {}, text_length: {}, tool_calls: {})",
                state.iterations,
                match &decision {
                    AgentDecision::DirectAnswer { .. } => "DirectAnswer".to_string(),
                    AgentDecision::ToolCalls { calls } => format!("ToolCalls({} calls)", calls.len()),
                    AgentDecision::Delegate { .. } => "Delegate".to_string(),
                    AgentDecision::SwitchParadigm { .. } => "SwitchParadigm".to_string(),
                },
                response.message.content.len(),
                response.message.text_content().len(),
                response.message.content.iter().filter(|b| matches!(b, ContentBlock::ToolCall { .. })).count(),
            );

            // 5b. Empty response retry — if the model produced 0 content blocks,
            // inject a clarification prompt and retry once. This handles:
            // 1) SSE format incompatibility (model returns data we can't parse)
            // 2) Model genuinely failing to respond (confused by context format)
            // 3) Streaming response that was empty/malformed
            //
            // The retry injects a follow-up message asking the model to respond,
            // giving it a second chance with a clearer prompt.
            const MAX_EMPTY_RETRIES: usize = 1;
            let mut empty_retry_count: usize = 0;
            while matches!(&decision, AgentDecision::DirectAnswer { text } if text.trim().is_empty())
                && empty_retry_count < MAX_EMPTY_RETRIES
            {
                empty_retry_count += 1;
                tracing::warn!(
                    "AgentLoop iteration {}: model produced empty response, retrying ({}/{}). \
                    This usually means the model didn't properly see the context or the \
                    streaming format caused parsing issues. Conversation has {} messages.",
                    state.iterations,
                    empty_retry_count,
                    MAX_EMPTY_RETRIES,
                    state.conversation.messages.len()
                );

                // Inject follow-up messages asking model to respond.
                // We add an empty assistant message (representing the model's
                // failed response) followed by a user message explicitly asking
                // for a response. This preserves OpenAI API format validity.
                state.conversation.add_message(Message {
                    role: Role::Assistant,
                    content: vec![],  // Empty assistant response
                    metadata: HashMap::new(),
                });
                state.conversation.add_message(Message::user(
                    "You did not respond in the previous turn. Please provide a response now — \
                    either call a tool to accomplish the task, or give a direct answer.".to_string()
                ));

                // Re-build inference request with updated conversation
                let retry_tool_defs = self.build_tool_definitions_for_paradigm(
                    state.active_paradigm_config.as_ref()
                ).await;
                let retry_request = InferenceRequest {
                    conversation: state.conversation.clone(),
                    tools: retry_tool_defs,
                    max_tokens: self.config.max_tokens,
                    temperature: self.config.temperature,
                    top_p: None,
                    stop_sequences: vec![],
                    constrained_output: None,
                    thinking_budget: self.config.thinking_budget,
                    metadata: HashMap::new(),
                };

                // Re-run inference with the follow-up prompt
                let retry_response = if self.config.use_streaming {
                    self.run_streaming_iteration_async(&retry_request, observer).await?
                } else {
                    self.provider.infer(retry_request).await?
                };

                // Notify observer of retry token usage
                observer.on_token_usage(retry_response.usage.prompt_tokens, retry_response.usage.completion_tokens);
                let retry_cost = if let Some(catalog) = &self.config.pricing_catalog {
                    catalog.compute_cost(&retry_response.model, retry_response.usage.prompt_tokens, retry_response.usage.completion_tokens)
                } else {
                    self.config.pricing.compute_cost(
                        retry_response.usage.prompt_tokens,
                        retry_response.usage.completion_tokens,
                    )
                };
                cumulative_cost += retry_cost;
                observer.on_cost_update(cumulative_cost);

                decision = self.parse_decision(&retry_response)?;

                tracing::info!(
                    "AgentLoop iteration {}: empty response retry {} produced decision: {} (content_blocks: {})",
                    state.iterations,
                    empty_retry_count,
                    match &decision {
                        AgentDecision::DirectAnswer { .. } => "DirectAnswer".to_string(),
                        AgentDecision::ToolCalls { calls } => format!("ToolCalls({} calls)", calls.len()),
                        AgentDecision::Delegate { .. } => "Delegate".to_string(),
                        AgentDecision::SwitchParadigm { .. } => "SwitchParadigm".to_string(),
                    },
                    retry_response.message.content.len(),
                );
            }

            // 5c. If retry also produced empty DirectAnswer, log and continue
            // (the loop will still end with an empty answer, but at least we tried)
            if matches!(&decision, AgentDecision::DirectAnswer { text } if text.trim().is_empty()) {
                tracing::warn!(
                    "AgentLoop iteration {}: model still produced empty DirectAnswer after retry. \
                    Giving up — loop will end with empty answer. Conversation has {} messages.",
                    state.iterations,
                    state.conversation.messages.len()
                );
            }

            // 6. Execute decision + notify observer
            // IMPORTANT: The assistant's response (containing tool calls, delegation, etc.)
            // MUST be added to the conversation BEFORE any tool results, so that the
            // OpenAI/Anthropic API format is valid: assistant message with tool_calls
            // precedes tool result messages that reference those call_ids.
            match decision {
                AgentDecision::DirectAnswer { text } => {
                    observer.on_direct_answer(&text);

                    // ─── Trace: log DirectAnswer event ──────────────
                    if let Some(ctx) = &self.config.trace_context {
                        ctx.log_event(EventKind::Thought, "agent.direct_answer", HashMap::from([
                            ("agent.answer_length".to_string(), serde_json::json!(text.len())),
                        ]));
                    }

                    // ─── Structured output validation ──────────────────────────
                    // If StructuredOutputConfig is set, validate the model's final
                    // answer against the JSON Schema. If validation fails and
                    // re_prompt_on_failure is true, inject the error and continue
                    // (ModelRetry pattern from PydanticAI).
                    //
                    // Retry attempts don't count against the iteration budget —
                    // they're self-correction attempts, not new task iterations.
                    if let Some(config) = &self.config.structured_output {
                        let validation = validate_json_schema(&text, &config.schema);
                        if !validation.passed {
                            if config.re_prompt_on_failure && structured_retry_count < config.max_retries {
                                structured_retry_count += 1;
                                let retry = oneai_core::ModelRetry {
                                    error_message: validation.error_summary(),
                                    retry_count: structured_retry_count,
                                    expected_schema: config.schema.clone(),
                                    failed_output: text.clone(),
                                };
                                let retry_prompt = build_retry_prompt(config, &retry);
                                tracing::info!(
                                    "StructuredOutput validation failed (retry {}/{}): {}",
                                    structured_retry_count, config.max_retries,
                                    validation.error_summary()
                                );
                                // Inject the validation error as a system message
                                state.conversation.add_message(Message::system(retry_prompt));
                                // Don't finalize the answer — continue the loop for re-generation
                                // Note: we don NOT increment iterations for retries
                                continue;
                            } else {
                                // Max retries exhausted or re_prompt disabled — end with error
                                tracing::warn!(
                                    "StructuredOutput validation failed (max retries {} exhausted): {}",
                                    config.max_retries,
                                    validation.error_summary()
                                );
                                state.conversation.add_message(Message::assistant(&text));
                                state.set_final_answer(format!(
                                    "[StructuredOutput validation failed]: {}",
                                    validation.error_summary()
                                ));
                            }
                        } else {
                            // Validation passed — finalize the answer
                            state.conversation.add_message(Message::assistant(&text));
                            state.set_final_answer(text);
                        }
                    } else {
                        // No StructuredOutput config — finalize normally
                        state.conversation.add_message(Message::assistant(&text));
                        state.set_final_answer(text);
                    }
                }
                AgentDecision::ToolCalls { calls } => {
                    observer.on_tool_calls(&calls);

                    // ─── Trace: log tool calls ──────────────────────────
                    if let Some(ctx) = &self.config.trace_context {
                        for call in &calls {
                            ctx.log_event(EventKind::Action, "tool.call", HashMap::from([
                                ("tool.name".to_string(), serde_json::json!(call.name)),
                                ("tool.call_id".to_string(), serde_json::json!(call.id)),
                            ]));
                        }
                    }

                    // ─── PreToolUse lifecycle hooks ─────────────────────────────
                    // Before executing each tool call, run PreToolUse hooks.
                    // Hooks can allow, deny, or modify the tool call args.
                    // This replaces some ApprovalGate use cases with programmatic hooks.
                    let mut filtered_calls = Vec::new();
                    {
                        let registry = self.hook_registry.read().await;
                        if registry.count_at(&HookPoint::PreToolUse) > 0 {
                            for call in &calls {
                                let hook_context = HookContext {
                                    point: HookPoint::PreToolUse,
                                    tool_name: Some(call.name.clone()),
                                    tool_args: Some(call.args.clone()),
                                    tool_output: None,
                                    inference_request: None,
                                    inference_response: None,
                                    iteration: state.iterations,
                                    paradigm: paradigm_name(&state.active_paradigm).to_string(),
                                };
                                let results = registry.run_hooks(HookPoint::PreToolUse, hook_context).await;
                                let resolved = HookRegistry::resolve_pre_tool_use_results(&results, &call.args);
                                match resolved {
                                    ResolvedHookAction::Allow { args: _ } => {
                                        // Original args — proceed as-is
                                        filtered_calls.push(call.clone());
                                    }
                                    ResolvedHookAction::Deny { reason } => {
                                        // Hook denied this tool call — inject denial message
                                        tracing::info!("PreToolUse hook denied tool '{}' ({})", call.name, reason);
                                        state.conversation.add_message(Message::tool_result(
                                            call.id.clone(),
                                            format!("Denied by lifecycle hook: {}", reason),
                                        ));
                                    }
                                    ResolvedHookAction::Modify { modified_args } => {
                                        // Hook modified args — use modified args
                                        tracing::info!("PreToolUse hook modified args for tool '{}'", call.name);
                                        filtered_calls.push(ToolCallRequest {
                                            id: call.id.clone(),
                                            name: call.name.clone(),
                                            args: modified_args,
                                        });
                                    }
                                }
                            }
                        } else {
                            // No PreToolUse hooks registered — proceed with all calls
                            filtered_calls = calls.clone();
                        }
                    }

                    // Add the assistant's tool-call message to conversation FIRST
                    // (the model's response with tool calls must precede tool results)
                    state.conversation.add_message(response.message.clone());

                    // Now execute the filtered tool calls and feed results
                    let results = if !filtered_calls.is_empty() {
                        self.execute_tool_calls(filtered_calls).await?
                    } else {
                        // All calls were denied by hooks — no results to feed
                        Vec::new()
                    };
                    for r in &results {
                        observer.on_tool_result(&r.call_id, &r.tool_name, &r.output);

                        // ─── PostToolUse lifecycle hooks ──────────────────────────
                        // After each tool execution, run PostToolUse hooks.
                        // Hooks can audit/log/transform the output.
                        {
                            let registry = self.hook_registry.read().await;
                            if registry.count_at(&HookPoint::PostToolUse) > 0 {
                                let hook_context = HookContext {
                                    point: HookPoint::PostToolUse,
                                    tool_name: Some("".to_string()), // Would need the tool name from the call
                                    tool_args: None,
                                    tool_output: Some(r.output.clone()),
                                    inference_request: None,
                                    inference_response: None,
                                    iteration: state.iterations,
                                    paradigm: paradigm_name(&state.active_paradigm).to_string(),
                                };
                                let _results = registry.run_hooks(HookPoint::PostToolUse, hook_context).await;
                                // PostToolUse hooks are informational (audit/log) —
                                // their results don't change the tool output for now.
                                // In a future version, Modify could transform the output.
                            }
                        }
                    }

                    // Error recovery: check for failed tool calls.
                    // This addresses the "RecoveryManager 未接线" gap.
                    // Previously, only a tracing::warn! was emitted. Now, if a
                    // RecoveryManager is wired in, failed calls trigger strategy
                    // evaluation and recovery feedback is injected into the conversation.
                    let failed_calls: Vec<_> = results.iter()
                        .filter(|r| !r.output.success)
                        .collect();
                    if !failed_calls.is_empty() {
                        tracing::warn!("{} tool calls failed in iteration {}",
                            failed_calls.len(), state.iterations);

                        // If RecoveryManager is available, evaluate recovery strategies
                        if let Some(rm) = &self.recovery_manager {
                            for failed in &failed_calls {
                                let strategy = self.select_recovery_strategy(failed);
                                let context = crate::error_recovery::ValidationContext {
                                    task: state.original_task.clone(),
                                    result: failed.output.error.as_deref().unwrap_or("Unknown error").to_string(),
                                    variables: std::collections::HashMap::from([
                                        ("tool_name".to_string(), "unknown".to_string()),
                                        ("iteration".to_string(), state.iterations.to_string()),
                                    ]),
                                };

                                let outcome = rm.apply(&strategy, &context).await?;
                                match outcome {
                                    crate::error_recovery::RecoveryOutcome::ValidationFailed { feedback } => {
                                        state.conversation.add_message(Message::system(
                                            format!("Recovery feedback: {}", feedback)
                                        ));
                                    }
                                    crate::error_recovery::RecoveryOutcome::Escalated { summary } => {
                                        state.conversation.add_message(Message::system(
                                            format!("Error escalated: {}", summary)
                                        ));
                                    }
                                    crate::error_recovery::RecoveryOutcome::RetryScheduled { max_retries } => {
                                        state.conversation.add_message(Message::system(
                                            format!("Recovery: retry scheduled (max {} attempts)", max_retries)
                                        ));
                                    }
                                    crate::error_recovery::RecoveryOutcome::RollbackTo { checkpoint_id } => {
                                        state.conversation.add_message(Message::system(
                                            format!("Recovery: rollback to checkpoint {}", checkpoint_id)
                                        ));
                                    }
                                    _ => {
                                        // Other outcomes are informational — just log
                                        tracing::debug!("Recovery outcome: {:?}", outcome);
                                    }
                                }
                            }
                        }
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

                    tracing::info!(
                        "AgentLoop iteration {}: ToolCalls completed. has_denied={}, conversation now has {} messages. \
                        Loop will continue with next iteration (is_complete={}).",
                        state.iterations,
                        has_denied,
                        state.conversation.messages.len(),
                        state.is_complete()
                    );
                }
                AgentDecision::Delegate { task, agent_type, budget } => {
                    observer.on_delegate(&task, &agent_type);

                    // ─── Trace: log delegation event ──────────────────────
                    if let Some(ctx) = &self.config.trace_context {
                        ctx.log_event(EventKind::WorkflowStepStart, "agent.delegate", HashMap::from([
                            ("agent.delegate_task".to_string(), serde_json::json!(task)),
                            ("agent.delegate_type".to_string(), serde_json::json!(format!("{:?}", agent_type))),
                        ]));
                    }
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

                    // ─── Trace: log paradigm switch event ──────────────────
                    if let Some(ctx) = &self.config.trace_context {
                        ctx.log_event(EventKind::WorkflowStepStart, "agent.paradigm_switch", HashMap::from([
                            ("agent.new_paradigm".to_string(), serde_json::json!(paradigm_name(&paradigm))),
                            ("agent.old_paradigm".to_string(), serde_json::json!(paradigm_name(&state.active_paradigm))),
                        ]));
                    }
                    let text_content = response.message.text_content();
                    if !text_content.is_empty() {
                        state.conversation.add_message(Message::assistant(&text_content));
                    }
                    // Try to execute a predefined StateGraph for this paradigm,
                    // fall back to semantic paradigm switch if no graph is available.
                    let result = self.apply_paradigm_switch_with_graph(paradigm, &mut state).await?;
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

        tracing::info!(
            "AgentLoop completed: iterations={}, completed={}, final_answer_len={}, final_answer_preview={}",
            result.iterations,
            result.completed,
            result.final_answer.len(),
            if result.final_answer.len() > 100 {
                // Use char-boundary-safe truncation to avoid panic on CJK strings
                let end = result.final_answer.char_indices()
                    .take_while(|(i, _)| *i < 100)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                format!("{}...", &result.final_answer[..end])
            } else {
                result.final_answer.clone()
            }
        );

        // ─── Trace: end AGENT span for the loop ──────────────────
        if let Some(ctx) = &self.config.trace_context {
            if !loop_span_id.is_empty() {
                ctx.set_attribute_on_span(&loop_span_id, "agent.iterations", serde_json::json!(result.iterations));
                ctx.set_attribute_on_span(&loop_span_id, "agent.completed", serde_json::json!(result.completed));
                ctx.exit_span(&loop_span_id, if result.completed { SpanStatus::Ok } else { SpanStatus::Error });
            }
        }

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

    // ─── StateGraph-driven Execution ─────────────────────────────────────────

    /// Run the AgentLoop using a StateGraph as the execution skeleton.
    ///
    /// This is the P2-2 "闭环" execution mode — when a DomainPack has a
    /// predefined StateGraph (e.g., "react-loop" for ReAct), the AgentLoop
    /// can execute it as an alternative to the standard while loop. The graph
    /// nodes delegate to the AgentLoop's own infrastructure (hooks, permission,
    /// domain pack, tool definitions) via `AgentLoopGraphActionExecutor`.
    ///
    /// This makes StateGraph execution a first-class mode of the AgentLoop,
    /// not a separate disconnected system. The key benefits:
    /// - LlmInfer nodes get proper tool definitions (filtered by paradigm config)
    /// - ToolCall nodes go through PreToolUse/PostToolUse hooks and domain permissions
    /// - Edge routing uses parsed_decision (GraphDecision) instead of string matching
    /// - SwitchParadigm nodes change the active paradigm for subsequent nodes
    ///
    /// If no StateGraph matching `graph_key` is found, falls back to the
    /// standard AgentLoop execution (`run_with_observer()`).
    pub async fn run_with_state_graph(
        &self,
        task: &str,
        graph_key: &str,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        // 1. Look up the StateGraph from DomainPack
        let graph = self.domain_pack.as_ref()
            .and_then(|dp| dp.get_state_graph(graph_key))
            .cloned();

        if graph.is_none() {
            tracing::info!(
                "No StateGraph '{}' found in DomainPack. Falling back to standard AgentLoop execution.",
                graph_key
            );
            // Fall back to standard execution
            return self.run_with_observer(task, observer).await;
        }

        let graph = graph.unwrap();
        tracing::info!(
            "Found StateGraph '{}' with {} nodes. Starting StateGraph-driven execution.",
            graph.name, graph.node_count()
        );

        // 2. Build GraphActionExecutor bridge
        let action_executor: Arc<dyn oneai_workflow::GraphActionExecutor> =
            Arc::new(AgentLoopGraphActionExecutor {
                provider: self.provider.clone(),
                tools: self.tools.clone(),
                parser: self.parser.clone(),
                approval_gate: self.approval_gate.clone(),
                domain_pack: self.domain_pack.clone(),
                hook_registry: self.hook_registry.clone(),
                recovery_manager: self.recovery_manager.clone(),
                config: self.config.clone(),
            });

        // 3. Build DelegateFactory bridge
        let delegate_factory: Arc<dyn oneai_workflow::DelegateFactory> =
            Arc::new(crate::sub_agent::SubAgentDelegateFactory::new(
                self.sub_agent_factory.clone(),
            ));

        // 4. Build initial GraphState from task
        let mut initial_state = oneai_workflow::GraphState::new();
        initial_state.conversation.add_message(Message::user(task.to_string()));
        if !initial_state.conversation.messages.iter().any(|m| m.role == Role::System) {
            initial_state.conversation.add_message(Message::system(self.config.system_prompt.clone()));
        }
        initial_state.variables.insert("task".to_string(), task.to_string());
        initial_state.active_paradigm = Some("react".to_string()); // Default paradigm for StateGraph

        // Set budget if available
        initial_state.token_budget_remaining = 100_000; // Default budget for StateGraph execution

        // 5. Create StateGraphExecutor with the bridge
        let executor = oneai_workflow::StateGraphExecutor::new(
            action_executor,
            delegate_factory,
            self.approval_gate.clone(),
            self.config.hard_max_iterations.unwrap_or(50),
        );

        // 6. Execute the graph
        observer.on_iteration_start(1, ParadigmKind::ReAct);

        let graph_result = executor.execute(&graph, initial_state).await?;

        // 7. Convert GraphExecutionResult → AgentLoopResult
        let result = AgentLoopResult {
            conversation: graph_result.final_state.conversation,
            final_answer: graph_result.final_state.last_result.clone().unwrap_or_default(),
            global_state: oneai_core::GlobalState::new(),
            iterations: graph_result.iterations,
            completed: graph_result.completed,
            active_paradigm: match graph_result.final_state.active_paradigm.as_deref() {
                Some("plan") => ParadigmKind::Plan,
                Some("reflect") => ParadigmKind::Reflect,
                Some("explore") => ParadigmKind::Explore,
                _ => ParadigmKind::ReAct,
            },
            sub_agent_results: Vec::new(),
        };

        observer.on_complete(&result);
        Ok(result)
    }

    // ─── Interrupt/Resume ────────────────────────────────────────────────

    /// Request an interrupt at the next iteration boundary.
    ///
    /// The loop will pause after completing the current iteration,
    /// emit `on_interrupt()`, and return a partial `AgentLoopResult`.
    /// The interrupt reason is stored and included in the `InterruptPoint`.
    ///
    /// The caller can then call `resume_from_interrupt()` to inject
    /// human feedback and continue execution.
    ///
    /// This is inspired by LangGraph's interrupt() pattern:
    /// the loop pauses at a clean boundary point, preserving all state,
    /// and resumes when the human provides guidance.
    pub fn request_interrupt(&self, reason: InterruptReason) {
        self.interrupt_requested.store(true, Ordering::Relaxed);
        // Store the reason — use try_lock since this is a synchronous method.
        // If the lock is held by the async loop, we'll still set the AtomicBool flag,
        // and the loop will check it at the next iteration boundary.
        if let Ok(mut guard) = self.interrupt_reason.try_lock() {
            *guard = Some(reason);
        }
    }

    /// Resume the agent loop from an interrupt point.
    ///
    /// This method creates a new LoopState from the interrupt context,
    /// injects the human feedback as a system message, and continues
    /// the loop execution.
    ///
    /// The `ResumeSignal` contains:
    /// - The interrupt ID being resumed from
    /// - Human feedback text
    /// - A `ResumeAction` (Continue, Modify, or Stop)
    ///
    /// Based on the ResumeAction:
    /// - **Continue**: Inject feedback and continue the loop
    /// - **Modify**: Inject feedback and modify the approach
    /// - **Stop**: Set a final answer and terminate the loop
    pub async fn resume_from_interrupt(
        &self,
        signal: ResumeSignal,
        observer: &dyn AgentLoopObserver,
    ) -> Result<AgentLoopResult> {
        observer.on_resume(&signal);

        // Create a new LoopState from the interrupt context
        // The conversation should already contain prior messages
        // (we start fresh with a new task that includes the feedback)
        let feedback_task = format!(
            "[Human feedback]: {}",
            signal.feedback
        );

        match signal.action {
            ResumeAction::Continue => {
                // Continue execution with the feedback injected
                self.run_with_observer(&feedback_task, observer).await
            }
            ResumeAction::Modify { modified_args } => {
                // Modify the approach based on feedback
                let modify_msg = if let Some(args) = modified_args {
                    format!(
                        "[Human feedback]: {}. Modified approach: {}",
                        signal.feedback,
                        args
                    )
                } else {
                    format!("[Human feedback]: {}. Please adjust your approach.", signal.feedback)
                };
                self.run_with_observer(&modify_msg, observer).await
            }
            ResumeAction::Stop => {
                // Human decided to abort — return a final result
                let result = AgentLoopResult {
                    conversation: Conversation::new(),
                    final_answer: format!("Task stopped by human: {}", signal.feedback),
                    global_state: oneai_core::GlobalState::new(),
                    iterations: 0,
                    completed: true,
                    active_paradigm: ParadigmKind::ReAct,
                    sub_agent_results: Vec::new(),
                };
                observer.on_complete(&result);
                Ok(result)
            }
        }
    }

    /// Get a reference to the hook registry for registering lifecycle hooks.
    ///
    /// Hooks can be registered before the loop starts running.
    /// They will be called at their registered lifecycle points.
    pub fn hook_registry(&self) -> Arc<tokio::sync::RwLock<HookRegistry>> {
        self.hook_registry.clone()
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
        // ─── Smart Tool Router ──────────────────────────────────────────────────
        // Intercept shell calls that are actually file operations and redirect them
        // to the appropriate specialized tool. This is a programmatic fallback that
        // works regardless of model intelligence — even if the model (GLM/Qwen)
        // ignores system prompt tool preference rules, we still route correctly.
        //
        // This addresses the "shell优先级过高" problem at the runtime level.
        // Pattern: "shell cat file.rs" → redirect to read_file
        // Pattern: "shell sed 's/old/new/' file" → redirect to edit_file
        // Pattern: "shell ls dir" → redirect to list_directory
        // Pattern: "shell grep pattern file" → redirect to grep
        // Pattern: "shell find . -name '*.rs'" → redirect to glob
        // Pattern: "shell mkdir dir" → redirect to shell (no mkdir tool, keep)
        let routed_calls: Vec<ToolCallRequest> = calls.into_iter().map(|call| {
            if call.name == "shell" {
                Self::route_shell_to_specialized(call)
            } else {
                call
            }
        }).collect();

        let tools_map = self.tools.read().await;
        let mut results = Vec::new();

        // Pre-check domain PermissionProfile for each call
        let domain_permission_checks: Vec<Option<PermissionAction>> = routed_calls.iter().map(|call| {
            self.domain_pack.as_ref().map(|dp| dp.resolve_permission(&call.name, &call.args))
        }).collect();

        let futures: Vec<_> = routed_calls.into_iter().enumerate().map(|(idx, call)| {
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
                        Ok(ToolCallResult { call_id, tool_name, output: ToolOutput {
                            success: false, content: String::new(),
                            error: Some(format!("Denied by domain policy: {}", reason)),
                        }})
                    }
                    Some(PermissionAction::AutoApprove) => {
                        // Domain says auto-approve — skip approval gate
                        match tool_opt {
                            Some(tool) => {
                                let output = tool.execute(args).await?;
                                Ok::<ToolCallResult, oneai_core::error::OneAIError>(ToolCallResult { call_id, tool_name, output })
                            }
                            None => {
                                let err_msg = format!("Tool '{}' not found", tool_name);
                                Ok(ToolCallResult { call_id, tool_name, output: ToolOutput {
                                    success: false, content: String::new(),
                                    error: Some(err_msg),
                                }})
                            }
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
                                Self::handle_approval(approval_gate, request, tool, args, call_id, tool_name).await
                            }
                            None => {
                                let err_msg = format!("Tool '{}' not found", tool_name);
                                Ok(ToolCallResult { call_id, tool_name, output: ToolOutput {
                                    success: false, content: String::new(),
                                    error: Some(err_msg),
                                }})
                            }
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
                                    Self::handle_approval(approval_gate, request, tool, args, call_id, tool_name).await
                                } else {
                                    let output = tool.execute(args).await?;
                                    Ok::<ToolCallResult, oneai_core::error::OneAIError>(ToolCallResult { call_id, tool_name, output })
                                }
                            }
                            None => {
                                let err_msg = format!("Tool '{}' not found", tool_name);
                                Ok(ToolCallResult { call_id, tool_name, output: ToolOutput {
                                    success: false, content: String::new(),
                                    error: Some(err_msg),
                                }})
                            }
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
                                    Self::handle_approval(approval_gate, request, tool, args, call_id, tool_name).await
                                } else {
                                    let output = tool.execute(args).await?;
                                    Ok(ToolCallResult { call_id, tool_name, output })
                                }
                            }
                            None => {
                                let err_msg = format!("Tool '{}' not found", tool_name);
                                Ok(ToolCallResult { call_id, tool_name, output: ToolOutput {
                                    success: false, content: String::new(),
                                    error: Some(err_msg),
                                }})
                            }
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
                    tool_name: String::new(),
                    output: ToolOutput {
                        success: false, content: String::new(),
                        error: Some(format!("Tool execution error: {}", e)),
                    },
                }),
            }
        }
        Ok(results)
    }

    /// Smart Tool Router — intercept shell calls for file operations and
    /// redirect to specialized tools.
    ///
    /// This is a programmatic fallback that works regardless of model intelligence.
    /// When the model (especially GLM/Qwen) calls shell with commands like
    /// "cat file.rs" or "sed 's/old/new/' file.rs", this router detects the
    /// actual intent and redirects to read_file or edit_file respectively.
    ///
    /// Only redirects when the specialized tool exists in the tools_map.
    /// If the specialized tool doesn't exist, the original shell call is kept.
    ///
    /// Inspired by Claude Code's approach where specialized tools are always
    /// preferred, and SWE-agent's Agent-Computer Interface pattern where
    /// raw shell access is constrained to purpose-built commands.
    fn route_shell_to_specialized(call: ToolCallRequest) -> ToolCallRequest {
        // Only intercept shell calls
        if call.name != "shell" {
            return call;
        }

        // Extract the command string from args
        let command = call.args.get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if command.is_empty() {
            return call;
        }

        // Parse the first word (the actual command) and its arguments
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            return call;
        }

        let cmd = parts[0];
        let cmd_args: Vec<&str> = if parts.len() > 1 {
            parts[1..].to_vec()
        } else {
            Vec::new()
        };

        // ─── Redirect patterns ──────────────────────────────────────────────────
        // Map common shell commands to their specialized tool equivalents.

        match cmd {
            // cat → read_file
            "cat" | "head" | "tail" | "less" | "more" | "bat" => {
                let file_path = cmd_args.iter().find(|a| !a.starts_with('-')).unwrap_or(&"");
                if !file_path.is_empty() {
                    return ToolCallRequest {
                        id: call.id,
                        name: "read_file".to_string(),
                        args: serde_json::json!({
                            "path": file_path,
                        }),
                    };
                }
            }

            // sed → keep as shell (too complex to parse reliably)
            "sed" => {
                // sed patterns are too varied to reliably parse into edit_file format
            }

            // ls → list_directory
            "ls" | "dir" => {
                let dir_path = cmd_args.iter().find(|a| !a.starts_with('-')).unwrap_or(&".");
                return ToolCallRequest {
                    id: call.id,
                    name: "list_directory".to_string(),
                    args: serde_json::json!({
                        "path": dir_path,
                    }),
                };
            }

            // grep (shell grep) → grep tool
            "grep" | "rg" | "ag" | "ack" => {
                // Parse: grep [options] pattern [path]
                let non_option_args: Vec<&str> = cmd_args.iter()
                    .filter(|a| !a.starts_with('-'))
                    .map(|a| *a)
                    .collect();
                if !non_option_args.is_empty() {
                    let pattern = non_option_args[0];
                    let path = non_option_args.get(1).copied().unwrap_or(".");
                    return ToolCallRequest {
                        id: call.id,
                        name: "grep".to_string(),
                        args: serde_json::json!({
                            "pattern": pattern,
                            "path": path,
                        }),
                    };
                }
            }

            // find → glob
            "find" | "locate" => {
                let path = cmd_args.iter().find(|a| !a.starts_with('-')).unwrap_or(&".");
                let name_idx = cmd_args.iter().position(|a| *a == "-name" || *a == "-iname");
                if let Some(idx) = name_idx {
                    if idx + 1 < cmd_args.len() {
                        let pattern = cmd_args[idx + 1].replace("\"", "");
                        return ToolCallRequest {
                            id: call.id,
                            name: "glob".to_string(),
                            args: serde_json::json!({
                                "pattern": pattern,
                                "path": path,
                            }),
                        };
                    }
                }
                // find without -name → list_directory
                return ToolCallRequest {
                    id: call.id,
                    name: "list_directory".to_string(),
                    args: serde_json::json!({
                        "path": path,
                    }),
                };
            }

            // pwd → environment
            "pwd" | "whoami" | "uname" | "which" => {
                return ToolCallRequest {
                    id: call.id,
                    name: "environment".to_string(),
                    args: serde_json::json!({}),
                };
            }

            // echo (simple, no redirect) → environment-like
            "echo" => {
                // If it has > or >>, it's a write operation → keep as shell
                if cmd_args.iter().any(|a| a.contains(">") || a.contains(">>")) {
                    return call;
                }
            }

            // tree → list_directory
            "tree" => {
                let dir_path = cmd_args.iter().find(|a| !a.starts_with('-')).unwrap_or(&".");
                return ToolCallRequest {
                    id: call.id,
                    name: "list_directory".to_string(),
                    args: serde_json::json!({
                        "path": dir_path,
                    }),
                };
            }

            // file → read_file
            "file" => {
                let file_path = cmd_args.iter().find(|a| !a.starts_with('-')).unwrap_or(&"");
                if !file_path.is_empty() {
                    return ToolCallRequest {
                        id: call.id,
                        name: "read_file".to_string(),
                        args: serde_json::json!({
                            "path": file_path,
                        }),
                    };
                }
            }

            // curl/wget → web_fetch (for simple URL fetches only)
            "curl" | "wget" => {
                let url_arg = cmd_args.iter().find(|a| a.starts_with("http://") || a.starts_with("https://"));
                if let Some(url) = url_arg {
                    // Only redirect simple URL fetches (not POST/PUT/etc.)
                    if !cmd_args.iter().any(|a| *a == "-X" || *a == "-d" || *a == "--data" || *a == "-F" || *a == "-T") {
                        return ToolCallRequest {
                            id: call.id,
                            name: "web_fetch".to_string(),
                            args: serde_json::json!({
                                "url": url,
                            }),
                        };
                    }
                }
            }

            // date → environment
            "date" => {
                return ToolCallRequest {
                    id: call.id,
                    name: "environment".to_string(),
                    args: serde_json::json!({}),
                };
            }

            _ => {
                // Unknown command — keep as shell (git, cargo, npm, python, etc.)
            }
        }

        // No redirect matched — keep original shell call
        call
    }

    async fn spawn_sub_agent(&self, task: String, agent_type: SubAgentKind, budget: oneai_core::budget::TokenBudget) -> Result<SubAgentSummary> {
        let sub_agent = self.sub_agent_factory.create(agent_type.clone(), budget).await?;
        sub_agent.run(&task).await
    }

    /// Apply a paradigm switch — produces real, observable behavior changes.
    ///
    /// This addresses the "范式切换语义空洞" gap. Previously, `run_paradigm()`
    /// just returned a formatted string like "Plan paradigm activated" — no
    /// actual behavior change. Now, paradigm switching does three things:
    ///
    /// 1. **Replaces the system prompt** in the conversation — removes the
    ///    old system message and adds a paradigm-specific one.
    /// 2. **Stores ParadigmConfig** in LoopState — `build_tool_definitions()`
    ///    uses the config's tool_filter to only send relevant tools to the model.
    /// 3. **Injects a decision hint** — a brief system message telling the model
    ///    what kind of decisions to make in this paradigm.
    ///
    /// Inspired by Aider's Architect/Editor dual-model pattern where each
    /// "role" has its own prompt and tool set. OneAI extends this to 4 paradigms.
    fn apply_paradigm_switch(&self, paradigm: ParadigmKind, state: &mut LoopState) -> String {
        let config = ParadigmConfig::for_paradigm(paradigm);

        // Step 1: Replace system prompt in conversation
        // Remove existing system messages and add the paradigm-specific one
        state.conversation.messages.retain(|m| m.role != Role::System);
        state.conversation.add_message(Message::system(&config.system_prompt));

        // Step 2: Inject decision hint as additional context
        if !config.decision_hint.is_empty() {
            state.conversation.add_message(Message::system(
                format!("[Paradigm switch]: {}", config.decision_hint)
            ));
        }

        // Step 3: Store ParadigmConfig for tool filtering
        state.active_paradigm = paradigm;
        state.active_paradigm_config = Some(config.clone());

        // Return a concise summary for the loop
        format!(
            "{} paradigm activated — system prompt changed, tools filtered to: [{}]",
            paradigm_name(&paradigm),
            config.tool_filter.join(", ")
        )
    }

    /// Apply paradigm switch — with optional StateGraph execution.
    ///
    /// When a DomainPack has a predefined StateGraph matching the paradigm
    /// (e.g., "react-loop" for ReAct), this method first applies the
    /// semantic paradigm switch, then attempts to execute the StateGraph.
    /// If the StateGraph executes successfully, its output is injected
    /// into the conversation as an assistant message.
    ///
    /// If no StateGraph is found, or execution fails, this falls back
    /// to the purely semantic paradigm switch (apply_paradigm_switch).
    async fn apply_paradigm_switch_with_graph(
        &self,
        paradigm: ParadigmKind,
        state: &mut LoopState,
    ) -> Result<String> {
        // First, apply the semantic paradigm switch (always happens)
        let base_result = self.apply_paradigm_switch(paradigm, state);

        // Look for a predefined StateGraph for this paradigm in the DomainPack
        let graph_key = match paradigm {
            ParadigmKind::ReAct => "react-loop",
            ParadigmKind::Plan => "plan-workflow",
            ParadigmKind::Reflect => "reflect-workflow",
            ParadigmKind::Explore => "explore-workflow",
        };

        let graph = self.domain_pack.as_ref()
            .and_then(|dp| dp.get_state_graph(graph_key))
            .cloned();

        if let Some(graph) = graph {
            tracing::info!(
                "Found predefined StateGraph '{}' for paradigm {}. Attempting execution.",
                graph.name, paradigm_name(&paradigm)
            );

            // Build a StateGraphExecutor from the AgentLoop's dependencies
            // Use the AgentLoop's SubAgentFactory as the DelegateFactory bridge
            let delegate_factory: Arc<dyn oneai_workflow::DelegateFactory> =
                Arc::new(crate::sub_agent::SubAgentDelegateFactory::new(
                    self.sub_agent_factory.clone(),
                ));

            // P2-2: Use DirectProviderActionExecutor for backward-compatible
            // StateGraph execution within paradigm switch context.
            // The AgentLoopGraphActionExecutor will be used for full
            // StateGraph-driven execution via run_with_state_graph().
            let executor = oneai_workflow::StateGraphExecutor::with_direct_provider_defaults(
                self.provider.clone(),
                self.tools.clone(),
                delegate_factory,
                self.approval_gate.clone(),
            );

            // Build initial state from the current conversation
            let mut initial_state = oneai_workflow::GraphState::new();
            initial_state.conversation = state.conversation.clone();
            // Copy relevant variables from LoopState into graph state
            initial_state.variables.insert("task".to_string(), state.original_task.clone());

            let graph_result = executor.execute(&graph, initial_state).await;

            match graph_result {
                Ok(result) => {
                    if result.completed {
                        tracing::info!(
                            "StateGraph '{}' completed successfully after {} iterations. Terminal: {}",
                            result.name, result.iterations,
                            result.terminal_node.as_deref().unwrap_or("none")
                        );
                        // Inject the StateGraph's final output into the loop conversation
                        if let Some(output) = &result.final_state.last_result {
                            state.conversation.add_message(Message::assistant(
                                format!("[StateGraph {} result]: {}", result.name, output)
                            ));
                        }
                        // Merge any new variables from the graph state back
                        for (key, value) in &result.final_state.variables {
                            if !key.starts_with("_") { // Skip internal variables
                                state.global_state.context.insert(key.clone(), value.clone());
                            }
                        }
                        return Ok(format!(
                            "{} paradigm + StateGraph '{}' executed ({} iterations). {}",
                            paradigm_name(&paradigm),
                            result.name,
                            result.iterations,
                            base_result
                        ));
                    } else {
                        tracing::warn!(
                            "StateGraph '{}' did not reach a terminal node after {} iterations.",
                            result.name, result.iterations
                        );
                        // Still useful — inject partial results
                        if let Some(output) = &result.final_state.last_result {
                            state.conversation.add_message(Message::assistant(
                                format!("[StateGraph {} partial]: {}", result.name, output)
                            ));
                        }
                        return Ok(format!(
                            "{} paradigm + StateGraph '{}' incomplete ({} iterations). {}",
                            paradigm_name(&paradigm),
                            result.name,
                            result.iterations,
                            base_result
                        ));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "StateGraph '{}' execution failed: {}. Falling back to semantic paradigm switch.",
                        graph.name, e
                    );
                    // Fall back — the semantic switch was already applied
                    return Ok(format!(
                        "{} paradigm activated (StateGraph '{}' failed: {}). {}",
                        paradigm_name(&paradigm),
                        graph.name,
                        e,
                        base_result
                    ));
                }
            }
        }

        // No predefined StateGraph — semantic switch only (already applied)
        Ok(base_result)
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
                    crate::streaming::StreamEvent::ThinkingFragment { text } => {
                        observer.on_thinking(&text);
                    }
                    crate::streaming::StreamEvent::ToolIntentDetected { call_id: _, tool_name } => {
                        // Tool intent detected — show the tool name in the TUI as a
                        // lightweight "intent" indicator (no args yet, just the name).
                        // This replaces the previous approach of calling on_tool_calls
                        // with empty args, which created duplicate tool call cards in
                        // the TUI (one for intent, one for completion). Now we only
                        // send on_tool_calls for fully assembled tool calls.
                        observer.on_stream_chunk(&format!("▸ preparing {}…", tool_name));
                    }
                    crate::streaming::StreamEvent::ToolCallComplete { call_id, tool_name, args } => {
                        // Tool call is fully assembled — notify observer with complete args
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

        // NOTE: Do NOT re-notify the observer with thinking content here.
        // During streaming, every ThinkingFragment already called
        // observer.on_thinking() with the incremental delta, and the TUI
        // appended those deltas into the thinking bubble. The content_blocks
        // here carry the FULL assembled thinking snapshot (used to build the
        // InferenceResponse below). Re-emitting it as an observer event would
        // make the TUI append the entire thinking text a second time — the
        // "thinking displays twice" bug.

        tracing::info!(
            "Streaming iteration completed: {} content blocks (text: {} chars, tool_calls: {}, thinking: {} chars)",
            content_blocks.len(),
            content_blocks.iter().filter_map(|b| match b { ContentBlock::Text { text } => Some(text.len()), _ => None }).sum::<usize>(),
            content_blocks.iter().filter(|b| matches!(b, ContentBlock::ToolCall { .. })).count(),
            content_blocks.iter().filter_map(|b| match b { ContentBlock::Thinking { text } => Some(text.len()), _ => None }).sum::<usize>(),
        );

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

    #[allow(dead_code)]
    async fn build_tool_definitions(&self) -> Vec<ToolDefinition> {
        let tools_map = self.tools.read().await;

        // Determine the active paradigm config for tool filtering.
        // If a paradigm config is active, only send tools in the config's
        // tool_filter to the model. This is the real behavioral change
        // that makes paradigm switching meaningful.
        //
        // When no paradigm config is active (initial state before any switch),
        // all tools are available (default ReAct behavior).
        let _tool_filter: Option<&[String]> = None; // Will be checked from LoopState in run_loop

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

    /// Build tool definitions filtered by paradigm config.
    ///
    /// This is called from run_loop() where we have access to the
    /// LoopState's active_paradigm_config. Paradigm-configured tool
    /// filtering is the key behavioral change that makes paradigm
    /// switching meaningful — Plan mode shouldn't see edit tools,
    /// Explore mode shouldn't see execution tools.
    async fn build_tool_definitions_for_paradigm(&self, paradigm_config: Option<&ParadigmConfig>) -> Vec<ToolDefinition> {
        let tools_map = self.tools.read().await;

        // If a paradigm config is active, filter tools by its tool_filter list.
        // Only tools in the filter are sent to the model — this prevents the
        // model from calling tools that aren appropriate for the current paradigm.
        let filtered_tools: Vec<&Arc<dyn Tool>> = if let Some(config) = paradigm_config {
            if config.tool_filter.is_empty() {
                // Empty filter means "all tools available" — no restriction
                tools_map.values().collect()
            } else {
                // Filter: only include tools that are in the paradigm's tool_filter
                tools_map.values()
                    .filter(|tool| config.tool_filter.contains(&tool.name().to_string()))
                    .collect()
            }
        } else {
            // No paradigm config — all tools available (default ReAct behavior)
            tools_map.values().collect()
        };

        // ─── Tool ordering strategy ──────────────────────────────────────────
        // Research shows LLMs exhibit significant position bias (15-30% accuracy
        // drop when correct tool moves from first to later position). Chinese models
        // (GLM/Qwen) are especially susceptible. To guide the model toward using
        // specialized tools instead of shell for file operations, we sort tools
        // strategically: specialized tools FIRST, shell LAST.
        //
        // Priority tiers:
        //   Tier 1 (highest): read_file, grep, glob, list_directory  (read-only, most specific)
        //   Tier 2 (high):    edit_file, apply_patch, notebook_edit (edit-specific)
        //   Tier 3 (medium):  web_fetch, environment, calculator   (general but not shell)
        //   Tier 4 (lowest):  shell                                (fallback, least specific)
        //   Tier 5 (default): any tool not in above tiers          (unknown tools)
        let tier_order = |name: &str| -> u32 {
            match name {
                // Tier 1: Read-only, most specific — always prefer over shell
                "read_file" | "file_read" => 1,
                "grep" | "search" => 1,
                "glob" | "file_glob" => 1,
                "list_directory" => 1,
                // Tier 2: Edit-specific — prefer over shell for modifications
                "edit_file" | "file_edit" => 2,
                "apply_patch" => 2,
                "notebook_edit" => 2,
                // Tier 3: General but not shell
                "web_fetch" => 3,
                "environment" => 3,
                "calculator" => 3,
                // Tier 4: Shell — ALWAYS LAST (most general, most overused)
                "shell" => 10,
                // Tier 5: Unknown/custom tools — after specialized, before shell
                _ => 5,
            }
        };

        let mut sorted_tools: Vec<&Arc<dyn Tool>> = filtered_tools;
        sorted_tools.sort_by_key(|tool| tier_order(tool.name()));

        // Apply domain pack tool decorators if present
        if let Some(domain) = &self.domain_pack {
            sorted_tools.iter().map(|tool| {
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
            // No domain pack — use raw tool definitions (still sorted)
            sorted_tools.iter().map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters_schema: tool.parameters_schema(),
            }).collect()
        }
    }

    fn active_skill_descriptors(&self) -> Result<Vec<oneai_core::SkillDescriptor>> {
        Ok(Vec::new())
    }

    /// Select a recovery strategy based on the type of tool call failure.
    ///
    /// Maps error patterns to appropriate RecoveryStrategy types:
    /// - Network/timeout errors → Retry (transient, may succeed on retry)
    /// - Permission denied → Escalate (requires human intervention)
    /// - Tool not found → ConditionalFallback (route to alternative tool)
    /// - Execution errors → ExternalFeedback (use validator to judge)
    ///
    /// This is a basic mapping — more sophisticated strategy selection
    /// can be added based on DomainPack recovery configurations.
    fn select_recovery_strategy(&self, failed: &ToolCallResult) -> crate::error_recovery::RecoveryStrategy {
        let error_msg = failed.output.error.as_deref().unwrap_or("");

        if error_msg.contains("timeout") || error_msg.contains("timed out")
            || error_msg.contains("network") || error_msg.contains("rate_limit") {
            // Transient errors → Retry with exponential backoff
            crate::error_recovery::RecoveryStrategy::Retry {
                policy: crate::error_recovery::RetryPolicy::default(),
            }
        } else if error_msg.starts_with("Denied") || error_msg.contains("permission") {
            // Permission denied → Escalate to human
            crate::error_recovery::RecoveryStrategy::Escalate {
                error_summary: format!("Tool call denied: {}", error_msg),
            }
        } else if error_msg.contains("not found") {
            // Tool not found → Fallback to alternative
            crate::error_recovery::RecoveryStrategy::ConditionalFallback {
                error_node: "tool_call".to_string(),
                fix_node: "alternative_approach".to_string(),
            }
        } else {
            // Default: escalate — let the main agent decide
            crate::error_recovery::RecoveryStrategy::Escalate {
                error_summary: format!("Tool execution error: {}", error_msg),
            }
        }
    }

    /// Handle the approval gate interaction for a tool call.
    async fn handle_approval(
        approval_gate: Arc<dyn ApprovalGate>,
        request: oneai_core::ApprovalRequest,
        tool: Arc<dyn Tool>,
        args: serde_json::Value,
        call_id: String,
        tool_name: String,
    ) -> Result<ToolCallResult> {
        match approval_gate.request_approval(request).await {
            Ok(oneai_core::ApprovalResponse::Approved { modified_args }) => {
                let final_args = modified_args.unwrap_or(args);
                let output = tool.execute(final_args).await?;
                Ok(ToolCallResult { call_id, tool_name, output })
            }
            Ok(oneai_core::ApprovalResponse::Denied { reason }) => {
                Ok(ToolCallResult { call_id, tool_name, output: ToolOutput {
                    success: false, content: String::new(),
                    error: Some(format!("Denied: {}", reason)),
                }})
            }
            Ok(oneai_core::ApprovalResponse::Modified { args: modified_args }) => {
                let output = tool.execute(modified_args).await?;
                Ok(ToolCallResult { call_id, tool_name, output })
            }
            Ok(oneai_core::ApprovalResponse::Observe { observation }) => {
                Ok(ToolCallResult { call_id, tool_name, output: ToolOutput {
                    success: false,
                    content: format!("Observe: {}", observation),
                    error: Some("Execution paused for observation".to_string()),
                }})
            }
            Err(e) => {
                Ok(ToolCallResult { call_id, tool_name, output: ToolOutput {
                    success: false, content: String::new(),
                    error: Some(format!("Approval error: {}", e)),
                }})
            }
        }
    }
}

// ─── AgentLoopGraphActionExecutor ──────────────────────────────────────────

/// Concrete `GraphActionExecutor` that delegates to AgentLoop's full infrastructure.
///
/// This is the P2-2 bridge — when a StateGraph is active, the AgentLoop
/// creates this executor which uses the loop's own:
/// - LLM provider (with context assembly + tool definitions)
/// - Tool registry (with domain pack decorators)
/// - Permission gate (with domain permission profile)
/// - Hook registry (PreInfer, PostInfer, PreToolUse, PostToolUse)
/// - Output parser (for GraphDecision parsing)
/// - Recovery manager (for error recovery on failed tool calls)
///
/// The key difference from `DirectProviderActionExecutor` is that LlmInfer
/// nodes get proper tool definitions (filtered by paradigm config and domain
/// pack decorators), and ToolCall nodes go through the full permission and
/// hooks pipeline. This makes StateGraph execution truly integrated with
/// the AgentLoop, not a separate disconnected system.
#[allow(dead_code)]
pub struct AgentLoopGraphActionExecutor {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    parser: Arc<dyn OutputParser>,
    approval_gate: Arc<dyn ApprovalGate>,
    domain_pack: Option<Arc<MergedDomainPack>>,
    hook_registry: Arc<RwLock<HookRegistry>>,
    recovery_manager: Option<Arc<crate::error_recovery::RecoveryManager>>,
    config: AgentLoopConfig,
}

#[async_trait::async_trait]
impl oneai_workflow::GraphActionExecutor for AgentLoopGraphActionExecutor {
    /// Execute an LLM inference node using AgentLoop's full pipeline.
    ///
    /// This method:
    /// 1. Determines the active paradigm from GraphState or NodeAction
    /// 2. Builds tool definitions filtered by paradigm config and domain pack decorators
    /// 3. Runs PreInfer hooks (if any registered)
    /// 4. Calls provider.infer() with the full inference request
    /// 5. Runs PostInfer hooks (if any registered)
    /// 6. Parses the response into a GraphDecision using the same OutputParser
    /// 7. Stores the parsed_decision in GraphState for edge condition routing
    async fn execute_llm_infer(
        &self,
        action: &oneai_workflow::NodeAction,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_workflow::ActionResult> {
        // Extract LlmInfer fields
        let (system_prompt_override, include_tool_definitions,
             tool_filter_override, thinking_budget, temperature, max_tokens) = match action {
            oneai_workflow::NodeAction::LlmInfer {
                system_prompt_override, include_tool_definitions,
                tool_filter_override, thinking_budget, temperature, max_tokens, ..
            } => (
                system_prompt_override.clone(),
                *include_tool_definitions,
                tool_filter_override.clone(),
                *thinking_budget,
                *temperature,
                *max_tokens,
            ),
            _ => return Err(oneai_core::error::OneAIError::Workflow("Expected LlmInfer action".to_string())),
        };

        // Build system prompt — use override or default from config
        let system_prompt = system_prompt_override
            .unwrap_or_else(|| self.config.system_prompt.clone());

        let mut conversation = state.conversation.clone();
        // Inject system prompt if not already present
        if !conversation.messages.iter().any(|m| m.role == Role::System) {
            conversation.add_message(Message::system(&system_prompt));
        }

        // Build tool definitions if requested
        let tool_defs = if include_tool_definitions {
            self.build_tool_definitions_for_state(&tool_filter_override, &state.active_paradigm).await
        } else {
            vec![]
        };

        // Build inference request
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

        // Run inference
        let response = self.provider.infer(request).await?;
        let output = response.message.text_content();

        // Update conversation
        state.conversation.add_message(response.message.clone());

        // Parse decision and store in state
        let _decision = self.parse_decision(&response, state).await?;

        Ok(oneai_workflow::ActionResult {
            output,
            error: None,
        })
    }

    /// Execute a tool call node using AgentLoop's permission and hooks pipeline.
    async fn execute_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_workflow::ActionResult> {
        // Find the tool
        let tools_map = self.tools.read().await;
        let tool = tools_map.get(tool_name)
            .ok_or_else(|| oneai_core::error::OneAIError::Workflow(
                format!("Tool '{}' not found for ToolCall node", tool_name)
            ))?;

        // Check domain permission profile (if domain_pack is available)
        if let Some(domain) = &self.domain_pack {
            let perm_action = domain.resolve_permission(tool_name, args);
            match perm_action {
                oneai_domain::PermissionAction::Deny { reason } => {
                    return Ok(oneai_workflow::ActionResult {
                        output: String::new(),
                        error: Some(format!("Denied by domain policy: {}", reason)),
                    });
                }
                oneai_domain::PermissionAction::AutoApprove => {
                    // Skip approval gate — domain says auto-approve
                    let output = tool.execute(args.clone()).await?;
                    state.conversation.add_message(Message::tool_result(
                        format!("graph_tool_{}", tool_name),
                        output.content.clone(),
                    ));
                    return Ok(oneai_workflow::ActionResult {
                        output: output.content,
                        error: output.error,
                    });
                }
                oneai_domain::PermissionAction::RequireConfirmation => {
                    // Need approval gate interaction
                    let request = oneai_core::ApprovalRequest {
                        tool_name: tool_name.to_string(),
                        args: args.clone(),
                        risk_level: oneai_core::RiskLevel::High,
                        permission_level: Some(oneai_core::PermissionLevel::Full),
                        justification: format!("Domain policy requires confirmation for '{}'", tool_name),
                    };
                    let approval = self.approval_gate.request_approval(request).await?;
                    match approval {
                        oneai_core::ApprovalResponse::Denied { reason } => {
                            return Ok(oneai_workflow::ActionResult {
                                output: String::new(),
                                error: Some(format!("Denied: {}", reason)),
                            });
                        }
                        oneai_core::ApprovalResponse::Approved { modified_args } => {
                            let final_args = modified_args.unwrap_or_else(|| args.clone());
                            let output = tool.execute(final_args).await?;
                            state.conversation.add_message(Message::tool_result(
                                format!("graph_tool_{}", tool_name),
                                output.content.clone(),
                            ));
                            return Ok(oneai_workflow::ActionResult {
                                output: output.content,
                                error: output.error,
                            });
                        }
                        _ => {
                            // Modified or Observe — proceed with execution
                            let output = tool.execute(args.clone()).await?;
                            state.conversation.add_message(Message::tool_result(
                                format!("graph_tool_{}", tool_name),
                                output.content.clone(),
                            ));
                            return Ok(oneai_workflow::ActionResult {
                                output: output.content,
                                error: output.error,
                            });
                        }
                    }
                }
                oneai_domain::PermissionAction::UseDefaultPermission { level } => {
                    if level == oneai_core::PermissionLevel::Full {
                        let request = oneai_core::ApprovalRequest {
                            tool_name: tool_name.to_string(),
                            args: args.clone(),
                            risk_level: tool.risk_level(),
                            permission_level: Some(level),
                            justification: format!("Full-permission tool '{}' requires approval", tool_name),
                        };
                        let approval = self.approval_gate.request_approval(request).await?;
                        match approval {
                            oneai_core::ApprovalResponse::Denied { reason } => {
                                return Ok(oneai_workflow::ActionResult {
                                    output: String::new(),
                                    error: Some(format!("Denied: {}", reason)),
                                });
                            }
                            _ => {
                                let output = tool.execute(args.clone()).await?;
                                state.conversation.add_message(Message::tool_result(
                                    format!("graph_tool_{}", tool_name),
                                    output.content.clone(),
                                ));
                                return Ok(oneai_workflow::ActionResult {
                                    output: output.content,
                                    error: output.error,
                                });
                            }
                        }
                    }
                    // Standard or Read permission — execute directly
                    let output = tool.execute(args.clone()).await?;
                    state.conversation.add_message(Message::tool_result(
                        format!("graph_tool_{}", tool_name),
                        output.content.clone(),
                    ));
                    return Ok(oneai_workflow::ActionResult {
                        output: output.content,
                        error: output.error,
                    });
                }
            }
        }

        // No domain pack — check tool's risk level for approval
        let perm_level = oneai_core::PermissionLevel::from_risk_level(tool.risk_level());
        if perm_level == oneai_core::PermissionLevel::Full {
            let request = oneai_core::ApprovalRequest {
                tool_name: tool_name.to_string(),
                args: args.clone(),
                risk_level: tool.risk_level(),
                permission_level: Some(perm_level),
                justification: format!("Full-permission tool '{}' requires approval", tool_name),
            };
            let approval = self.approval_gate.request_approval(request).await?;
            match approval {
                oneai_core::ApprovalResponse::Denied { reason } => {
                    return Ok(oneai_workflow::ActionResult {
                        output: String::new(),
                        error: Some(format!("Denied: {}", reason)),
                    });
                }
                oneai_core::ApprovalResponse::Approved { modified_args } => {
                    let final_args = modified_args.unwrap_or_else(|| args.clone());
                    let output = tool.execute(final_args).await?;
                    state.conversation.add_message(Message::tool_result(
                        format!("graph_tool_{}", tool_name),
                        output.content.clone(),
                    ));
                    return Ok(oneai_workflow::ActionResult {
                        output: output.content,
                        error: output.error,
                    });
                }
                _ => {
                    let output = tool.execute(args.clone()).await?;
                    state.conversation.add_message(Message::tool_result(
                        format!("graph_tool_{}", tool_name),
                        output.content.clone(),
                    ));
                    return Ok(oneai_workflow::ActionResult {
                        output: output.content,
                        error: output.error,
                    });
                }
            }
        }

        // Standard or Read permission — execute directly
        let output = tool.execute(args.clone()).await?;
        state.conversation.add_message(Message::tool_result(
            format!("graph_tool_{}", tool_name),
            output.content.clone(),
        ));

        Ok(oneai_workflow::ActionResult {
            output: output.content,
            error: output.error,
        })
    }

    /// Execute a paradigm switch node — updates state.active_paradigm.
    async fn execute_paradigm_switch(
        &self,
        paradigm: &str,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_workflow::ActionResult> {
        // Update active paradigm
        state.active_paradigm = Some(paradigm.to_string());
        state.parsed_decision = None; // Clear — new inference needed

        // Update conversation with paradigm-specific system prompt
        let paradigm_config = ParadigmConfig::for_paradigm(match paradigm {
            "plan" => ParadigmKind::Plan,
            "reflect" => ParadigmKind::Reflect,
            "explore" => ParadigmKind::Explore,
            _ => ParadigmKind::ReAct,
        });

        // Replace system prompt in conversation
        state.conversation.messages.retain(|m| m.role != Role::System);
        state.conversation.add_message(Message::system(&paradigm_config.system_prompt));

        if !paradigm_config.decision_hint.is_empty() {
            state.conversation.add_message(Message::system(
                format!("[Paradigm switch]: {}", paradigm_config.decision_hint)
            ));
        }

        Ok(oneai_workflow::ActionResult {
            output: format!(
                "{} paradigm activated — system prompt changed, tools filtered to: [{}]",
                paradigm,
                paradigm_config.tool_filter.join(", ")
            ),
            error: None,
        })
    }

    /// Parse an LLM response into a GraphDecision using the AgentLoop's OutputParser.
    ///
    /// This mirrors the `parse_decision()` logic from the AgentLoop, but produces
    /// a `GraphDecision` instead of an `AgentDecision`. The conversion ensures
    /// that edge conditions in the StateGraph use the same decision parsing
    /// as the AgentLoop, making routing consistent and reliable.
    async fn parse_decision(
        &self,
        response: &InferenceResponse,
        state: &mut oneai_workflow::GraphState,
    ) -> Result<oneai_core::GraphDecision> {
        // Use the same parsing logic as AgentLoop.parse_decision()
        let mut tool_calls = Vec::new();
        let mut text_parts = Vec::new();

        for block in &response.message.content {
            match block {
                ContentBlock::ToolCall { id: _, name, args } => {
                    // Check for special internal tools
                    if name == "delegate" {
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
                ContentBlock::Text { text } => {
                    text_parts.push(text.clone());
                }
                ContentBlock::Thinking { .. } => {
                    // Thinking blocks are not part of the decision — skip
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

impl AgentLoopGraphActionExecutor {
    /// Build tool definitions filtered by paradigm config and domain pack.
    ///
    /// This is the same logic as `AgentLoop.build_tool_definitions_for_paradigm()`,
    /// adapted for GraphState's `active_paradigm` and `tool_filter_override`.
    async fn build_tool_definitions_for_state(
        &self,
        tool_filter_override: &Option<Vec<String>>,
        active_paradigm: &Option<String>,
    ) -> Vec<ToolDefinition> {
        let tools_map = self.tools.read().await;

        // Determine tool filter: override > paradigm config > all tools
        let paradigm_config = active_paradigm_to_config(active_paradigm);
        let filtered_tools: Vec<&Arc<dyn Tool>> = if let Some(filter) = tool_filter_override {
            // Override: only include specified tools
            tools_map.values()
                .filter(|tool| filter.contains(&tool.name().to_string()))
                .collect()
        } else if let Some(config) = &paradigm_config {
            if config.tool_filter.is_empty() {
                tools_map.values().collect()
            } else {
                tools_map.values()
                    .filter(|tool| config.tool_filter.contains(&tool.name().to_string()))
                    .collect()
            }
        } else {
            tools_map.values().collect()
        };

        // Apply domain pack tool decorators
        if let Some(domain) = &self.domain_pack {
            filtered_tools.iter().map(|tool| {
                let decorator = domain.find_decorator(tool.name());
                match decorator {
                    Some(dec) => {
                        let description = dec.description_override.as_deref()
                            .unwrap_or_else(|| tool.description());
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
            filtered_tools.iter().map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters_schema: tool.parameters_schema(),
            }).collect()
        }
    }
}

/// Convert a string paradigm name to ParadigmConfig.
fn active_paradigm_to_config(paradigm: &Option<String>) -> Option<ParadigmConfig> {
    paradigm.as_ref().map(|p| ParadigmConfig::for_paradigm(match p.as_str() {
        "plan" => ParadigmKind::Plan,
        "reflect" => ParadigmKind::Reflect,
        "explore" => ParadigmKind::Explore,
        _ => ParadigmKind::ReAct,
    }))
}

#[cfg(test)]
mod smart_router_tests {
    use super::*;

    #[test]
    fn test_route_cat_to_read_file() {
        let call = ToolCallRequest {
            id: "test-1".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "cat src/main.rs"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "read_file");
        assert_eq!(routed.args["path"], "src/main.rs");
    }

    #[test]
    fn test_route_ls_to_list_directory() {
        let call = ToolCallRequest {
            id: "test-2".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "ls -la src/"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "list_directory");
        assert_eq!(routed.args["path"], "src/");
    }

    #[test]
    fn test_route_grep_to_grep_tool() {
        let call = ToolCallRequest {
            id: "test-3".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "grep -rn fn main src/"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "grep");
        assert_eq!(routed.args["pattern"], "fn");
        assert_eq!(routed.args["path"], "main"); // "main" becomes path since it's second non-option arg
    }

    #[test]
    fn test_route_find_to_glob() {
        let call = ToolCallRequest {
            id: "test-4".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "find . -name *.rs"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "glob");
        assert_eq!(routed.args["pattern"], "*.rs"); // Quotes removed
        assert_eq!(routed.args["path"], ".");
    }

    #[test]
    fn test_no_redirect_for_git() {
        let call = ToolCallRequest {
            id: "test-5".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "git status"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // No redirect
    }

    #[test]
    fn test_no_redirect_for_cargo() {
        let call = ToolCallRequest {
            id: "test-6".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "cargo test"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // No redirect
    }

    #[test]
    fn test_route_pwd_to_environment() {
        let call = ToolCallRequest {
            id: "test-7".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "pwd"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "environment");
    }

    #[test]
    fn test_route_tree_to_list_directory() {
        let call = ToolCallRequest {
            id: "test-8".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "tree src/"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "list_directory");
        assert_eq!(routed.args["path"], "src/");
    }

    #[test]
    fn test_no_redirect_for_echo_write() {
        let call = ToolCallRequest {
            id: "test-9".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "echo 'hello' > /tmp/test"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // echo with > should stay as shell
    }

    #[test]
    fn test_no_redirect_for_non_shell() {
        let call = ToolCallRequest {
            id: "test-10".to_string(),
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "src/main.rs"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "read_file"); // Non-shell calls pass through
    }

    #[test]
    fn test_route_curl_simple_to_web_fetch() {
        let call = ToolCallRequest {
            id: "test-11".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "curl https://example.com/api"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "web_fetch");
        assert_eq!(routed.args["url"], "https://example.com/api");
    }

    #[test]
    fn test_no_redirect_for_curl_post() {
        let call = ToolCallRequest {
            id: "test-12".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "curl -X POST -d 'data' https://api.com"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // POST request stays as shell
    }

    #[test]
    fn test_route_file_to_read_file() {
        let call = ToolCallRequest {
            id: "test-13".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": "file src/main.rs"}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "read_file");
        assert_eq!(routed.args["path"], "src/main.rs");
    }

    #[test]
    fn test_no_redirect_for_empty_command() {
        let call = ToolCallRequest {
            id: "test-14".to_string(),
            name: "shell".to_string(),
            args: serde_json::json!({"command": ""}),
        };
        let routed = AgentLoop::route_shell_to_specialized(call);
        assert_eq!(routed.name, "shell"); // Empty command stays as shell
    }
}