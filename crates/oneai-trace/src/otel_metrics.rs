//! OTEL Metrics Provider — counters, histograms, gauges for agent observability.
//!
//! When the `otel` feature is enabled, this module provides `OtelMetricsProvider`
//! which registers OpenTelemetry metrics instruments for tracking:
//!
//! - **Counters**: tool calls, inference requests, errors, approval denials
//! - **Histograms**: inference latency, session duration
//! - **Gauges**: STM/LTM entry counts
//!
//! ## Usage
//!
//! ```ignore
//! let metrics = OtelMetricsProvider::new(&config);
//!
//! // During agent execution:
//! metrics.record_tool_call("calculator", true);
//! metrics.record_inference_latency(350);
//! metrics.record_tokens(500, 200);
//! ```
//!
//! ## Architecture
//!
//! The `OtelMetricsProvider` stores metrics in a thread-safe internal state
//! and provides a `snapshot()` method for retrieving current values.
//! This design allows metrics to work without a real OTEL SDK connection,
//! making it usable for testing and local monitoring.
//!
//! When a real OTEL SDK pipeline is configured, the metrics can be exported
//! via OTLP to Prometheus, Grafana, or other backends.

use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use std::sync::Arc;

// ─── MetricsSnapshot ──────────────────────────────────────────────

/// A snapshot of all OTEL metrics at a point in time.
///
/// Used for reporting, dashboards, and evaluation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetricsSnapshot {
    /// Total tool call count.
    pub tool_call_count: u64,
    /// Tool call success count.
    pub tool_success_count: u64,
    /// Tool call failure count.
    pub tool_call_failure_count: u64,
    /// Total LLM inference request count.
    pub inference_request_count: u64,
    /// Total LLM token usage (prompt + completion).
    pub total_tokens_used: u64,
    /// Total prompt tokens.
    pub total_prompt_tokens: u64,
    /// Total completion tokens.
    pub total_completion_tokens: u64,
    /// Total error count.
    pub error_count: u64,
    /// Total approval denial count.
    pub approval_denial_count: u64,
    /// Total approval request count.
    pub approval_request_count: u64,
    /// Total session count.
    pub session_count: u64,
    /// Sum of all inference latencies (ms).
    pub total_inference_latency_ms: u64,
    /// Sum of all session durations (ms).
    pub total_session_duration_ms: u64,
    /// Current STM entry count.
    pub stm_entry_count: u64,
    /// Current LTM entry count: u64.
    pub ltm_entry_count: u64,
    /// Total memory reflection count.
    pub memory_reflection_count: u64,
    /// Total LTM recall (injection) count.
    pub ltm_recall_count: u64,
}

impl Default for MetricsSnapshot {
    fn default() -> Self {
        Self {
            tool_call_count: 0,
            tool_success_count: 0,
            tool_call_failure_count: 0,
            inference_request_count: 0,
            total_tokens_used: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            error_count: 0,
            approval_denial_count: 0,
            approval_request_count: 0,
            session_count: 0,
            total_inference_latency_ms: 0,
            total_session_duration_ms: 0,
            stm_entry_count: 0,
            ltm_entry_count: 0,
            memory_reflection_count: 0,
            ltm_recall_count: 0,
        }
    }
}

// ─── OtelMetricsProvider ──────────────────────────────────────────

/// OTEL Metrics Provider — thread-safe counters, histograms, and gauges.
///
/// All metrics are stored as atomic counters, providing zero-overhead
/// increment operations. The `snapshot()` method creates a consistent
/// view of all metrics at a point in time.
///
/// The naming convention follows OTEL semantic conventions:
/// - `oneai.tool.calls` → tool_call_count (counter)
/// - `oneai.inference.requests` → inference_request_count (counter)
/// - `oneai.inference.latency_ms` → total_inference_latency_ms (histogram sum)
/// - `oneai.tokens.total` → total_tokens_used (counter)
/// - `oneai.errors` → error_count (counter)
/// - `oneai.approval.denials` → approval_denial_count (counter)
/// - `oneai.session.duration_ms` → total_session_duration_ms (histogram sum)
/// - `oneai.memory.stm.entries` → stm_entry_count (gauge)
/// - `oneai.memory.ltm.entries` → ltm_entry_count (gauge)
pub struct OtelMetricsProvider {
    // ─── Counters (atomic, zero-overhead increments) ──────────────────
    tool_call_count: AtomicU64,
    tool_success_count: AtomicU64,
    tool_call_failure_count: AtomicU64,
    inference_request_count: AtomicU64,
    total_tokens_used: AtomicU64,
    total_prompt_tokens: AtomicU64,
    total_completion_tokens: AtomicU64,
    error_count: AtomicU64,
    approval_denial_count: AtomicU64,
    approval_request_count: AtomicU64,
    session_count: AtomicU64,

