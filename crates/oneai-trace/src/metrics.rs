//! Trace metrics — computed evaluation metrics from a TraceTree.
//!
//! Metrics cover the key evaluation dimensions:
//! - Task success rate
//! - Token cost and usage
//! - Latency and performance
//! - Tool call distribution and success rate
//! - Approval gate denial rate
//! - Parser fallback rate (3-layer defense effectiveness)
//! - Retry count and iteration statistics

use serde::{Deserialize, Serialize};

use crate::span::{Span, SpanKind, SpanStatus};
use crate::event::EventKind;

// ─── TraceMetrics ────────────────────────────────────────────────────

/// Computed metrics from a TraceTree for agent evaluation.
///
/// These metrics are derived from the span tree by walking all spans
/// and events, aggregating statistics relevant to agent quality.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMetrics {
    /// Task success rate (completed sessions / total sessions).
    pub success_rate: f64,

    /// Total LLM token usage (prompt + completion tokens).
    pub total_tokens: u64,

    /// Average latency per inference call (ms).
    pub avg_inference_latency_ms: f64,

    /// Total tool calls count.
    pub tool_call_count: usize,

    /// Tool success rate (successful tool calls / total).
    pub tool_success_rate: f64,

    /// Approval denial rate (denied / total approval requests).
    pub approval_denial_rate: f64,

    /// Parser fallback rate (Layer 2/3 usage / total parse attempts).
    pub parser_fallback_rate: f64,

    /// Total retry count across all steps.
    pub total_retries: usize,

    /// Workflow step completion rate.
    pub workflow_step_success_rate: f64,

    /// Average ReAct iterations per session.
    pub avg_iterations: f64,

    /// Total session duration in milliseconds.
    pub total_session_duration_ms: u64,

    /// Number of error events.
    pub error_count: usize,

    /// Number of checkpoint saves.
    pub checkpoint_count: usize,
}

impl Default for TraceMetrics {
    fn default() -> Self {
        Self {
            success_rate: 0.0,
            total_tokens: 0,
            avg_inference_latency_ms: 0.0,
            tool_call_count: 0,
            tool_success_rate: 0.0,
            approval_denial_rate: 0.0,
            parser_fallback_rate: 0.0,
            total_retries: 0,
            workflow_step_success_rate: 0.0,
            avg_iterations: 0.0,
            total_session_duration_ms: 0,
            error_count: 0,
            checkpoint_count: 0,
        }
    }
}

impl TraceMetrics {
    /// Compute metrics from a root span tree by walking all spans and events.
    pub fn compute_from_tree(root: &Span) -> Self {
        let mut metrics = Self::default();

        // Session success: root span status
        metrics.success_rate = if root.status == SpanStatus::Ok { 1.0 } else { 0.0 };
        metrics.total_session_duration_ms = root.duration_ms.unwrap_or(0);

        // Walk the tree and aggregate
        Self::walk_span(root, &mut metrics);

        // Average inference latency
        let inference_spans = root.spans_by_kind(SpanKind::LLM);
        if !inference_spans.is_empty() {
            let total_latency: u64 = inference_spans.iter()
                .filter_map(|s| s.duration_ms)
                .sum();
            metrics.avg_inference_latency_ms = total_latency as f64 / inference_spans.len() as f64;
        }

        // Workflow step success rate
        let workflow_spans = root.spans_by_kind(SpanKind::WORKFLOW);
        if !workflow_spans.is_empty() {
            let completed = workflow_spans.iter()
                .filter(|s| s.status == SpanStatus::Ok)
                .count();
            metrics.workflow_step_success_rate = completed as f64 / workflow_spans.len() as f64;
        }

        // Tool success rate
        if metrics.tool_call_count > 0 {
            let tool_spans = root.spans_by_kind(SpanKind::TOOL);
            let successful = tool_spans.iter()
                .filter(|s| s.status == SpanStatus::Ok)
                .count();
            metrics.tool_success_rate = successful as f64 / metrics.tool_call_count as f64;
        }

        metrics
    }

