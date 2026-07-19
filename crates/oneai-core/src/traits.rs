//! Core trait definitions for the OneAI framework.
//!
//! These traits define the primary abstractions that all components implement:
//! - `LlmProvider`: LLM inference (streaming + non-streaming)
//! - `Tool`: Tool registration and execution
//! - `MemoryStore`: Short-term and long-term memory
//! - `SkillProvider`: Skill selection and management
//! - `PlatformTool`: Platform-specific tool extension
//! - `InteractionGate`: Human-machine collaboration at every loop decision point
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

    /// Probe the provider's own model-metadata endpoint for the context window.
    ///
    /// This is the L2 dynamic-detection layer of OneAI's 3-layer context-window
    /// resolution (user config > provider probe > built-in library). Implementations
    /// query endpoints like Ollama `/api/show`, Anthropic `/v1/models/{id}`, or
    /// Gemini `models.get` and return the discovered window size in tokens.
    ///
    /// The default returns `None` so the resolver falls through to the built-in
    /// static library. Probing must be best-effort — network/auth failures
    /// return `None` rather than erroring, so inference is never blocked by a
    /// metadata-endpoint outage.
    async fn probe_context_window(&self) -> Option<u32> {
        None
    }

    /// Whether this backend benefits from constrained/structured-output decoding.
    ///
    /// Encodes a **cost/benefit judgment, not raw capability**: cloud SOTA backends
    /// return `false` (constrained decoding hurts reasoning quality on strong models
    /// for little gain — they already emit valid JSON reliably); local/self-hosted
    /// backends (Ollama small models, vLLM/llama.cpp) return `true` (their invalid-JSON
    /// rate is high enough that grammar-constrained decoding is net positive).
    ///
    /// Honored only when the agent's `ConstrainedOutputPolicy` is `Auto`. Post-hoc
    /// `StructuredOutputConfig` validation + `ModelRetry` runs regardless of this flag.
    /// Default `false` so cloud providers opt out unless they explicitly override.
    fn prefers_constrained_output(&self) -> bool {
        false
    }
}

// ─── Tool ─────────────────────────────────────────────────────────────────────

/// Unified interface for all tools — local, MCP, and platform-specific.
///
/// Each tool has a name, description, parameter schema, and risk level.
/// High-risk tools must pass through the `InteractionGate` (ToolApproval) before execution.
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

// ─── InteractionGate ──────────────────────────────────────────────────────────

/// Unified interaction gate — the single surface for every "agent loop suspends
/// → asks the application layer → resumes with a reply" decision point.
///
/// Covers tool approval (PreInfer/PostInfer/ToolApproval), planning tradeoffs
/// (PlanDecision), and final plan confirmation (PlanReview). The application
/// layer decides per-point whether to actually call back to the UI via
/// [`enabled`](Self::enabled); points that return `false` are short-circuited by
/// the loop with zero latency (no lock taken, no channel send).
///
/// Implementations:
/// - `NoopInteractionGate` — every point `enabled()==false`; the zero-latency
///   default.
/// - `ChannelInteractionGate` — mpsc+oneshot bridge to an external UI thread,
///   configurable per-point via `InteractionGateConfig`.
/// - `ThresholdInteractionGate` — low-risk tools auto-proceed, the rest go to
///   the channel.
#[async_trait]
pub trait InteractionGate: Send + Sync {
    /// Block at the decision point until the application layer replies.
    async fn request(&self, req: InteractionRequest) -> Result<InteractionResponse>;

