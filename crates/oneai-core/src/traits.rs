//! Core trait definitions for the OneAI framework.
//!
//! These traits define the primary abstractions that all components implement:
//! - `LlmProvider`: LLM inference (streaming + non-streaming)
//! - `Tool`: Tool registration and execution
//! - `MemoryStore`: Short-term and long-term memory
//! - `SkillProvider`: Skill selection and management
//! - `PlatformTool`: Platform-specific tool extension
//! - `ApprovalGate`: Human-machine collaboration approval
//! - `OutputParser`: 3-layer output parsing defense
//! - `StateReducer`: ScopeState reduction for parallel agents
//! - `TaskScheduler`: Platform-independent task scheduling
//! - `StatePersistence`: Checkpoint save/load for agent state recovery

use crate::error::Result;
use crate::types::*;
use crate::platform::Platform;
use crate::types::{HookPoint, HookResult, HookContext};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

// ─── LlmProvider ──────────────────────────────────────────────────────────────

/// The primary abstraction for all LLM interactions.
///
/// Implementations handle provider-specific protocol translation (OpenAI, Anthropic, Ollama, etc.)
/// and expose a uniform interface for inference and streaming.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Perform a complete (non-streaming) inference request.
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse>;

    /// Perform a streaming inference request, returning an SSE stream.
    ///
    /// The stream yields `InferenceStreamChunk` items as they arrive from the provider.
    /// The final chunk will have `is_final = true` and include token usage.
    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>>;

    /// Query the capabilities of the connected model.
    fn capabilities(&self) -> ModelCapability;

    /// Get the model configuration.
    fn config(&self) -> &ModelConfig;
}

// ─── Tool ─────────────────────────────────────────────────────────────────────

/// Unified interface for all tools — local, MCP, and platform-specific.
///
/// Each tool has a name, description, parameter schema, and risk level.
/// High-risk tools must pass through the `ApprovalGate` before execution.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's unique name.
    fn name(&self) -> &str;

    /// Human-readable description of what the tool does.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// The risk level of this tool's operations.
    fn risk_level(&self) -> RiskLevel;

    /// Execute the tool with the given arguments.
    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput>;
}

// ─── MemoryStore ──────────────────────────────────────────────────────────────

/// Abstraction for both short-term and long-term memory.
///
/// Short-term memory uses sliding window with in-memory storage.
/// Long-term memory uses vector storage with hybrid scoring
/// (semantic similarity + temporal proximity).
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Store a new memory entry.
    async fn store(&self, entry: MemoryEntry) -> Result<()>;

    /// Retrieve memory entries matching the query.
    async fn retrieve(&self, query: &MemoryQuery, top_k: usize) -> Result<Vec<MemoryEntry>>;

    /// Compress memory entries when they exceed a threshold.
    /// Returns the entries that were summarized/removed.
    async fn compress(&self, threshold: usize) -> Result<Vec<MemoryEntry>>;

    /// Clear all stored entries.
    async fn clear(&self) -> Result<()>;
}

// ─── SkillProvider ────────────────────────────────────────────────────────────

/// Skill selection and management.
///
/// The SKILL Selector uses lightweight vector/keyword matching to dynamically
/// inject the most relevant skill descriptions into the agent's context.
/// Skills are progressively disclosed and auto-unloaded when the topic changes.
#[async_trait]
pub trait SkillProvider: Send + Sync {
    /// Select the most relevant skills for a given user input.
    async fn select_skills(&self, user_input: &str, top_k: usize) -> Result<Vec<SkillDescriptor>>;

    /// Register a new skill.
    fn register_skill(&self, skill: SkillDescriptor) -> Result<()>;

    /// Remove a skill by name.
    fn remove_skill(&self, name: &str) -> Result<()>;

    /// List all registered skills.
    fn list_skills(&self) -> Result<Vec<SkillDescriptor>>;
}

// ─── PlatformTool ─────────────────────────────────────────────────────────────

/// Platform-specific tool interface.
///
/// Extends the base `Tool` trait with platform identification.
/// Platform tools are implemented per platform in the `platforms/` directory.
pub trait PlatformTool: Tool {
    /// The platform this tool is designed for.
    fn platform(&self) -> Platform;
}

// ─── ApprovalGate ─────────────────────────────────────────────────────────────

