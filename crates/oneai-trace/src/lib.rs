//! # OneAI Trace — OpenInference-compatible trajectory logger
//!
//! Global trace mechanism for the OneAI Agent framework, inspired by
//! LangSmith and OpenInference standards. Captures every step of agent
//! execution (Thought, Action, Observation) as a structured JSON tree
//! for evaluation: task success rate, trajectory quality, fault tolerance,
//! cost, and performance.
//!
//! ## Architecture
//!
//! - **Span**: Unit of work with start/end times, attributes, events, and children
//! - **TraceEvent**: Timestamped occurrence within a span
//! - **TraceContext**: Thread-safe span stack + collector routing
//! - **TraceEmitter**: Global singleton for initialization and context creation
//! - **TraceCollector**: Output strategy (in-memory, file, remote)
//! - **TraceTree**: Assembled span hierarchy + metadata + metrics
//! - **TraceMetrics**: Computed evaluation statistics
//!
//! ## Conditional Compilation
//!
//! When the `trace` feature is disabled (`default-features = false`),
//! all trace types become zero-cost stubs that compile away completely.

// When trace feature is enabled, use the full implementation
#[cfg(feature = "trace")]
mod span;
#[cfg(feature = "trace")]
mod event;
#[cfg(feature = "trace")]
mod context;
#[cfg(feature = "trace")]
mod tree;
#[cfg(feature = "trace")]
mod emitter;
#[cfg(feature = "trace")]
mod collector;
#[cfg(feature = "trace")]
mod metrics;

// When trace feature is disabled, use zero-cost stubs
#[cfg(not(feature = "trace"))]
mod noop;

// ─── Public exports ──────────────────────────────────────────────────

#[cfg(feature = "trace")]
pub use span::{Span, SpanKind, SpanStatus};
#[cfg(feature = "trace")]
pub use event::{TraceEvent, EventKind};
#[cfg(feature = "trace")]
pub use context::TraceContext;
#[cfg(feature = "trace")]
pub use tree::{TraceTree, TraceMetadata};
#[cfg(feature = "trace")]
pub use emitter::TraceEmitter;
#[cfg(feature = "trace")]
pub use collector::{TraceCollector, InMemoryCollector, FileCollector, NoopCollector};
#[cfg(feature = "trace")]
pub use metrics::TraceMetrics;

