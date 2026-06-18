//! Trace DTO — converts TraceTree/Span/Event data into JSON for the Studio frontend.

use serde::{Deserialize, Serialize};
use oneai_trace::{TraceTree, TraceMetrics, Span, TraceEvent};

// ─── TraceTreeView ───────────────────────────────────────────────────

/// Complete trace tree visualization data for the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceTreeView {
    /// Trace ID.
    pub trace_id: String,

    /// Session metadata.
    pub metadata: TraceMetadataView,

    /// Root span with nested children.
    pub root_span: SpanView,

    /// Computed metrics.
    pub metrics: MetricsView,
}

// ─── TraceMetadataView ───────────────────────────────────────────────

/// Frontend-friendly trace metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMetadataView {
    pub framework: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub created_at: String,
}

// ─── SpanView ────────────────────────────────────────────────────────

/// Frontend-friendly span representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanView {
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub kind: String,
    pub name: String,
    pub start_time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub status: String,
    #[serde(default)]
    pub attributes: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub events: Vec<EventView>,
    #[serde(default)]
    pub children: Vec<SpanView>,
}

// ─── EventView ───────────────────────────────────────────────────────

/// Frontend-friendly event representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventView {
    pub timestamp: String,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

// ─── MetricsView ─────────────────────────────────────────────────────

/// Frontend-friendly trace metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsView {
    pub success_rate: f64,
    pub total_tokens: u64,
    pub estimated_cost_usd: f64,
    pub avg_inference_latency_ms: f64,
    pub tool_call_count: usize,
    pub tool_success_rate: f64,
    pub approval_denial_rate: f64,
    pub parser_fallback_rate: f64,
    pub total_retries: usize,
    pub workflow_step_success_rate: f64,
    pub avg_iterations: f64,
    pub total_session_duration_ms: u64,
    pub error_count: usize,
    pub checkpoint_count: usize,
}

// ─── Conversion ──────────────────────────────────────────────────────

impl TraceTreeView {
    /// Convert a TraceTree into a frontend-friendly TraceTreeView.
    pub fn from_trace_tree(tree: &TraceTree) -> Self {
        Self {
            trace_id: tree.trace_id.clone(),
            metadata: TraceMetadataView {
                framework: tree.metadata.framework.clone(),
                version: tree.metadata.version.clone(),
                session_id: tree.metadata.session_id.clone(),
                platform: tree.metadata.platform.clone(),
                model: tree.metadata.model.clone(),
                created_at: tree.metadata.created_at.clone(),
            },
            root_span: span_to_view(&tree.root_span),
            metrics: MetricsView {
                success_rate: tree.metrics.success_rate,
                total_tokens: tree.metrics.total_tokens,
                estimated_cost_usd: tree.metrics.estimated_cost_usd,
                avg_inference_latency_ms: tree.metrics.avg_inference_latency_ms,
                tool_call_count: tree.metrics.tool_call_count,
                tool_success_rate: tree.metrics.tool_success_rate,
                approval_denial_rate: tree.metrics.approval_denial_rate,
                parser_fallback_rate: tree.metrics.parser_fallback_rate,
                total_retries: tree.metrics.total_retries,
                workflow_step_success_rate: tree.metrics.workflow_step_success_rate,
                avg_iterations: tree.metrics.avg_iterations,
                total_session_duration_ms: tree.metrics.total_session_duration_ms,
                error_count: tree.metrics.error_count,
                checkpoint_count: tree.metrics.checkpoint_count,
            },
        }
    }
}

fn span_to_view(span: &Span) -> SpanView {
    SpanView {
        span_id: span.span_id.clone(),
        parent_span_id: span.parent_span_id.clone(),
        kind: span.kind.as_ref().to_string(),
        name: span.name.clone(),
        start_time: span.start_time.to_rfc3339(),
        end_time: span.end_time.map(|t| t.to_rfc3339()),
        duration_ms: span.duration_ms,
        status: span.status.as_ref().to_string(),
        attributes: serde_json::to_value(&span.attributes)
            .unwrap_or_default()
            .as_object()
            .cloned()
            .unwrap_or_default(),
        events: span.events.iter().map(event_to_view).collect(),
        children: span.children.iter().map(span_to_view).collect(),
    }
}