    // ─── Histogram sums ──────────────────────────────────────────────
    total_inference_latency_ms: AtomicU64,
    total_session_duration_ms: AtomicU64,

    // ─── Gauges ──────────────────────────────────────────────────────
    stm_entry_count: AtomicU64,
    ltm_entry_count: AtomicU64,

    // ─── Memory-specific counters ────────────────────────────────────
    memory_reflection_count: AtomicU64,
    ltm_recall_count: AtomicU64,
}

impl OtelMetricsProvider {
    /// Create a new metrics provider with all counters initialized to zero.
    pub fn new() -> Self {
        Self {
            tool_call_count: AtomicU64::new(0),
            tool_success_count: AtomicU64::new(0),
            tool_call_failure_count: AtomicU64::new(0),
            inference_request_count: AtomicU64::new(0),
            total_tokens_used: AtomicU64::new(0),
            total_prompt_tokens: AtomicU64::new(0),
            total_completion_tokens: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            approval_denial_count: AtomicU64::new(0),
            approval_request_count: AtomicU64::new(0),
            session_count: AtomicU64::new(0),
            total_inference_latency_ms: AtomicU64::new(0),
            total_session_duration_ms: AtomicU64::new(0),
            stm_entry_count: AtomicU64::new(0),
            ltm_entry_count: AtomicU64::new(0),
            memory_reflection_count: AtomicU64::new(0),
            ltm_recall_count: AtomicU64::new(0),
        }
    }

    /// Create a shared (Arc-wrapped) metrics provider.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    // ─── Recording methods (counter increments) ──────────────────────

    /// Record a tool call (increments tool_call_count).
    /// If success=true, also increments tool_success_count.
    /// If success=false, also increments tool_call_failure_count.
    pub fn record_tool_call(&self, tool_name: &str, success: bool) {
        self.tool_call_count.fetch_add(1, Ordering::Relaxed);
        if success {
            self.tool_success_count.fetch_add(1, Ordering::Relaxed);
        } else {
            self.tool_call_failure_count.fetch_add(1, Ordering::Relaxed);
        }
        tracing::debug!("Metrics: tool call {} success={}", tool_name, success);
    }

