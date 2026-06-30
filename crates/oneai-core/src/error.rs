//! Core error types for the OneAI framework.

use thiserror::Error;

/// The top-level error type for all OneAI operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OneAIError {
    /// Errors from LLM provider interactions.
    #[error("Provider error: {0}")]
    Provider(String),

    /// Errors from output parsing.
    #[error("Parser error: {0}")]
    Parser(ParserError),

    /// Errors from tool execution.
    #[error("Tool error: {0}")]
    Tool(String),

    /// Errors from memory operations.
    #[error("Memory error: {0}")]
    Memory(String),

    /// Errors from workflow operations.
    #[error("Workflow error: {0}")]
    Workflow(String),

    /// Errors from agent execution.
    #[error("Agent error: {0}")]
    Agent(String),

    /// Errors from skill selection.
    #[error("Skill error: {0}")]
    Skill(String),

    /// Errors from scheduling.
    #[error("Scheduler error: {0}")]
    Scheduler(String),

    /// Errors from persistence / checkpointing.
    #[error("Persistence error: {0}")]
    Persistence(String),

    /// Errors from RAG operations.
    #[error("RAG error: {0}")]
    Rag(String),

    /// Configuration errors.
    #[error("Config error: {0}")]
    Config(String),

    /// Errors from the unified interaction gate (PreInfer/PostInfer/ToolApproval/
    /// PlanDecision/PlanReview).
    #[error("Interaction error: {0}")]
    Interaction(InteractionError),

    /// Serialization / deserialization errors.
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Network / HTTP errors.
    #[error("Network error: {0}")]
    Network(String),

    /// Timeout errors.
    #[error("Timeout: {0}")]
    Timeout(String),

    /// Platform capability errors (features not available on current platform).
    #[error("Platform error: {0}")]
    Platform(String),

    /// WASM sandbox execution errors.
    #[error("WASM error: {0}")]
    Wasm(String),

    /// Embedding service errors (API call failures, dimension mismatch, etc.).
    #[error("Embedding error: {0}")]
    Embedding(String),

    /// Evaluation errors (eval suite not found, runner errors, etc.).
    #[error("Eval error: {0}")]
    Eval(String),

    /// Usage tracking errors (usage recording/retrieval failures, etc.).
    #[error("Usage error: {0}")]
    Usage(String),

    /// Rate limiting errors (rate limit exceeded, limiter configuration errors, etc.).
    #[error("Rate limit error: {0}")]
    RateLimit(String),

    /// Provider fallback errors (all providers exhausted, fallback chain failed, etc.).
    #[error("Fallback error: {0}")]
    Fallback(String),

    /// Token counting errors (context overflow, estimation failures, etc.).
    #[error("Token counting error: {0}")]
    TokenCount(String),

    /// Team coordination errors (team validation, strategy execution failures, etc.).
    #[error("Team coordination error: {0}")]
    Team(String),

    /// Handoff protocol errors (handoff validation, depth exceeded, target not found, etc.).
    #[error("Handoff error: {0}")]
    Handoff(String),

    /// Swarm orchestration errors (swarm validation, routing failures, agent not found, etc.).
    #[error("Swarm error: {0}")]
    Swarm(String),

    /// Generic errors with context.
    #[error("{0}")]
    Other(String),
}

/// Parser-specific errors, used by the 3-layer parsing defense.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ParserError {
    /// Layer 2: JSON could not be repaired.
    #[error("Fuzzy JSON repair failed: {0}")]
    FuzzyRepairFailed(String),

    /// Layer 3: Self-correction loop exhausted max retries.
    #[error("Self-correction fallback exhausted after {retries} retries: {reason}")]
    FallbackExhausted { retries: usize, reason: String },

    /// Constrained decoding was requested but not supported by provider.
    #[error("Constrained decoding not supported: {0}")]
    ConstrainedNotSupported(String),

    /// Tool call format could not be parsed.
    #[error("Tool call format error: {0}")]
    ToolCallFormat(String),

    /// General parsing error.
    #[error("Parse error: {0}")]
    General(String),
}

/// Errors from the unified [`InteractionGate`](crate::traits::InteractionGate).
#[derive(Debug, Error)]
pub enum InteractionError {
    /// The interaction channel was dropped before a response arrived (the UI
    /// handler exited or never consumed the request).
    #[error("Interaction response channel dropped")]
    ChannelDropped,

    /// No interaction gate is configured.
    #[error("No interaction gate configured")]
    NotConfigured,

    /// The interaction timed out waiting for the application layer.
    #[error("Interaction timed out after {seconds}s")]
    Timeout { seconds: u64 },
}

/// Convenience type alias for Results in the OneAI framework.
pub type Result<T> = std::result::Result<T, OneAIError>;