/// Human-machine collaboration approval gate.
///
/// When a high-risk tool is triggered, the approval gate suspends execution
/// and sends an `ApprovalRequest` to the upper layer (UI). The process
/// resumes after the user responds. This avoids callback hell by using
/// a suspend/resume pattern with state preserved in the persistence layer.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// Request approval for a high-risk tool execution.
    ///
    /// This method blocks until the user responds.
    async fn request_approval(&self, request: ApprovalRequest) -> Result<ApprovalResponse>;
}

// ─── OutputParser ─────────────────────────────────────────────────────────────

/// 3-layer output parsing defense trait.
///
/// Layer 1: Constrained decoding (BNF grammar) — guarantees correct format at generation.
/// Layer 2: Fuzzy JSON repair — repairs malformed output (bracket closing, regex extraction).
/// Layer 3: Fallback self-correction — re-feeds error message to model for re-generation.
#[async_trait]
pub trait OutputParser: Send + Sync {
    /// Parse raw model output into structured content blocks.
    ///
    /// Applies the 3-layer defense automatically:
    /// 1. If constrained decoding is active, the output is already correct (Layer 1).
    /// 2. If not, attempt fuzzy repair (Layer 2).
    /// 3. If repair fails, trigger fallback self-correction (Layer 3).
    async fn parse<'a>(&self, raw_output: &str, schema: Option<&'a serde_json::Value>) -> Result<ParsedOutput>;
}

// ─── ConstrainedDecoder ───────────────────────────────────────────────────────

/// Layer 1: Constrained decoding trait.
///
/// Implementations activate BNF/JSON Schema grammar constraints on providers
/// that support them (LiteRT-LM, Ollama, llama.cpp).
pub trait ConstrainedDecoder: Send + Sync {
    /// Whether constrained decoding is available for the current provider.
    fn is_available(&self) -> bool;

    /// Apply constrained decoding to an inference request.
    fn apply_constraint(&self, req: &mut InferenceRequest, grammar: &str) -> Result<()>;
}

// ─── StateReducer ─────────────────────────────────────────────────────────────

/// Merges sub-agent reductions (ScopeState) back into the global state.
///
/// Implements the MVI/Redux pattern for parallel agent execution.
/// Sub-agents run in isolated Sandbox Scopes with read-only global memory;
/// their results are merged back via this reducer.
pub trait StateReducer: Send + Sync {
    /// Merge a set of reductions into the global state.
    fn reduce(&self, global: &mut GlobalState, reductions: Vec<Reduction>) -> Result<()>;
}

// ─── GlobalState ──────────────────────────────────────────────────────────────

/// The global state shared across all agents in a session.
///
/// Contains the main conversation, memory entries, and shared context variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalState {
    /// The main conversation.
    pub conversation: Conversation,

    /// Global memory entries.
    pub memory: Vec<MemoryEntry>,

    /// Shared context variables (key-value pairs).
    pub context: HashMap<String, String>,

    /// Results from completed sub-agent steps.
    pub step_results: HashMap<String, ContentBlock>,
}

impl GlobalState {
    /// Create a new empty global state.
    pub fn new() -> Self {
        Self {
            conversation: Conversation::new(),
            memory: Vec::new(),
            context: HashMap::new(),
            step_results: HashMap::new(),
        }
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Reduction ────────────────────────────────────────────────────────────────

/// Describes how a sub-agent's result should be merged into the global state.
///
/// Sub-agents produce reductions in their isolated ScopeState;
/// the StateReducer applies these to the global state after parallel execution completes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Reduction {
    /// Append a memory entry to global memory.
    AppendMemory { entry: MemoryEntry },

    /// Update a shared context variable.
    UpdateContext { key: String, value: String },

    /// Set the result for a specific plan step.
    SetResult { step_id: String, result: ContentBlock },
}

// ─── TaskScheduler ────────────────────────────────────────────────────────────

/// Platform-independent task scheduling.
///
/// Core layer provides a standard async delay trigger.
/// Platform adapters implement native scheduling:
/// - Android: WorkManager
/// - HarmonyOS: WorkScheduler
/// - Desktop: Daemon process
#[async_trait]
pub trait TaskScheduler: Send + Sync {
    /// Schedule a one-shot task with a delay.
    async fn schedule_one_shot(&self, task: ScheduledTask, delay: std::time::Duration) -> Result<TaskHandle>;

    /// Schedule a periodic task with an interval.
    async fn schedule_periodic(&self, task: ScheduledTask, interval: std::time::Duration) -> Result<TaskHandle>;

