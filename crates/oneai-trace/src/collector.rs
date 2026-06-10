//! Trace collector — trait for routing trace data to different backends.
//!
//! Implementations:
//! - InMemoryCollector: stores all spans for later JSON export
//! - FileCollector: writes JSON lines to a file
//! - NoopCollector: silently drops all events (for disabled tracing)

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::span::Span;
use crate::event::TraceEvent;

// ─── TraceCollector Trait ────────────────────────────────────────────

/// Trait for collecting trace data — implement to route to
/// in-memory, file, or remote (LangSmith-compatible) backends.
#[async_trait]
pub trait TraceCollector: Send + Sync {
    /// Called when a span is started.
    async fn on_span_start(&self, span: &Span);

    /// Called when a span is ended.
    async fn on_span_end(&self, span: &Span);

    /// Called when an event is logged.
    async fn on_event(&self, event: &TraceEvent, span_id: &str);

    /// Called when the session ends — flush all data.
    async fn flush(&self) -> Result<(), String>;
}

// ─── InMemoryCollector ──────────────────────────────────────────────

/// In-memory collector — stores all spans for later JSON export.
///
/// Spans are stored in a HashMap keyed by span_id.
/// When build_tree() is called, they are assembled into a TraceTree.
pub struct InMemoryCollector {
    spans: Mutex<HashMap<String, Span>>,
}

impl InMemoryCollector {
    /// Create a new in-memory collector.
    pub fn new() -> Self {
        Self {
            spans: Mutex::new(HashMap::new()),
        }
    }

    /// Get all collected spans.
    pub fn get_spans(&self) -> HashMap<String, Span> {
        self.spans.lock().unwrap().clone()
    }

    /// Get a span by ID.
    pub fn get_span(&self, span_id: &str) -> Option<Span> {
        self.spans.lock().unwrap().get(span_id).cloned()
    }

    /// Count collected spans.
    pub fn span_count(&self) -> usize {
        self.spans.lock().unwrap().len()
    }
}

impl Default for InMemoryCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TraceCollector for InMemoryCollector {
    async fn on_span_start(&self, span: &Span) {
        self.spans.lock().unwrap().insert(span.span_id.clone(), span.clone());
    }

    async fn on_span_end(&self, span: &Span) {
        self.spans.lock().unwrap().insert(span.span_id.clone(), span.clone());
    }

    async fn on_event(&self, _event: &TraceEvent, _span_id: &str) {
        // Events are added directly to spans via TraceContext, not here
    }

    async fn flush(&self) -> Result<(), String> {
        // In-memory collector doesn't need to flush
        Ok(())
    }
}

// ─── FileCollector ──────────────────────────────────────────────────

/// File collector — writes JSON lines to a file (one span per line).
///
/// Each completed span is written as a JSON line to the output file.
/// The file can be read later and assembled into a TraceTree.
pub struct FileCollector {
    path: PathBuf,
    writer: Mutex<BufWriter<std::fs::File>>,
}

impl FileCollector {
    /// Create a new file collector that writes to the given path.
    pub fn new(path: &str) -> Self {
        let file = std::fs::File::create(path)
            .expect("Failed to create trace output file");
        Self {
            path: PathBuf::from(path),
            writer: Mutex::new(BufWriter::new(file)),
        }
    }

    /// Get the output file path.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

#[async_trait]
impl TraceCollector for FileCollector {
    async fn on_span_start(&self, _span: &Span) {
        // Don't write on start — wait for the span to complete
    }

    async fn on_span_end(&self, span: &Span) {
        let json = serde_json::to_string(span).unwrap_or_default();
        let mut writer = self.writer.lock().unwrap();
        use std::io::Write;
        writer.write_all(json.as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
    }

    async fn on_event(&self, _event: &TraceEvent, _span_id: &str) {
        // Events are embedded in spans
    }

    async fn flush(&self) -> Result<(), String> {
        let mut writer = self.writer.lock().unwrap();
        writer.flush().map_err(|e: std::io::Error| e.to_string())
    }
}

// ─── NoopCollector ──────────────────────────────────────────────────

/// No-op collector — silently drops all events.
/// Used when tracing is disabled or for testing without collection.
pub struct NoopCollector;

#[async_trait]
impl TraceCollector for NoopCollector {
    async fn on_span_start(&self, _span: &Span) {}
    async fn on_span_end(&self, _span: &Span) {}
    async fn on_event(&self, _event: &TraceEvent, _span_id: &str) {}
    async fn flush(&self) -> Result<(), String> { Ok(()) }
}