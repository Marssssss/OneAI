//! Trace tree — assembled span hierarchy with metrics and JSON export.
//!
//! A TraceTree is the final output of a trace session:
//! - Root span (SESSION) with nested children
//! - Metadata (framework version, session ID, platform)
//! - Computed metrics (success rate, token cost, latency, etc.)

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::span::{Span, SpanKind, SpanStatus};
use crate::metrics::TraceMetrics;

// ─── TraceMetadata ──────────────────────────────────────────────────

/// Metadata about a trace session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMetadata {
    /// Framework name.
    pub framework: String,

    /// Framework version.
    pub version: String,

    /// Session ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Platform (macos, windows, linux, android, ios, harmony).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,

    /// LLM model name (if configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Trace creation timestamp (ISO 8601 UTC).
    pub created_at: String,
}

impl Default for TraceMetadata {
    fn default() -> Self {
        Self {
            framework: "oneai".to_string(),
            version: "0.1.0".to_string(),
            session_id: None,
            platform: None,
            model: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

// ─── TraceTree ───────────────────────────────────────────────────────

/// A complete trace tree — root span with nested children.
///
/// Exportable to OpenInference-compatible JSON for agent evaluation.
/// Contains:
/// - Root span (SESSION) with the full nested span hierarchy
/// - Metadata (framework, session, platform, model)
/// - Computed metrics (success rate, cost, latency, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceTree {
    /// Trace ID (derived from session ID or root span ID).
    pub trace_id: String,

    /// Session metadata.
    pub metadata: TraceMetadata,

    /// The root span (SESSION) with nested children.
    pub root_span: Span,

    /// Computed metrics from the trace.
    pub metrics: TraceMetrics,
}

impl TraceTree {
    /// Build a TraceTree from a flat HashMap of spans.
    ///
    /// Assembles the tree by:
    /// 1. Finding the root span (no parent_span_id)
    /// 2. Nesting children by parent_span_id references
    /// 3. Computing metrics from the assembled tree
    pub fn from_spans(spans: HashMap<String, Span>, session_id: Option<String>) -> Self {
        // Find root span (no parent)
        let root_id = spans.values()
            .find(|s| s.parent_span_id.is_none())
            .map(|s| s.span_id.clone());

        if let Some(root_id) = root_id {
            let mut root = spans.get(&root_id).cloned().unwrap_or_else(|| {
                Span::new(SpanKind::SESSION, "session", None)
            });

            // Assemble children into the root
            root = assemble_children(root, &spans);

            let trace_id = session_id.clone().unwrap_or_else(|| root.span_id.clone());

            let mut metadata = TraceMetadata::default();
            metadata.session_id = session_id;

            // Extract platform and model from root attributes
            if let Some(platform) = root.attributes.get("session.platform") {
                metadata.platform = Some(platform.as_str().unwrap_or_default().to_string());
            }
            if let Some(model) = root.attributes.get("session.model") {
                metadata.model = Some(model.as_str().unwrap_or_default().to_string());
            }

            let metrics = TraceMetrics::compute_from_tree(&root);

            Self {
                trace_id,
                metadata,
                root_span: root,
                metrics,
            }
        } else {
            // No spans collected — create an empty tree
            let mut root = Span::new(SpanKind::SESSION, "empty_session", None);
            root.end(SpanStatus::Cancelled);

            Self {
                trace_id: session_id.clone().unwrap_or_else(|| "no_trace".to_string()),
                metadata: TraceMetadata {
                    session_id,
                    ..Default::default()
                },
                root_span: root,
                metrics: TraceMetrics::default(),
            }
        }
    }

    /// Export to JSON string (OpenInference format).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Export to a JSON file.
    pub fn to_file(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let json = self.to_json().map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }

    /// Count total spans in the tree.
    pub fn span_count(&self) -> usize {
        self.root_span.count_spans()
    }

    /// Count total events in the tree.
    pub fn event_count(&self) -> usize {
        self.root_span.count_events()
    }
}

/// Recursively assemble children into a parent span.
/// For each span, find all spans that have it as parent and nest them.
fn assemble_children(parent: Span, all_spans: &HashMap<String, Span>) -> Span {
    let mut assembled = parent;

    // Find all spans whose parent_span_id matches this span's ID
    let child_ids: Vec<String> = all_spans.values()
        .filter(|s| s.parent_span_id.as_deref() == Some(&assembled.span_id))
        .map(|s| s.span_id.clone())
        .collect();

    for child_id in child_ids {
        if let Some(child) = all_spans.get(&child_id).cloned() {
            let assembled_child = assemble_children(child, all_spans);
            assembled.add_child(assembled_child);
        }
    }

    assembled
}