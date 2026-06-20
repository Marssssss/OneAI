//! Trace context — thread-safe span stack and collector routing.
//!
//! TraceContext is the primary interface for emitting trace events
//! during execution. It holds:
//! - A span stack (current active span, parent hierarchy)
//! - A collector (where events are routed — in-memory, file, remote)
//! - A runtime enabled flag (can disable tracing without recompilation)
//!
//! Thread-safe via Arc<Mutex> for the span stack and HashMap.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::span::{Span, SpanKind, SpanStatus};
use crate::event::{TraceEvent, EventKind};
use crate::collector::TraceCollector;
use crate::tree::TraceTree;

// ─── SharedTraceContext ──────────────────────────────────────────────

/// Shared inner state for TraceContext — wrapped in Arc for thread safety.
struct SharedTraceContext {
    /// Current span ID stack (top = current active span).
    span_stack: Mutex<Vec<String>>,

    /// All spans collected so far, keyed by span_id.
    spans: Mutex<HashMap<String, Span>>,

    /// The collector to route events to.
    #[allow(dead_code)]
    collector: Arc<dyn TraceCollector>,

    /// Whether tracing is enabled (runtime toggle).
    enabled: AtomicBool,

    /// Session ID (set at session creation).
    session_id: Mutex<Option<String>>,
}

// ─── TraceContext ────────────────────────────────────────────────────

/// Thread-safe trace context carried through execution.
///
/// Cloning produces a new Arc reference to the same shared state,
/// so all clones share the same span stack and collector.
///
/// Usage:
/// ```ignore
/// let ctx = TraceContext::new(Arc::new(InMemoryCollector::new()));
/// let session_span = ctx.enter_span(SpanKind::SESSION, "session", None);
/// ctx.set_attribute("session.id", json!("sess_123"));
/// ctx.log_event(EventKind::Thought, "agent.thought", attrs);
/// ctx.exit_span(session_span, SpanStatus::Ok);
/// let tree = ctx.build_tree();
/// ```
#[derive(Debug, Clone)]
pub struct TraceContext {
    inner: Arc<SharedTraceContext>,
}

impl std::fmt::Debug for SharedTraceContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedTraceContext")
            .field("enabled", &self.enabled.load(std::sync::atomic::Ordering::Relaxed))
            .field("session_id", &self.session_id.lock().unwrap())
            .field("span_stack_len", &self.span_stack.lock().unwrap().len())
            .field("spans_count", &self.spans.lock().unwrap().len())
            .finish_non_exhaustive()
    }
}

impl TraceContext {
    /// Create a new trace context with the given collector.
    pub fn new(collector: Arc<dyn TraceCollector>) -> Self {
        Self {
            inner: Arc::new(SharedTraceContext {
                span_stack: Mutex::new(Vec::new()),
                spans: Mutex::new(HashMap::new()),
                collector,
                enabled: AtomicBool::new(true),
                session_id: Mutex::new(None),
            }),
        }
    }

