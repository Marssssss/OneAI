//! Zero-cost stubs when the "trace" feature is disabled.
//!
//! When `#[cfg(not(feature = "trace"))]`, all trace types become
//! no-op stubs that compile away completely. This ensures zero runtime
//! overhead when tracing is not needed.

#[cfg(not(feature = "trace"))]
use std::collections::HashMap;

#[cfg(not(feature = "trace"))]
use serde_json::Value;

// ─── Stub Types (feature disabled) ──────────────────────────────────

/// No-op TraceContext — all methods are empty stubs.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone)]
pub struct TraceContext;

#[cfg(not(feature = "trace"))]
impl TraceContext {
    pub fn enter_span(&self, _kind: SpanKind, _name: &str, _parent_id: Option<&str>) -> String {
        String::new()
    }

    pub fn exit_span(&self, _span_id: &str, _status: SpanStatus) {}

    pub fn log_event(&self, _kind: EventKind, _name: &str, _attrs: HashMap<String, Value>) {}

    pub fn log_event_in_span(&self, _span_id: &str, _kind: EventKind, _name: &str, _attrs: HashMap<String, Value>) {}

    pub fn set_attribute(&self, _key: &str, _value: Value) {}

    pub fn set_attribute_on_span(&self, _span_id: &str, _key: &str, _value: Value) {}

    pub fn current_span_id(&self) -> Option<String> { None }

    pub fn is_enabled(&self) -> bool { false }

    pub fn set_enabled(&self, _enabled: bool) {}

    pub fn set_session_id(&self, _session_id: &str) {}

    pub fn session_id(&self) -> Option<String> { None }

    pub fn build_tree(&self) -> TraceTree {
        TraceTree::default()
    }
}

/// Stub SpanKind — empty enum with no variants when trace is disabled.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone, Copy)]
pub enum SpanKind {
    _Hidden,
}

/// Stub SpanStatus — empty enum.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone, Copy)]
pub enum SpanStatus {
    _Hidden,
}

/// Stub EventKind — empty enum.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone, Copy)]
pub enum EventKind {
    _Hidden,
}

/// Stub Span — no fields when trace is disabled.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone)]
pub struct Span;

/// Stub TraceEvent — no fields when trace is disabled.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone)]
pub struct TraceEvent;

/// Stub TraceTree — no fields when trace is disabled.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone)]
pub struct TraceTree;

#[cfg(not(feature = "trace"))]
impl Default for TraceTree {
    fn default() -> Self { Self }
}

/// Stub TraceMetrics — no fields when trace is disabled.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone)]
pub struct TraceMetrics;

#[cfg(not(feature = "trace"))]
impl Default for TraceMetrics {
    fn default() -> Self { Self }
}

/// Stub TraceMetadata — no fields when trace is disabled.
#[cfg(not(feature = "trace"))]
#[derive(Debug, Clone)]
pub struct TraceMetadata;

#[cfg(not(feature = "trace"))]
impl Default for TraceMetadata {
    fn default() -> Self { Self }
}

/// Stub TraceCollector trait — empty interface.
#[cfg(not(feature = "trace"))]
pub trait TraceCollector: Send + Sync {}

/// Stub TraceEmitter — empty singleton.
#[cfg(not(feature = "trace"))]
pub struct TraceEmitter;

#[cfg(not(feature = "trace"))]
impl TraceEmitter {
    pub fn global() -> &'static TraceEmitter {
        static EMITTER: TraceEmitter = TraceEmitter;
        &EMITTER
    }

    pub fn create_context(&self) -> TraceContext { TraceContext }
    pub fn create_disabled_context(&self) -> TraceContext { TraceContext }
}

/// Stub InMemoryCollector — no-op.
#[cfg(not(feature = "trace"))]
pub struct InMemoryCollector;

/// Stub FileCollector — no-op.
#[cfg(not(feature = "trace"))]
pub struct FileCollector;

/// Stub NoopCollector — no-op.
#[cfg(not(feature = "trace"))]
pub struct NoopCollector;