    /// Walk a span tree recursively, aggregating metrics.
    fn walk_span(span: &Span, metrics: &mut TraceMetrics) {
        // Count tool calls (TOOL kind spans)
        if span.kind == SpanKind::TOOL {
            metrics.tool_call_count += 1;
        }

        // Count checkpoint saves from events
        for event in &span.events {
            match event.kind {
                EventKind::CheckpointSave => metrics.checkpoint_count += 1,
                EventKind::Error => metrics.error_count += 1,
                EventKind::Action => metrics.tool_call_count += 1,
                _ => {}
            }

            // Extract token count from inference events
            if event.kind == EventKind::InferenceEnd {
                if let Some(tokens) = event.attributes.get("llm.total_tokens") {
                    metrics.total_tokens += tokens.as_u64().unwrap_or(0);
                }
            }

            // Count approval denials
            if event.kind == EventKind::ApprovalResponse {
                if let Some(response) = event.attributes.get("approval.response") {
                    if response.as_str() == Some("Denied") {
                        metrics.approval_denial_rate += 1.0;
                    }
                }
            }

            // Count parser fallbacks
            if event.kind == EventKind::ParseFallback {
                metrics.parser_fallback_rate += 1.0;
            }
        }

        // Count retries from span attributes
        if let Some(retries) = span.attributes.get("retries_used") {
            metrics.total_retries += retries.as_u64().unwrap_or(0) as usize;
        }

        // Count iterations from AGENT spans
        if span.kind == SpanKind::AGENT {
            if let Some(iter) = span.attributes.get("agent.iteration") {
                metrics.avg_iterations += iter.as_u64().unwrap_or(1) as f64;
            }
        }

        // Recurse into children
        for child in &span.children {
            Self::walk_span(child, metrics);
        }
    }

    /// Merge multiple metrics (from multiple sessions) into aggregate metrics.
    pub fn merge(metrics_list: &[TraceMetrics]) -> Self {
        if metrics_list.is_empty() {
            return Self::default();
        }

        let total_sessions = metrics_list.len() as f64;
        let successful_sessions = metrics_list.iter()
            .filter(|m| m.success_rate > 0.0)
            .count() as f64;

        Self {
            success_rate: successful_sessions / total_sessions,
            total_tokens: metrics_list.iter().map(|m| m.total_tokens).sum(),
            avg_inference_latency_ms: metrics_list.iter().map(|m| m.avg_inference_latency_ms).sum::<f64>() / total_sessions,
            tool_call_count: metrics_list.iter().map(|m| m.tool_call_count).sum(),
            tool_success_rate: metrics_list.iter().map(|m| m.tool_success_rate * m.tool_call_count as f64).sum::<f64>()
                / metrics_list.iter().map(|m| m.tool_call_count as f64).sum::<f64>().max(1.0),
            approval_denial_rate: metrics_list.iter().map(|m| m.approval_denial_rate).sum::<f64>()
                / metrics_list.iter().map(|m| m.tool_call_count as f64).sum::<f64>().max(1.0),
            parser_fallback_rate: metrics_list.iter().map(|m| m.parser_fallback_rate).sum::<f64>()
                / metrics_list.iter().map(|m| m.tool_call_count as f64 + 1.0).sum::<f64>(),
            total_retries: metrics_list.iter().map(|m| m.total_retries).sum(),
            workflow_step_success_rate: metrics_list.iter().map(|m| m.workflow_step_success_rate).sum::<f64>() / total_sessions,
            avg_iterations: metrics_list.iter().map(|m| m.avg_iterations).sum::<f64>() / total_sessions,
            total_session_duration_ms: metrics_list.iter().map(|m| m.total_session_duration_ms).sum(),
            error_count: metrics_list.iter().map(|m| m.error_count).sum(),
            checkpoint_count: metrics_list.iter().map(|m| m.checkpoint_count).sum(),
        }
    }
}