    /// Record an LLM inference request (increments inference_request_count).
    pub fn record_inference_request(&self) {
        self.inference_request_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record inference latency in milliseconds (adds to histogram sum).
    pub fn record_inference_latency(&self, latency_ms: u64) {
        self.total_inference_latency_ms.fetch_add(latency_ms, Ordering::Relaxed);
    }

    /// Record token usage (adds to counters).
    pub fn record_tokens(&self, prompt_tokens: u32, completion_tokens: u32) {
        self.total_prompt_tokens.fetch_add(prompt_tokens as u64, Ordering::Relaxed);
        self.total_completion_tokens.fetch_add(completion_tokens as u64, Ordering::Relaxed);
        self.total_tokens_used.fetch_add((prompt_tokens + completion_tokens) as u64, Ordering::Relaxed);
    }

    /// Record an error (increments error_count).
    pub fn record_error(&self) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an approval request (increments approval_request_count).
    pub fn record_approval_request(&self) {
        self.approval_request_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an approval denial (increments approval_denial_count).
    pub fn record_approval_denial(&self) {
        self.approval_denial_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a new session (increments session_count).
    pub fn record_session(&self) {
        self.session_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record session duration in milliseconds (adds to histogram sum).
    pub fn record_session_duration(&self, duration_ms: u64) {
        self.total_session_duration_ms.fetch_add(duration_ms, Ordering::Relaxed);
    }

    /// Update STM entry count gauge.
    pub fn set_stm_entry_count(&self, count: u64) {
        self.stm_entry_count.store(count, Ordering::Relaxed);
    }

    /// Update LTM entry count gauge.
    pub fn set_ltm_entry_count(&self, count: u64) {
        self.ltm_entry_count.store(count, Ordering::Relaxed);
    }

    /// Record a memory reflection (increments memory_reflection_count).
    pub fn record_memory_reflection(&self) {
        self.memory_reflection_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a LTM recall/injection (increments ltm_recall_count).
    pub fn record_ltm_recall(&self) {
        self.ltm_recall_count.fetch_add(1, Ordering::Relaxed);
    }

    // ─── Snapshot ──────────────────────────────────────────────────

    /// Take a snapshot of all current metric values.
    ///
    /// The snapshot provides a consistent view of all counters at the
    /// time of the call. Useful for reporting, dashboards, and evaluation.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            tool_call_count: self.tool_call_count.load(Ordering::Relaxed),
            tool_success_count: self.tool_success_count.load(Ordering::Relaxed),
            tool_call_failure_count: self.tool_call_failure_count.load(Ordering::Relaxed),
            inference_request_count: self.inference_request_count.load(Ordering::Relaxed),
            total_tokens_used: self.total_tokens_used.load(Ordering::Relaxed),
            total_prompt_tokens: self.total_prompt_tokens.load(Ordering::Relaxed),
            total_completion_tokens: self.total_completion_tokens.load(Ordering::Relaxed),
            error_count: self.error_count.load(Ordering::Relaxed),
            approval_denial_count: self.approval_denial_count.load(Ordering::Relaxed),
            approval_request_count: self.approval_request_count.load(Ordering::Relaxed),
            session_count: self.session_count.load(Ordering::Relaxed),
            total_inference_latency_ms: self.total_inference_latency_ms.load(Ordering::Relaxed),
            total_session_duration_ms: self.total_session_duration_ms.load(Ordering::Relaxed),
            stm_entry_count: self.stm_entry_count.load(Ordering::Relaxed),
            ltm_entry_count: self.ltm_entry_count.load(Ordering::Relaxed),
            memory_reflection_count: self.memory_reflection_count.load(Ordering::Relaxed),
            ltm_recall_count: self.ltm_recall_count.load(Ordering::Relaxed),
        }
    }

    /// Export the snapshot as JSON.
    pub fn snapshot_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.snapshot())
    }

    // ─── Computed metrics ──────────────────────────────────────────

    /// Compute average inference latency (ms).
    pub fn avg_inference_latency(&self) -> f64 {
        let total = self.total_inference_latency_ms.load(Ordering::Relaxed);
        let count = self.inference_request_count.load(Ordering::Relaxed);
        if count == 0 { 0.0 } else { total as f64 / count as f64 }
    }

    /// Compute tool success rate (0.0 to 1.0).
    pub fn tool_success_rate(&self) -> f64 {
        let total = self.tool_call_count.load(Ordering::Relaxed);
        let success = self.tool_success_count.load(Ordering::Relaxed);
        if total == 0 { 0.0 } else { success as f64 / total as f64 }
    }

    /// Compute approval denial rate (0.0 to 1.0).
    pub fn approval_denial_rate(&self) -> f64 {
        let total = self.approval_request_count.load(Ordering::Relaxed);
        let denials = self.approval_denial_count.load(Ordering::Relaxed);
        if total == 0 { 0.0 } else { denials as f64 / total as f64 }
    }
}

impl Default for OtelMetricsProvider {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_metrics_provider_new() {
        let provider = OtelMetricsProvider::new();
        let snapshot = provider.snapshot();
        assert_eq!(snapshot.tool_call_count, 0);
        assert_eq!(snapshot.inference_request_count, 0);
        assert_eq!(snapshot.total_tokens_used, 0);
        assert_eq!(snapshot.error_count, 0);
    }