    /// Whether this point should call back to the application layer.
    ///
    /// Returning `false` lets the loop skip the entire interaction block — no
    /// lock acquisition, no channel send, no allocation. This is the lever that
    /// lets a TUI enable `PlanDecision`/`PlanReview`/`ToolApproval` while leaving
    /// `PreInfer`/`PostInfer` off (no per-iteration interruption). The default
    /// returns `true`; `NoopInteractionGate` overrides it to `false` for all
    /// points.
    fn enabled(&self, _point: InteractionPoint) -> bool {
        true
    }
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
/// Lifecycle hooks are the evolution from InteractionGate's "围栏式安全"
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

// ─── MemoryPersistence ─────────────────────────────────────────────────────

/// Trait for persisting and restoring memory and conversation state.
///
/// Enables SQLite (or other) backends to store STM entries, LTM entries,
/// and conversation history, allowing session resume and knowledge accumulation
/// across application restarts.
///
/// This addresses the critical gap where all memory is purely in-memory
/// (HashMap, VecDeque) and lost on restart. With a MemoryPersistence backend,
/// the agent framework becomes truly usable for production scenarios.
#[async_trait]
pub trait MemoryPersistence: Send + Sync {
    /// Save STM entries for a session (bulk operation).
    async fn save_stm(&self, session_id: &str, entries: &[MemoryEntry]) -> Result<()>;

    /// Load STM entries for a session (ordered by position in the sliding window).
    async fn load_stm(&self, session_id: &str) -> Result<Vec<MemoryEntry>>;

    /// Clear STM entries for a session.
    async fn clear_stm(&self, session_id: &str) -> Result<()>;

    /// Save a single LTM entry.
    async fn save_ltm(&self, entry: &MemoryEntry) -> Result<()>;

    /// Load a LTM entry by ID.
    async fn load_ltm(&self, id: &str) -> Result<Option<MemoryEntry>>;

    /// Search LTM by keyword (case-insensitive substring match).
    async fn search_ltm_keyword(&self, keyword: &str, top_k: usize) -> Result<Vec<MemoryEntry>>;

    /// Search LTM by embedding (cosine similarity against stored embeddings).
    ///
    /// Loads entries with embeddings from storage, computes brute-force cosine
    /// similarity in Rust (acceptable for <10K entries), and returns the top_k
    /// most similar entries with their scores.
    async fn search_ltm_embedding(&self, query: &[f32], top_k: usize) -> Result<Vec<(MemoryEntry, f32)>>;

    /// Delete a LTM entry by ID.
    async fn delete_ltm(&self, id: &str) -> Result<()>;

    /// Clear all LTM entries.
    async fn clear_ltm(&self) -> Result<()>;

    /// Save a conversation (message history for multi-turn sessions).
    async fn save_conversation(&self, id: &str, conversation: &Conversation) -> Result<()>;

    /// Load a conversation by ID.
    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>>;

    /// List all saved conversations (metadata only, not full message history).
    async fn list_conversations(&self) -> Result<Vec<SessionInfo>>;

    /// Delete a conversation and its associated STM entries by ID.
    async fn delete_conversation(&self, id: &str) -> Result<()>;

    // ─── MemoryFact persistence (core/archival tiers) ──────────────────────
    //
    // These back the DomainPack MemoryProfile layer's durable facts. Default
    // impls are no-ops so existing backends keep compiling; the SQLite backend
    // overrides them to persist facts across restarts ("越用越好用").

    /// Upsert a fact (conflict-resolved by user_id+subject+predicate).
    async fn store_fact(&self, _fact: &MemoryFact) -> Result<()> {
        Ok(()) // no-op default
    }