    /// Cancel a scheduled task.
    async fn cancel(&self, handle: &TaskHandle) -> Result<()>;
}

// ─── ScheduledTask / TaskHandle ───────────────────────────────────────────────

/// A task to be scheduled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    /// Unique task identifier.
    pub id: String,

    /// Human-readable task name.
    pub name: String,

    /// The task payload (serialized agent state or workflow config).
    pub payload: String,

    /// Task metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// A handle to a scheduled task (for cancellation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHandle {
    /// The task ID.
    pub task_id: String,

    /// Platform-specific scheduling identifier.
    pub platform_handle: String,
}

// ─── StatePersistence ─────────────────────────────────────────────────────────

/// State persistence for checkpointing and recovery.
///
/// Used to save agent/workflow state when interrupted,
/// and recover it when the session resumes.
#[async_trait]
pub trait StatePersistence: Send + Sync {
    /// Save a checkpoint of the current agent state.
    async fn save_checkpoint(&self, state: &AgentState) -> Result<String>;

    /// Load a checkpoint by ID.
    async fn load_checkpoint(&self, id: &str) -> Result<AgentState>;

    /// List all available checkpoints.
    async fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>>;

    /// Delete a checkpoint by ID.
    async fn delete_checkpoint(&self, id: &str) -> Result<()>;
}

// ─── AgentState / CheckpointInfo ──────────────────────────────────────────────

/// The full state of an agent session, for checkpointing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    /// Unique session identifier.
    pub session_id: String,

    /// The global state at the time of checkpoint.
    pub global_state: GlobalState,

    /// The agent paradigm that was active.
    pub active_paradigm: String,

    /// The step in the workflow/plan that was being executed.
    #[serde(default)]
    pub active_step: Option<String>,

    /// Timestamp of the checkpoint.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Metadata about a saved checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointInfo {
    /// The checkpoint ID.
    pub id: String,

    /// The session ID this checkpoint belongs to.
    pub session_id: String,

    /// When the checkpoint was created.
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Brief description of what was checkpointed.
    pub description: String,
}

// ─── LifecycleHook ────────────────────────────────────────────────────────────

/// A lifecycle hook that runs at specific points in the agent loop.
///
/// Lifecycle hooks are the evolution from ApprovalGate's "围栏式安全"
/// (gate-based: approve/deny before execution) to "生命周期安全"
/// (event-driven: allow/deny/modify at every lifecycle stage).
///
/// Inspired by Claude Code's hooks system (PreToolUse/PostToolUse/Notification/Stop),
/// OneAI extends this to include inference lifecycle hooks (PreInfer/PostInfer).
///
/// Hooks can:
/// - **Allow**: Proceed without changes (audit/logging hooks)
/// - **Deny**: Block the action (safety/policy hooks)
/// - **Modify**: Transform the parameters (constraint enforcement hooks)
///
/// Multiple hooks can be registered at the same point. They execute in
/// registration order. For PreToolUse: if any hook returns Deny, the overall
/// result is Deny; if any hook returns Modify, the last Modify's args win.
#[async_trait]
pub trait LifecycleHook: Send + Sync {
    /// The hook points where this hook should be triggered.
    /// A hook can register at multiple points (e.g., a logging hook
    /// at both PreToolUse and PostToolUse).
    fn points(&self) -> Vec<HookPoint>;

    /// Run the hook at the given context.
    /// Returns a HookResult indicating whether to allow, deny, or modify.
    async fn run(&self, context: HookContext) -> HookResult;

    /// Unique name for this hook (for logging/debugging/identification).
    fn name(&self) -> &str;
}

// ─── VectorStore ──────────────────────────────────────────────────────────────

/// Abstraction for vector storage, allowing swap between embedded and remote implementations.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Store a vector with associated metadata.
    async fn upsert(&self, id: &str, embedding: Vec<f32>, metadata: HashMap<String, String>) -> Result<()>;

    /// Search for vectors similar to the query embedding.
    async fn search(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<VectorSearchResult>>;

    /// Delete a vector by ID.
    async fn delete(&self, id: &str) -> Result<()>;
}

/// A result from vector similarity search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorSearchResult {
    /// The ID of the matching vector.
    pub id: String,

    /// Similarity score (0.0 to 1.0).
    pub score: f32,

    /// Associated metadata.
    pub metadata: HashMap<String, String>,
}

// ─── Re-export serde_json for trait definitions ──────────────────────────────

use serde::{Deserialize, Serialize};
use std::collections::HashMap;