//! Trace events — timestamped occurrences within spans.
//!
//! Events capture discrete moments like:
//! - Agent thought produced (Thought)
//! - Tool call decision made (Action)
//! - Tool result fed back (Observation)
//! - LLM inference started/completed
//! - Approval request/response
//! - Parser layer attempt/success/fallback
//! - Memory store/retrieve
//! - Checkpoint save/load
//! - Errors

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use chrono::{DateTime, Utc};

// ─── EventKind ───────────────────────────────────────────────────────

/// Event kinds — categorize the type of occurrence being logged.
///
/// Follows the ReAct paradigm naming (Thought/Action/Observation)
/// plus framework-specific events for LLM, tool, approval, parser, etc.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum EventKind {
    // ─── Agent paradigm events ──────────────────────────────────────
    /// Model reasoning/plan — the "Thought" step in ReAct.
    Thought,
    /// Tool call decision — the "Action" step in ReAct.
    Action,
    /// Tool result fed back — the "Observation" step in ReAct.
    Observation,

    // ─── LLM inference events ──────────────────────────────────────
    /// LLM inference request started.
    InferenceStart,
    /// LLM inference completed (with token usage).
    InferenceEnd,
    /// Streaming chunk received.
    StreamingChunk,

    // ─── Tool execution events ─────────────────────────────────────
    /// Tool call initiated.
    ToolCall,
    /// Tool execution completed with result.
    ToolResult,
    /// Tool execution failed with error.
    ToolError,

    // ─── Approval gate events ──────────────────────────────────────
    /// Approval request sent to human (high-risk tool).
    ApprovalRequest,
    /// Approval response received (approve/deny/modify).
    ApprovalResponse,

    // ─── Output parser events ──────────────────────────────────────
    /// Parser attempting a specific layer.
    ParseAttempt,
    /// Parser succeeded on a specific layer.
    ParseSuccess,
    /// Parser fell back to a lower layer (repair or self-correction).
    ParseFallback,

    // ─── Memory events ────────────────────────────────────────────
    /// Memory entry stored.
    MemoryStore,
    /// Memory entries retrieved.
    MemoryRetrieve,

    // ─── Workflow events ──────────────────────────────────────────
    /// Workflow step started.
    WorkflowStepStart,
    /// Workflow step completed.
    WorkflowStepEnd,

    // ─── Checkpoint events ────────────────────────────────────────
    /// Session checkpoint saved.
    CheckpointSave,
    /// Session checkpoint loaded.
    CheckpointLoad,

    // ─── General events ───────────────────────────────────────────
    /// An error occurred.
    Error,
    /// Custom event (user-defined).
    Custom,
}

// ─── TraceEvent ──────────────────────────────────────────────────────

/// A timestamped event within a span.
///
/// Events capture discrete occurrences like "thought produced",
/// "tool denied", "parser fallback triggered". Each event has:
/// - A timestamp (when it happened)
/// - A kind (what category it belongs to)
/// - A name (human-readable identifier)
/// - Attributes (OpenInference-compatible dot-namespace key-value pairs)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    /// Event timestamp (ISO 8601 UTC).
    pub timestamp: DateTime<Utc>,

    /// The event kind.
    pub kind: EventKind,

    /// Event name (human-readable, e.g., "agent.thought", "tool.call").
    pub name: String,

    /// Event attributes (OpenInference namespace conventions).
    /// Keys use dot namespaces: "tool.name", "llm.token_count", etc.
    #[serde(default)]
    pub attributes: HashMap<String, serde_json::Value>,
}

impl TraceEvent {
    /// Create a new event with the given kind and name.
    pub fn new(kind: EventKind, name: &str) -> Self {
        Self {
            timestamp: Utc::now(),
            kind,
            name: name.to_string(),
            attributes: HashMap::new(),
        }
    }

    /// Create an event with kind and name, plus a single attribute.
    pub fn with_attr(kind: EventKind, name: &str, key: &str, value: serde_json::Value) -> Self {
        Self {
            timestamp: Utc::now(),
            kind,
            name: name.to_string(),
            attributes: HashMap::from([(key.to_string(), value)]),
        }
    }

    /// Create an event with kind and name, plus multiple attributes.
    pub fn with_attrs(
        kind: EventKind,
        name: &str,
        attrs: impl IntoIterator<Item = (String, serde_json::Value)>,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            kind,
            name: name.to_string(),
            attributes: attrs.into_iter().collect(),
        }
    }

    /// Add an attribute to this event.
    pub fn add_attr(&mut self, key: &str, value: serde_json::Value) {
        self.attributes.insert(key.to_string(), value);
    }

    /// Convenience: create a Thought event (agent reasoning step).
    pub fn thought(message: &str) -> Self {
        Self::with_attr(EventKind::Thought, "agent.thought", "input.message", serde_json::json!(message))
    }

    /// Convenience: create an Action event (tool call decision).
    pub fn action(tool_name: &str, args: &serde_json::Value) -> Self {
        Self::with_attrs(EventKind::Action, "tool.call", [
            ("tool.name".to_string(), serde_json::json!(tool_name)),
            ("tool.args".to_string(), args.clone()),
        ])
    }

    /// Convenience: create an Observation event (tool result).
    pub fn observation(success: bool, content: &str) -> Self {
        Self::with_attrs(EventKind::Observation, "tool.result", [
            ("tool.result.success".to_string(), serde_json::json!(success)),
            ("tool.result.content".to_string(), serde_json::json!(content)),
        ])
    }
}