    /// Load all facts for a user (cross-session habits) and/or session.
    async fn load_facts(&self, _user_id: &str, _session_id: &str) -> Result<Vec<MemoryFact>> {
        Ok(Vec::new()) // no-op default
    }
}

// ─── DiscardedSink ──────────────────────────────────────────────────────────

/// Sink for messages discarded during context compression.
///
/// The "压缩即不丢" closure: when `ContextBudgetManager::compress` summarizes
/// away older turns, the discarded `Message`s are handed to this sink before
/// being dropped from the live conversation. A typical implementation persists
/// them as a turn-scoped conversation snapshot (via `MemoryPersistence::
/// save_conversation`) so they remain available for resume, audit, and on-demand
/// `memory_search` fallback — raw transcript is not lost even though it leaves
/// the working context.
///
/// Compression-coupled fact extraction (turning discarded turns into durable
/// `MemoryFact`s) runs *inside* the compressor; this sink is the complementary
/// raw-transcript archive. Failures must not propagate — a bad sink must not
/// break the compression path.
#[async_trait]
pub trait DiscardedSink: Send + Sync {
    /// Archive a batch of discarded messages, scoped to `session_id`.
    async fn archive_discarded(&self, session_id: &str, discarded: Vec<Message>) -> Result<()>;
}

// ─── SessionInfo ────────────────────────────────────────────────────────────

/// Metadata about a saved conversation session.
///
/// Used by `MemoryPersistence::list_conversations()` to return summary
/// information without loading the full message history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionInfo {
    /// The session/conversation ID.
    pub id: String,

    /// When the session was first created.
    pub created_at: chrono::DateTime<chrono::Utc>,

    /// When the session was last updated (last message timestamp).
    pub updated_at: chrono::DateTime<chrono::Utc>,

    /// Number of messages in the conversation.
    pub message_count: usize,

    /// A short title derived from the first user message (truncated, whitespace
    /// collapsed). `None` when the conversation has no user message yet. Used by
    /// foreign UIs (e.g. the Android drawer) to label session rows without
    /// loading full histories.
    #[serde(default)]
    pub title: Option<String>,
}

impl SessionInfo {
    /// Create a new SessionInfo with the given fields (title = None).
    pub fn new(
        id: String,
        created_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
        message_count: usize,
    ) -> Self {
        Self { id, created_at, updated_at, message_count, title: None }
    }

    /// Create a new SessionInfo with an explicit title (first-user-message
    /// preview). The caller is responsible for truncating/collapsing the title.
    pub fn with_title(
        id: String,
        created_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
        message_count: usize,
        title: Option<String>,
    ) -> Self {
        Self { id, created_at, updated_at, message_count, title }
    }
}

// ─── Re-export serde_json for trait definitions ──────────────────────────────

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── EmbeddingService ──────────────────────────────────────────────────────

/// Embedding service — generates vector embeddings from text.
///
/// The primary interface for embedding generation. Implementations
/// use different backends (local ONNX, Ollama API, OpenAI API, Anthropic API).
///
/// When integrated into DocumentIndex, the service is called automatically
/// during document insertion — each chunk's embedding is computed
/// and stored in the vector store without manual intervention.
///
/// When integrated into MemoryManager, the service is called automatically
/// during `add()` and `inject_ltm_context()` — embeddings are computed
/// for each memory entry, enabling true semantic search in LTM.
///
/// Concrete implementations live in `oneai-rag`:
/// - `OpenAIEmbeddingService` — OpenAI text-embedding API (cloud, high quality)
/// - `VoyageEmbeddingService` — Voyage embedding API (`api.voyageai.com`, `VOYAGE_API_KEY`)
/// - `OllamaEmbeddingService` — Ollama local embedding API (local, no API key needed)
/// - `FastEmbedService` — local ONNX model via fastembed crate
#[async_trait]
pub trait EmbeddingService: Send + Sync {
    /// Generate an embedding for a single text string.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Generate embeddings for multiple text strings in a batch.
    ///
    /// Batch embedding is more efficient than individual calls
    /// because it amortizes the model inference overhead.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Get the embedding model being used.
    fn model(&self) -> EmbeddingModel;

    /// The provider's effective max input size, in UTF-8 bytes used as a
    /// conservative proxy for tokens (token count ≤ UTF-8 byte length).
    ///
    /// `None` means "no enforced limit"; the chunk splitter skips splitting
    /// in that case. Providers with a known per-model cap override this.
    fn max_input_tokens(&self) -> Option<usize> {
        None
    }

    /// Get the embedding dimension.
    fn dimension(&self) -> usize {
        let dim = self.model().dimension();
        if dim == 0 {
            0 // Runtime-determined models (Ollama) — use actual_dimension()
        } else {
            dim
        }
    }