    /// Create a disabled trace context (events are silently dropped).
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(SharedTraceContext {
                span_stack: Mutex::new(Vec::new()),
                spans: Mutex::new(HashMap::new()),
                collector: Arc::new(crate::collector::NoopCollector),
                enabled: AtomicBool::new(false),
                session_id: Mutex::new(None),
            }),
        }
    }

    /// Check if tracing is enabled.
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set runtime enabled/disabled.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the session ID for this trace context.
    pub fn set_session_id(&self, session_id: &str) {
        let mut sid = self.inner.session_id.lock().unwrap();
        *sid = Some(session_id.to_string());
    }

    /// Get the session ID.
    pub fn session_id(&self) -> Option<String> {
        self.inner.session_id.lock().unwrap().clone()
    }

    // ─── Span Management ────────────────────────────────────────────

    /// Enter a new span — push onto the stack and store it.
    ///
    /// If `parent_id` is None, the parent is the current top-of-stack span
    /// (or this becomes a root span if the stack is empty).
    /// Returns the new span's ID for later exit_span() calls.
    pub fn enter_span(&self, kind: SpanKind, name: &str, parent_id: Option<&str>) -> String {
        if !self.is_enabled() {
            return String::new();
        }

        // Determine parent: use explicit parent_id, or current stack top
        let resolved_parent = parent_id.map(|s| s.to_string()).or_else(|| {
            self.inner.span_stack.lock().unwrap().last().cloned()
        });

        let span = Span::new(kind, name, resolved_parent.as_deref());
        let span_id = span.span_id.clone();

        // Store the span
        self.inner.spans.lock().unwrap().insert(span_id.clone(), span);

        // Push onto the stack
        self.inner.span_stack.lock().unwrap().push(span_id.clone());

        tracing::debug!("Trace: enter span {} kind={} name={}", span_id, kind.as_ref(), name);

        span_id
    }

    /// Exit a span — pop from the stack, set end_time, compute duration.
    pub fn exit_span(&self, span_id: &str, status: SpanStatus) {
        if !self.is_enabled() || span_id.is_empty() {
            return;
        }

        // Pop from the stack
        {
            let mut stack = self.inner.span_stack.lock().unwrap();
            // Find and remove the span from the stack (may not be top if nested)
            if let Some(pos) = stack.iter().rposition(|s| s == span_id) {
                stack.remove(pos);
            }
        }

        // End the span
        {
            let mut spans = self.inner.spans.lock().unwrap();
            if let Some(span) = spans.get_mut(span_id) {
                span.end(status);
                tracing::debug!("Trace: exit span {} status={}", span_id, status.as_ref());
            }
        }
    }

    /// Get the current span ID (top of stack).
    pub fn current_span_id(&self) -> Option<String> {
        self.inner.span_stack.lock().unwrap().last().cloned()
    }

    // ─── Event Logging ──────────────────────────────────────────────

    /// Log an event within the current span (top of stack).
    pub fn log_event(&self, kind: EventKind, name: &str, attrs: HashMap<String, serde_json::Value>) {
        if !self.is_enabled() {
            return;
        }

        let event = TraceEvent::with_attrs(kind, name, attrs);

        // Add to the current span
        let current_id = self.current_span_id();
        if let Some(span_id) = current_id {
            let mut spans = self.inner.spans.lock().unwrap();
            if let Some(span) = spans.get_mut(&span_id) {
                span.add_event(event);
                tracing::debug!("Trace: log event {} kind={} in span {}", name, kind.as_ref(), span_id);
            }
        } else {
            tracing::warn!("Trace: log event {} with no active span", name);
        }
    }

    /// Log an event within a specific span (by ID).
    pub fn log_event_in_span(&self, span_id: &str, kind: EventKind, name: &str, attrs: HashMap<String, serde_json::Value>) {
        if !self.is_enabled() || span_id.is_empty() {
            return;
        }

        let event = TraceEvent::with_attrs(kind, name, attrs);

        let mut spans = self.inner.spans.lock().unwrap();
        if let Some(span) = spans.get_mut(span_id) {
            span.add_event(event);
        }
    }

    // ─── Attributes ─────────────────────────────────────────────────

    /// Set an attribute on the current span (top of stack).
    pub fn set_attribute(&self, key: &str, value: serde_json::Value) {
        if !self.is_enabled() {
            return;
        }

        let current_id = self.current_span_id();
        if let Some(span_id) = current_id {
            let mut spans = self.inner.spans.lock().unwrap();
            if let Some(span) = spans.get_mut(&span_id) {
                span.set_attribute(key, value);
            }
        }
    }

    /// Set an attribute on a specific span (by ID).
    pub fn set_attribute_on_span(&self, span_id: &str, key: &str, value: serde_json::Value) {
        if !self.is_enabled() || span_id.is_empty() {
            return;
        }

        let mut spans = self.inner.spans.lock().unwrap();
        if let Some(span) = spans.get_mut(span_id) {
            span.set_attribute(key, value);
        }
    }

    // ─── Tree Building ──────────────────────────────────────────────

    /// Build the final TraceTree from all collected spans.
    ///
    /// Assembles the span tree by:
    /// 1. Finding the root span (no parent_span_id)
    /// 2. Nesting children by parent_span_id references
    /// 3. Computing metrics
    pub fn build_tree(&self) -> TraceTree {
        let spans = self.inner.spans.lock().unwrap().clone();
        TraceTree::from_spans(spans, self.session_id())
    }
}

// ─── SpanKind as_ref() ──────────────────────────────────────────────

impl SpanKind {
    /// Get a string representation of the span kind.
    pub fn as_ref(&self) -> &'static str {
        match self {
            SpanKind::SESSION => "SESSION",
            SpanKind::AGENT => "AGENT",
            SpanKind::TOOL => "TOOL",
            SpanKind::LLM => "LLM",
            SpanKind::RETRIEVER => "RETRIEVER",
            SpanKind::WORKFLOW => "WORKFLOW",
            SpanKind::APPROVAL => "APPROVAL",
            SpanKind::PARSER => "PARSER",
            SpanKind::INTERNAL => "INTERNAL",
        }
    }
}

impl SpanStatus {
    /// Get a string representation of the span status.
    pub fn as_ref(&self) -> &'static str {
        match self {
            SpanStatus::Ok => "Ok",
            SpanStatus::Error => "Error",
            SpanStatus::Cancelled => "Cancelled",
        }
    }
}

impl EventKind {
    /// Get a string representation of the event kind.
    pub fn as_ref(&self) -> &'static str {
        match self {
            EventKind::Thought => "Thought",
            EventKind::Action => "Action",
            EventKind::Observation => "Observation",
            EventKind::InferenceStart => "InferenceStart",
            EventKind::InferenceEnd => "InferenceEnd",
            EventKind::StreamingChunk => "StreamingChunk",
            EventKind::ToolCall => "ToolCall",
            EventKind::ToolResult => "ToolResult",
            EventKind::ToolError => "ToolError",
            EventKind::ApprovalRequest => "ApprovalRequest",
            EventKind::ApprovalResponse => "ApprovalResponse",
            EventKind::ParseAttempt => "ParseAttempt",
            EventKind::ParseSuccess => "ParseSuccess",
            EventKind::ParseFallback => "ParseFallback",
            EventKind::MemoryStore => "MemoryStore",
            EventKind::MemoryRetrieve => "MemoryRetrieve",
            EventKind::WorkflowStepStart => "WorkflowStepStart",
            EventKind::WorkflowStepEnd => "WorkflowStepEnd",
            EventKind::CheckpointSave => "CheckpointSave",
            EventKind::CheckpointLoad => "CheckpointLoad",
            EventKind::Error => "Error",
            EventKind::Custom => "Custom",
        }
    }
}