#[cfg(not(feature = "trace"))]
pub use noop::{
    TraceContext, SpanKind, SpanStatus, EventKind,
    Span, TraceEvent, TraceTree, TraceMetadata,
    TraceEmitter, TraceCollector, TraceMetrics,
    InMemoryCollector, FileCollector, NoopCollector,
};

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(all(test, feature = "trace"))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::collections::HashMap;

    #[test]
    fn test_span_creation() {
        let span = Span::new(SpanKind::SESSION, "session", None);
        assert_eq!(span.kind, SpanKind::SESSION);
        assert_eq!(span.name, "session");
        assert!(span.parent_span_id.is_none());
        assert!(span.is_running());
        assert!(span.end_time.is_none());
    }

    #[test]
    fn test_span_with_parent() {
        let parent = Span::new(SpanKind::SESSION, "session", None);
        let child = Span::new(SpanKind::TOOL, "tool.calc", Some(&parent.span_id));
        assert_eq!(child.parent_span_id, Some(parent.span_id.clone()));
    }

    #[test]
    fn test_span_end() {
        let mut span = Span::new(SpanKind::TOOL, "tool.calc", None);
        span.end(SpanStatus::Ok);
        assert!(!span.is_running());
        assert!(span.end_time.is_some());
        assert!(span.duration_ms.is_some());
        assert_eq!(span.status, SpanStatus::Ok);
    }

    #[test]
    fn test_span_attributes() {
        let mut span = Span::new(SpanKind::TOOL, "tool.calc", None);
        span.set_attribute("tool.name", serde_json::json!("calculator"));
        span.set_attribute("tool.risk_level", serde_json::json!("low"));
        assert_eq!(span.attributes.get("tool.name").unwrap(), "calculator");
    }

    #[test]
    fn test_span_events() {
        let mut span = Span::new(SpanKind::AGENT, "react_loop", None);
        span.add_event(TraceEvent::thought("I need to calculate 2+2"));
        span.add_event(TraceEvent::action("calculator", &serde_json::json!({"expression": "2+2"})));
        span.add_event(TraceEvent::observation(true, "4"));
        assert_eq!(span.events.len(), 3);
        assert_eq!(span.events[0].kind, EventKind::Thought);
        assert_eq!(span.events[1].kind, EventKind::Action);
        assert_eq!(span.events[2].kind, EventKind::Observation);
    }

    #[test]
    fn test_span_tree_operations() {
        let mut root = Span::new(SpanKind::SESSION, "session", None);
        let child1 = Span::new(SpanKind::AGENT, "react_loop", Some(&root.span_id));
        let child1_id = child1.span_id.clone();
        root.add_child(child1);

        // Add child2 into child1
        let child2 = Span::new(SpanKind::TOOL, "tool.calc", Some(&child1_id));
        let child2_id = child2.span_id.clone();
        root.find_span_mut(&child1_id).unwrap().add_child(child2);

        assert_eq!(root.count_spans(), 3);
        assert!(root.find_span(&child2_id).is_some());
    }

    #[test]
    fn test_span_spans_by_kind() {
        let mut root = Span::new(SpanKind::SESSION, "session", None);
        let tool1 = Span::new(SpanKind::TOOL, "tool.calc", Some(&root.span_id));
        let tool2 = Span::new(SpanKind::TOOL, "tool.shell", Some(&root.span_id));
        let agent = Span::new(SpanKind::AGENT, "react", Some(&root.span_id));

        root.add_child(tool1);
        root.add_child(tool2);
        root.add_child(agent);

        let tool_spans = root.spans_by_kind(SpanKind::TOOL);
        assert_eq!(tool_spans.len(), 2);
        let agent_spans = root.spans_by_kind(SpanKind::AGENT);
        assert_eq!(agent_spans.len(), 1);
    }

    #[test]
    fn test_trace_context_basic() {
        let ctx = TraceContext::new(Arc::new(InMemoryCollector::new()));
        assert!(ctx.is_enabled());

        let session_span = ctx.enter_span(SpanKind::SESSION, "session", None);
        ctx.set_attribute("session.id", serde_json::json!("test_123"));

        ctx.log_event(EventKind::Thought, "agent.thought", HashMap::from([
            ("input.message".to_string(), serde_json::json!("Hello")),
        ]));

        ctx.exit_span(&session_span, SpanStatus::Ok);

        let tree = ctx.build_tree();
        assert_eq!(tree.root_span.kind, SpanKind::SESSION);
        assert_eq!(tree.root_span.status, SpanStatus::Ok);
        assert_eq!(tree.root_span.events.len(), 1);
        assert!(tree.root_span.end_time.is_some());
    }

    #[test]
    fn test_trace_context_nested_spans() {
        let ctx = TraceContext::new(Arc::new(InMemoryCollector::new()));

        let session = ctx.enter_span(SpanKind::SESSION, "session", None);
        let agent = ctx.enter_span(SpanKind::AGENT, "react_loop", None);
        let tool = ctx.enter_span(SpanKind::TOOL, "tool.calc", None);

        ctx.log_event(EventKind::Action, "tool.call", HashMap::new());
        ctx.exit_span(&tool, SpanStatus::Ok);
        ctx.exit_span(&agent, SpanStatus::Ok);
        ctx.exit_span(&session, SpanStatus::Ok);

        let tree = ctx.build_tree();
        assert_eq!(tree.root_span.count_spans(), 3);
    }

    #[test]
    fn test_trace_context_disabled() {
        let ctx = TraceContext::disabled();
        assert!(!ctx.is_enabled());

        let span_id = ctx.enter_span(SpanKind::SESSION, "session", None);
        assert!(span_id.is_empty()); // Disabled → no span created

        ctx.log_event(EventKind::Thought, "agent.thought", HashMap::new());
        // Events silently dropped
    }

    #[test]
    fn test_trace_tree_json_export() {
        let ctx = TraceContext::new(Arc::new(InMemoryCollector::new()));

        let session = ctx.enter_span(SpanKind::SESSION, "session", None);
        ctx.set_attribute("session.id", serde_json::json!("test_456"));
        ctx.log_event(EventKind::Thought, "agent.thought", HashMap::from([
            ("input.message".to_string(), serde_json::json!("What is Rust?")),
        ]));
        ctx.exit_span(&session, SpanStatus::Ok);

        let tree = ctx.build_tree();
        let json = tree.to_json().unwrap();

        // Verify JSON structure
        assert!(json.contains("\"SESSION\""));
        assert!(json.contains("\"Thought\""));
        assert!(json.contains("\"session.id\""));
        assert!(json.contains("\"trace_id\""));
        assert!(json.contains("\"metadata\""));
        assert!(json.contains("\"metrics\""));
    }

    #[test]
    fn test_trace_metrics_computation() {
        let mut root = Span::new(SpanKind::SESSION, "session", None);
        root.end(SpanStatus::Ok);

        let mut tool1 = Span::new(SpanKind::TOOL, "tool.calc", Some(&root.span_id));
        tool1.end(SpanStatus::Ok);
        root.add_child(tool1);

        let mut tool2 = Span::new(SpanKind::TOOL, "tool.shell", Some(&root.span_id));
        tool2.end(SpanStatus::Error);
        root.add_child(tool2);

        let metrics = TraceMetrics::compute_from_tree(&root);
        assert_eq!(metrics.success_rate, 1.0); // Root span is Ok
        assert_eq!(metrics.tool_call_count, 2);
        assert_eq!(metrics.tool_success_rate, 0.5); // 1 success, 1 error
    }

    #[test]
    fn test_event_convenience_methods() {
        let thought = TraceEvent::thought("I need to use the calculator");
        assert_eq!(thought.kind, EventKind::Thought);
        assert!(thought.attributes.contains_key("input.message"));

        let action = TraceEvent::action("calculator", &serde_json::json!({"expr": "2+2"}));
        assert_eq!(action.kind, EventKind::Action);
        assert!(action.attributes.contains_key("tool.name"));

        let obs = TraceEvent::observation(true, "4");
        assert_eq!(obs.kind, EventKind::Observation);
        assert!(obs.attributes.contains_key("tool.result.success"));
    }

    #[test]
    fn test_in_memory_collector() {
        let collector = InMemoryCollector::new();
        assert_eq!(collector.span_count(), 0);
    }
}