    /// Get the actual embedding dimension by generating a test embedding.
    ///
    /// This is needed for models like Ollama where the dimension isn't
    /// known until runtime. For models with a fixed dimension, this
    /// returns the known value without making an API call.
    async fn actual_dimension(&self) -> Result<usize> {
        let dim = self.model().dimension();
        if dim > 0 {
            Ok(dim)
        } else {
            let test = self.embed("test").await?;
            Ok(test.len())
        }
    }

    /// Health check — verify the embedding service is reachable and functional.
    ///
    /// Generates a tiny test embedding to verify connectivity and correctness.
    /// Returns Ok(()) if the service is healthy, Err with details otherwise.
    async fn health_check(&self) -> Result<()> {
        let embedding = self.embed("health check").await?;
        if embedding.is_empty() {
            return Err(crate::error::OneAIError::Embedding("Embedding service returned empty vector".to_string()));
        }
        for val in &embedding {
            if !val.is_finite() {
                return Err(crate::error::OneAIError::Embedding("Embedding service returned non-finite values".to_string()));
            }
        }
        Ok(())
    }
}

// ─── EmbeddingModel ─────────────────────────────────────────────────────────

/// An embedding model, identified by its canonical model-name string.
///
/// Newtype over `String`: adding a new model never requires changing an enum
/// — just pass its name string, e.g. `EmbeddingModel::new("voyage-3")`. Known
/// dimensions are looked up from [`KNOWN_EMBEDDING_DIMENSIONS`]; names absent
/// from the table return `0` and are resolved at runtime via
/// [`EmbeddingService::actual_dimension`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EmbeddingModel(pub String);

impl EmbeddingModel {
    /// Create a model identifier from any string-like input.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The canonical model-name string sent to provider APIs.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Look up the known vector dimension for this model.
    ///
    /// Returns `0` for models whose dimension is determined at runtime (e.g.
    /// Ollama models, or names absent from [`KNOWN_EMBEDDING_DIMENSIONS`]);
    /// callers should then use [`EmbeddingService::actual_dimension`].
    pub fn dimension(&self) -> usize {
        KNOWN_EMBEDDING_DIMENSIONS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&self.0))
            .map(|(_, d)| *d)
            .unwrap_or(0)
    }

    // ── Built-in convenience constructors ──────────────────────────────────

    /// `text-embedding-3-small` (OpenAI, 1536-dim).
    pub fn openai_small() -> Self {
        Self::new("text-embedding-3-small")
    }
    /// `text-embedding-3-large` (OpenAI, 3072-dim).
    pub fn openai_large() -> Self {
        Self::new("text-embedding-3-large")
    }
    /// `voyage-3` (Voyage, 1024-dim).
    pub fn voyage3() -> Self {
        Self::new("voyage-3")
    }
    /// `voyage-3-lite` (Voyage, 512-dim).
    pub fn voyage3_lite() -> Self {
        Self::new("voyage-3-lite")
    }
    /// `nomic-embed-text` (Ollama, runtime-dim).
    pub fn nomic_embed_text() -> Self {
        Self::new("nomic-embed-text")
    }
    /// `all-MiniLM-L6-v2` (FastEmbed, 384-dim).
    pub fn allminilm_l6_v2() -> Self {
        Self::new("all-MiniLM-L6-v2")
    }
    /// `bge-base-en-v1.5` (FastEmbed, 768-dim).
    pub fn bge_base_en_v15() -> Self {
        Self::new("bge-base-en-v1.5")
    }
    /// `mixedbread-embed-large-v1` (FastEmbed, 1024-dim).
    pub fn mxbai_embed_large_v1() -> Self {
        Self::new("mixedbread-embed-large-v1")
    }
}