fn event_to_view(event: &TraceEvent) -> EventView {
    EventView {
        timestamp: event.timestamp.to_rfc3339(),
        kind: event.kind.as_ref().to_string(),
        name: event.name.clone(),
        attributes: serde_json::to_value(&event.attributes)
            .unwrap_or_default()
            .as_object()
            .cloned()
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::collections::HashMap;
    use oneai_trace::{TraceContext, InMemoryCollector, SpanKind, SpanStatus, EventKind, Span, TraceMetrics};

    #[test]
    fn test_trace_tree_view_conversion() {
        let ctx = TraceContext::new(Arc::new(InMemoryCollector::new()));

        let session = ctx.enter_span(SpanKind::SESSION, "session", None);
        ctx.set_attribute("session.id", serde_json::json!("test_123"));
        ctx.log_event(EventKind::Thought, "agent.thought", HashMap::from([
            ("input.message".to_string(), serde_json::json!("Hello")),
        ]));
        ctx.exit_span(&session, SpanStatus::Ok);

        let tree = ctx.build_tree();
        let view = TraceTreeView::from_trace_tree(&tree);

        assert_eq!(view.trace_id, tree.trace_id);
        assert_eq!(view.root_span.kind, "SESSION");
        assert_eq!(view.root_span.status, "Ok");
        assert_eq!(view.root_span.events.len(), 1);
        assert_eq!(view.root_span.events[0].kind, "Thought");
    }

    #[test]
    fn test_trace_tree_view_json() {
        let ctx = TraceContext::new(Arc::new(InMemoryCollector::new()));

        let session = ctx.enter_span(SpanKind::SESSION, "session", None);
        ctx.exit_span(&session, SpanStatus::Ok);

        let tree = ctx.build_tree();
        let view = TraceTreeView::from_trace_tree(&tree);

        let json = serde_json::to_string_pretty(&view).unwrap();
        assert!(json.contains("\"SESSION\""));
        assert!(json.contains("\"Ok\""));
        assert!(json.contains("\"trace_id\""));
    }

    #[test]
    fn test_span_view_nested() {
        let ctx = TraceContext::new(Arc::new(InMemoryCollector::new()));

        let session = ctx.enter_span(SpanKind::SESSION, "session", None);
        let agent = ctx.enter_span(SpanKind::AGENT, "react_loop", None);
        ctx.exit_span(&agent, SpanStatus::Ok);
        ctx.exit_span(&session, SpanStatus::Ok);

        let tree = ctx.build_tree();
        let view = TraceTreeView::from_trace_tree(&tree);

        // Root span should have one child
        assert_eq!(view.root_span.children.len(), 1);
        assert_eq!(view.root_span.children[0].kind, "AGENT");
    }

    #[test]
    fn test_metrics_view() {
        let mut root = Span::new(SpanKind::SESSION, "session", None);
        root.end(SpanStatus::Ok);

        let mut tool = Span::new(SpanKind::TOOL, "tool.calc", Some(&root.span_id));
        tool.end(SpanStatus::Ok);
        root.add_child(tool);

        let metrics = TraceMetrics::compute_from_tree(&root);
        let view = MetricsView {
            success_rate: metrics.success_rate,
            total_tokens: metrics.total_tokens,
            estimated_cost_usd: metrics.estimated_cost_usd,
            avg_inference_latency_ms: metrics.avg_inference_latency_ms,
            tool_call_count: metrics.tool_call_count,
            tool_success_rate: metrics.tool_success_rate,
            approval_denial_rate: metrics.approval_denial_rate,
            parser_fallback_rate: metrics.parser_fallback_rate,
            total_retries: metrics.total_retries,
            workflow_step_success_rate: metrics.workflow_step_success_rate,
            avg_iterations: metrics.avg_iterations,
            total_session_duration_ms: metrics.total_session_duration_ms,
            error_count: metrics.error_count,
            checkpoint_count: metrics.checkpoint_count,
        };

        assert_eq!(view.success_rate, 1.0);
        assert_eq!(view.tool_call_count, 1);
    }
}