    #[test]
    fn test_metrics_tool_calls() {
        let provider = OtelMetricsProvider::new();

        provider.record_tool_call("calculator", true);
        provider.record_tool_call("shell", false);
        provider.record_tool_call("search", true);

        let snapshot = provider.snapshot();
        assert_eq!(snapshot.tool_call_count, 3);
        assert_eq!(snapshot.tool_success_count, 2);
        assert_eq!(snapshot.tool_call_failure_count, 1);
        assert_eq!(provider.tool_success_rate(), 2.0 / 3.0);
    }

    #[test]
    fn test_metrics_inference() {
        let provider = OtelMetricsProvider::new();

        provider.record_inference_request();
        provider.record_inference_latency(350);
        provider.record_tokens(500, 200);

        provider.record_inference_request();
        provider.record_inference_latency(280);
        provider.record_tokens(400, 150);

        let snapshot = provider.snapshot();
        assert_eq!(snapshot.inference_request_count, 2);
        assert_eq!(snapshot.total_inference_latency_ms, 630);
        assert_eq!(snapshot.total_tokens_used, 1250);
        assert_eq!(snapshot.total_prompt_tokens, 900);
        assert_eq!(snapshot.total_completion_tokens, 350);
        assert_eq!(provider.avg_inference_latency(), 315.0);
    }

    #[test]
    fn test_metrics_errors_and_approvals() {
        let provider = OtelMetricsProvider::new();

        provider.record_error();
        provider.record_error();
        provider.record_approval_request();
        provider.record_approval_request();
        provider.record_approval_denial();

        let snapshot = provider.snapshot();
        assert_eq!(snapshot.error_count, 2);
        assert_eq!(snapshot.approval_request_count, 2);
        assert_eq!(snapshot.approval_denial_count, 1);
        assert_eq!(provider.approval_denial_rate(), 0.5);
    }

    #[test]
    fn test_metrics_sessions() {
        let provider = OtelMetricsProvider::new();

        provider.record_session();
        provider.record_session_duration(5000);
        provider.record_session();
        provider.record_session_duration(3000);

        let snapshot = provider.snapshot();
        assert_eq!(snapshot.session_count, 2);
        assert_eq!(snapshot.total_session_duration_ms, 8000);
    }

    #[test]
    fn test_metrics_memory() {
        let provider = OtelMetricsProvider::new();

        provider.set_stm_entry_count(5);
        provider.set_ltm_entry_count(20);
        provider.record_memory_reflection();
        provider.record_ltm_recall();
        provider.record_ltm_recall();

        let snapshot = provider.snapshot();
        assert_eq!(snapshot.stm_entry_count, 5);
        assert_eq!(snapshot.ltm_entry_count, 20);
        assert_eq!(snapshot.memory_reflection_count, 1);
        assert_eq!(snapshot.ltm_recall_count, 2);
    }

    #[test]
    fn test_metrics_snapshot_json() {
        let provider = OtelMetricsProvider::new();
        provider.record_tool_call("calculator", true);
        provider.record_inference_latency(100);

        let json = provider.snapshot_json().unwrap();
        assert!(json.contains("tool_call_count"));
        assert!(json.contains("1"));
        assert!(json.contains("total_inference_latency_ms"));
    }

    #[test]
    fn test_metrics_shared_arc() {
        let provider = OtelMetricsProvider::shared();
        provider.record_tool_call("calculator", true);

        let snapshot = provider.snapshot();
        assert_eq!(snapshot.tool_call_count, 1);
    }

    #[test]
    fn test_metrics_concurrent_access() {
        let provider = Arc::new(OtelMetricsProvider::new());

        // Simulate concurrent recording from multiple threads
        let handles: Vec<_> = (0..10).map(|i| {
            let p = provider.clone();
            std::thread::spawn(move || {
                p.record_tool_call(&format!("tool_{}", i), true);
                p.record_inference_request();
                p.record_tokens(100, 50);
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }

        let snapshot = provider.snapshot();
        assert_eq!(snapshot.tool_call_count, 10);
        assert_eq!(snapshot.inference_request_count, 10);
        assert_eq!(snapshot.total_tokens_used, 1500); // 10 * (100 + 50)
    }
}