impl From<String> for EmbeddingModel {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl From<&str> for EmbeddingModel {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl std::fmt::Display for EmbeddingModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Static table of known embedding-model dimensions.
///
/// Case-insensitive lookup; names not present resolve to `0` (runtime-probed
/// via [`EmbeddingService::actual_dimension`]).
pub static KNOWN_EMBEDDING_DIMENSIONS: &[(&str, usize)] = &[
    ("all-MiniLM-L6-v2", 384),
    ("bge-base-en-v1.5", 768),
    ("mixedbread-embed-large-v1", 1024),
    ("text-embedding-3-small", 1536),
    ("text-embedding-3-large", 3072),
    ("voyage-3", 1024),
    ("voyage-3-lite", 512),
];

// ─── EmbeddingProvider ──────────────────────────────────────────────────────

/// The embedding provider to use (or [`Auto`][Self::Auto] for zero-config
/// auto-detection).
///
/// `Auto` walks the detection chain (embedding-specific keys, never reusing
/// the LLM provider's key — embedding and chat are separate capabilities) and
/// picks the first available provider; if none is available the resolved
/// service is `None` and memory recall falls back to keyword matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EmbeddingProvider {
    /// Zero-config auto-detection (the default).
    Auto,
    /// OpenAI `text-embedding-3-*` (official API, `OPENAI_API_KEY`).
    OpenAi,
    /// Voyage `voyage-3*` (`api.voyageai.com`, `VOYAGE_API_KEY`).
    Voyage,
    /// Ollama local embedding API (no key; probes `localhost:11434`).
    Ollama,
    /// FastEmbed local ONNX (no key; available when implemented).
    FastEmbed,
    /// OpenAI-compatible relay/gateway (explicit `base_url` + key required).
    OpenAiCompat,
}

impl EmbeddingProvider {
    /// Parse a provider id from its serde form (case-insensitive, accepts
    /// `openai-compat` / `openai_compat` / `openai-compatible`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "openai" => Some(Self::OpenAi),
            "voyage" => Some(Self::Voyage),
            "ollama" => Some(Self::Ollama),
            "fastembed" => Some(Self::FastEmbed),
            "openai-compat" | "openai_compat" | "openai-compatible" => Some(Self::OpenAiCompat),
            _ => None,
        }
    }
}

impl Default for EmbeddingProvider {
    fn default() -> Self {
        Self::Auto
    }
}

impl std::fmt::Display for EmbeddingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Auto => "auto",
            Self::OpenAi => "openai",
            Self::Voyage => "voyage",
            Self::Ollama => "ollama",
            Self::FastEmbed => "fastembed",
            Self::OpenAiCompat => "openai-compat",
        };
        write!(f, "{}", s)
    }
}

// ─── InputType ──────────────────────────────────────────────────────────────

/// Whether an embedding is for a search query or an indexed document.
///
/// Some providers (OpenAI, Voyage) optimize retrieval quality when the input
/// type is declared; providers that ignore it simply do so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum InputType {
    Query,
    Document,
}

// ─── EmbeddingConfig ─────────────────────────────────────────────────────────

/// Configuration for the embedding service.
///
/// The default ([`EmbeddingConfig::default`]) is **zero-config**:
/// `provider = Auto`, so the embedding service is auto-resolved at build time
/// from environment-detectable signals (embedding-specific keys, a reachable
/// local Ollama, etc.). Most users never touch this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EmbeddingConfig {
    /// Which provider to use (`Auto` = detect).
    #[serde(default)]
    pub provider: EmbeddingProvider,
    /// Model-name override (None → the provider's default model).
    #[serde(default)]
    pub model: Option<EmbeddingModel>,
    /// Embedding-specific API key. Sourced, in priority order, from this
    /// field, `ONEAI_EMBEDDING_API_KEY`, or the provider's own env var
    /// (`OPENAI_API_KEY` / `VOYAGE_API_KEY`). **Never** reused from the LLM
    /// provider's key — embedding and chat are separate capabilities.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Base URL override (required for `OpenAiCompat` relays).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Fallback provider used if the primary fails to create or first-call
    /// (build-time + runtime, sharing one `should_continue` classifier).
    #[serde(default)]
    pub fallback: Option<EmbeddingProvider>,
    /// Query/Document input-type hint (OpenAI/Voyage retrieval optimization).
    #[serde(default)]
    pub input_type: Option<InputType>,
    /// Provider output-dimensionality override (OpenAI `dimensions`).
    #[serde(default)]
    pub output_dimensionality: Option<usize>,
    /// Hard cap on input size before byte-bisection chunking kicks in
    /// (None → provider default table).
    #[serde(default)]
    pub max_input_tokens: Option<usize>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::Auto,
            model: None,
            api_key: None,
            base_url: None,
            fallback: None,
            input_type: None,
            output_dimensionality: None,
            max_input_tokens: None,
        }
    }
}

