//! Trace emitter — global singleton for trace initialization and context creation.
//!
//! The TraceEmitter is the primary entry point for all trace operations.
//! It holds the global configuration (collector, enabled flag) and
//! creates TraceContext instances for each session.

use std::sync::Arc;
use std::sync::OnceLock;

use crate::context::TraceContext;
use crate::collector::TraceCollector;

// ─── TraceEmitter ────────────────────────────────────────────────────

/// Global trace emitter singleton — the primary API for all crates.
///
/// Usage:
/// ```ignore
/// // Initialize at app startup:
/// TraceEmitter::global().initialize(Arc::new(InMemoryCollector::new()));
///
/// // Create a context for each session:
/// let ctx = TraceEmitter::global().create_context();
///
/// // Use the context during execution:
/// let span_id = ctx.enter_span(SpanKind::SESSION, "session", None);
/// ctx.log_event(EventKind::Thought, "agent.thought", attrs);
/// ctx.exit_span(span_id, SpanStatus::Ok);
///
/// // Build the tree at the end:
/// let tree = ctx.build_tree();
/// println!("{}", tree.to_json().unwrap());
/// ```
pub struct TraceEmitter {
    default_collector: OnceLock<Arc<dyn TraceCollector>>,
}

impl TraceEmitter {
    /// Get the global emitter instance.
    pub fn global() -> &'static TraceEmitter {
        static EMITTER: OnceLock<TraceEmitter> = OnceLock::new();
        EMITTER.get_or_init(|| TraceEmitter {
            default_collector: OnceLock::new(),
        })
    }

    /// Initialize with a default collector (called once at app startup).
    ///
    /// If this is called multiple times, only the first call takes effect.
    pub fn initialize(&self, collector: Arc<dyn TraceCollector>) {
        self.default_collector.set(collector).ok();
    }

    /// Create a trace context for a new session.
    ///
    /// Uses the default collector if initialized, otherwise uses a NoopCollector.
    pub fn create_context(&self) -> TraceContext {
        let collector = self.default_collector.get()
            .cloned()
            .unwrap_or_else(|| Arc::new(crate::collector::NoopCollector));
        TraceContext::new(collector)
    }

    /// Create a trace context with a specific collector (overrides default).
    pub fn create_context_with_collector(&self, collector: Arc<dyn TraceCollector>) -> TraceContext {
        TraceContext::new(collector)
    }

    /// Create a disabled trace context (no events will be collected).
    pub fn create_disabled_context(&self) -> TraceContext {
        TraceContext::disabled()
    }
}