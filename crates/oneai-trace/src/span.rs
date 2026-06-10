//! OpenInference-compatible span kinds — SCREAMING_SNAKE_CASE convention.
//!
//! Each span kind represents a category of work being tracked:
//! - SESSION: root span for an entire session
//! - AGENT: agent paradigm execution (ReAct, Plan, etc.)
//! - TOOL: tool call and execution
//! - LLM: LLM inference request
//! - RETRIEVER: memory/RAG retrieval
//! - WORKFLOW: workflow DAG execution
//! - APPROVAL: approval gate interaction
//! - PARSER: output parsing (3-layer defense)
//! - INTERNAL: internal framework operations

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::event::TraceEvent;

// ─── SpanKind ────────────────────────────────────────────────────────

/// OpenInference span kinds — categorize the type of work a span represents.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum SpanKind {
    /// Root span: entire session lifecycle.
    SESSION,
    /// Agent paradigm execution (ReAct, Plan, Reflection, etc.).
    AGENT,
    /// Tool call and execution.
    TOOL,
    /// LLM inference request.
    LLM,
    /// Memory/RAG retrieval operation.
    RETRIEVER,
    /// Workflow DAG execution.
    WORKFLOW,
    /// Approval gate interaction (human review).
    APPROVAL,
    /// Output parsing (3-layer defense).
    PARSER,
    /// Internal framework operation (not directly user-visible).
    INTERNAL,
}

// ─── SpanStatus ──────────────────────────────────────────────────────

/// Span execution status — indicates whether the operation succeeded, failed, or was cancelled.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum SpanStatus {
    /// Operation completed successfully.
    Ok,
    /// Operation failed with an error.
    Error,
    /// Operation was cancelled (e.g., approval denied, timeout).
    Cancelled,
}

// ─── Span ─────────────────────────────────────────────────────────────

/// A span represents a unit of work with a start time, end time, and attributes.
///
/// Spans form a tree structure:
/// - Root span (SESSION) → child spans (AGENT, TOOL, LLM, etc.)
/// - Each span can have events and nested children
///
/// Compatible with OpenInference semantic conventions:
/// - `kind` uses SCREAMING_SNAKE_CASE
/// - `attributes` use dot-namespace keys (e.g., "tool.name", "llm.token_count")
/// - `events` capture discrete occurrences within the span
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    /// Unique span ID (UUID v4).
    pub span_id: String,

    /// Parent span ID (None for root spans).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,

    /// The kind of work this span represents.
    pub kind: SpanKind,

    /// Span name (e.g., "session", "react_iteration", "tool.calculator").
    pub name: String,

    /// Start time (ISO 8601 UTC).
    pub start_time: DateTime<Utc>,

    /// End time (ISO 8601 UTC, None if still running).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<DateTime<Utc>>,

    /// Duration in milliseconds (computed from start_time and end_time).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,

    /// Span execution status.
    #[serde(default = "default_span_status")]
    pub status: SpanStatus,

    /// OpenInference-compatible attributes (dot-namespace key-value pairs).
    #[serde(default)]
    pub attributes: HashMap<String, serde_json::Value>,

    /// Events logged within this span (timestamped occurrences).
    #[serde(default)]
    pub events: Vec<TraceEvent>,

    /// Child spans (nested sub-operations).
    #[serde(default)]
    pub children: Vec<Span>,
}

fn default_span_status() -> SpanStatus {
    SpanStatus::Ok
}

impl Span {
    /// Create a new span with the given kind and name.
    pub fn new(kind: SpanKind, name: &str, parent_id: Option<&str>) -> Self {
        Self {
            span_id: Uuid::new_v4().to_string(),
            parent_span_id: parent_id.map(|s| s.to_string()),
            kind,
            name: name.to_string(),
            start_time: Utc::now(),
            end_time: None,
            duration_ms: None,
            status: SpanStatus::Ok,
            attributes: HashMap::new(),
            events: Vec::new(),
            children: Vec::new(),
        }
    }

    /// End this span — set end_time, compute duration, and set status.
    pub fn end(&mut self, status: SpanStatus) {
        self.end_time = Some(Utc::now());
        self.duration_ms = Some(
            (self.end_time.unwrap() - self.start_time)
                .num_milliseconds()
                .max(0) as u64,
        );
        self.status = status;
    }

    /// Set an attribute on this span.
    pub fn set_attribute(&mut self, key: &str, value: serde_json::Value) {
        self.attributes.insert(key.to_string(), value);
    }

    /// Add an event to this span.
    pub fn add_event(&mut self, event: TraceEvent) {
        self.events.push(event);
    }

    /// Add a child span.
    pub fn add_child(&mut self, child: Span) {
        self.children.push(child);
    }

    /// Check if this span is still running (no end_time).
    pub fn is_running(&self) -> bool {
        self.end_time.is_none()
    }

    /// Find a span by ID in this span's tree (recursive).
    pub fn find_span(&self, span_id: &str) -> Option<&Span> {
        if self.span_id == span_id {
            return Some(self);
        }
        for child in &self.children {
            if let Some(found) = child.find_span(span_id) {
                return Some(found);
            }
        }
        None
    }

    /// Find a mutable span by ID in this span's tree (recursive).
    pub fn find_span_mut(&mut self, span_id: &str) -> Option<&mut Span> {
        if self.span_id == span_id {
            return Some(self);
        }
        for child in &mut self.children {
            if let Some(found) = child.find_span_mut(span_id) {
                return Some(found);
            }
        }
        None
    }

    /// Count all spans (including children) in this tree.
    pub fn count_spans(&self) -> usize {
        1 + self.children.iter().map(|c| c.count_spans()).sum::<usize>()
    }

    /// Count all events (including in children) in this tree.
    pub fn count_events(&self) -> usize {
        self.events.len() + self.children.iter().map(|c| c.count_events()).sum::<usize>()
    }

    /// Collect all spans of a given kind (including children).
    pub fn spans_by_kind(&self, kind: SpanKind) -> Vec<&Span> {
        let mut result = Vec::new();
        if self.kind == kind {
            result.push(self);
        }
        for child in &self.children {
            result.extend(child.spans_by_kind(kind));
        }
        result
    }
}