impl EmbeddingConfig {
    /// Zero-config (auto-detect) configuration — the recommended default.
    pub fn auto() -> Self {
        Self::default()
    }

    /// OpenAI embedding (official API, `OPENAI_API_KEY`).
    pub fn openai(api_key: String) -> Self {
        Self {
            provider: EmbeddingProvider::OpenAi,
            api_key: Some(api_key),
            ..Self::default()
        }
    }

    /// Voyage embedding (`api.voyageai.com`, `VOYAGE_API_KEY`).
    pub fn voyage(api_key: String) -> Self {
        Self {
            provider: EmbeddingProvider::Voyage,
            api_key: Some(api_key),
            ..Self::default()
        }
    }

    /// Ollama local embedding (no key; probes `localhost:11434`).
    pub fn ollama() -> Self {
        Self {
            provider: EmbeddingProvider::Ollama,
            ..Self::default()
        }
    }

    /// OpenAI-compatible relay (explicit `base_url` + key required).
    pub fn openai_compat(api_key: String, base_url: String) -> Self {
        Self {
            provider: EmbeddingProvider::OpenAiCompat,
            api_key: Some(api_key),
            base_url: Some(base_url),
            ..Self::default()
        }
    }

    /// FastEmbed local ONNX (no key; available once implemented).
    pub fn fastembed() -> Self {
        Self {
            provider: EmbeddingProvider::FastEmbed,
            ..Self::default()
        }
    }

    /// Builder-style: set the model.
    pub fn with_model(mut self, model: impl Into<EmbeddingModel>) -> Self {
        self.model = Some(model.into());
        self
    }
    /// Builder-style: set the base_url.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }
    /// Builder-style: set the fallback provider.
    pub fn with_fallback(mut self, fallback: EmbeddingProvider) -> Self {
        self.fallback = Some(fallback);
        self
    }
}

// ─── EmbeddingHealthStatus ──────────────────────────────────────────────────

/// Health status report for the embedding service registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EmbeddingHealthStatus {
    /// Primary service model name.
    pub primary_service: String,
    /// Whether the primary service is healthy.
    pub primary_healthy: bool,
    /// Fallback service model name (if configured).
    pub fallback_service: Option<String>,
    /// Whether the fallback service is healthy (if configured).
    pub fallback_healthy: Option<bool>,
    /// Whether caching is enabled.
    pub cache_enabled: bool,
    /// Number of cached embeddings.
    pub cache_size: usize,
}

impl EmbeddingHealthStatus {
    /// Whether the overall embedding system is functional.
    ///
    /// Returns true if either the primary or fallback service is healthy.
    pub fn is_functional(&self) -> bool {
        self.primary_healthy || self.fallback_healthy.unwrap_or(false)
    }

    /// Create a new EmbeddingHealthStatus.
    pub fn new(
        primary_service: String,
        primary_healthy: bool,
        fallback_service: Option<String>,
        fallback_healthy: Option<bool>,
        cache_enabled: bool,
        cache_size: usize,
    ) -> Self {
        Self {
            primary_service,
            primary_healthy,
            fallback_service,
            fallback_healthy,
            cache_enabled,
            cache_size,
        }
    }
}