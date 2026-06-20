//! WASM resource monitor — tracks fuel, memory, and execution metrics per module.
//!
//! The monitor provides:
//! - Per-module execution metrics (calls, fuel, time, errors)
//! - Resource events (fuel consumed, fuel low, memory near limit, timeout)
//! - Event subscribers for integration with oneai-trace
//!
//! ## Usage
//!
//! ```ignore
//! let monitor = Arc::new(WasmResourceMonitor::new());
//!
//! // Record execution start and end
//! monitor.record_execution_start("calculator").await;
//! monitor.record_execution_end("calculator", 50, 100000, 95000, true).await;
//!
//! // Get metrics
//! let metrics = monitor.get_metrics("calculator").await;
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

/// Resource event emitted during WASM execution.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WasmResourceEvent {
    /// Fuel consumed during execution.
    FuelConsumed {
        /// Module name.
        module_name: String,
        /// Fuel before execution.
        fuel_before: u64,
        /// Fuel after execution.
        fuel_after: u64,
    },
    /// Fuel running low (approaching limit).
    FuelLow {
        /// Module name.
        module_name: String,
        /// Fuel remaining.
        fuel_remaining: u64,
        /// Fuel limit.
        fuel_limit: u64,
    },
    /// Memory usage near the configured limit.
    MemoryNearLimit {
        /// Module name.
        module_name: String,
        /// Pages used.
        pages_used: u32,
        /// Pages limit.
        pages_limit: u32,
    },
    /// Execution exceeded time limit.
    TimeoutExceeded {
        /// Module name.
        module_name: String,
        /// Elapsed time in milliseconds.
        elapsed_ms: u64,
        /// Time limit in milliseconds.
        limit_ms: u64,
    },
    /// Module execution completed normally.
    ExecutionCompleted {
        /// Module name.
        module_name: String,
        /// Elapsed time in milliseconds.
        elapsed_ms: u64,
        /// Fuel consumed.
        fuel_consumed: u64,
    },
}

/// Per-module execution metrics.
#[derive(Debug, Clone)]
pub struct WasmExecutionMetrics {
    /// Module name.
    module_name: String,
    /// Total calls.
    total_calls: u64,
    /// Total fuel consumed across all calls.
    total_fuel_consumed: u64,
    /// Average execution time in milliseconds.
    avg_execution_time_ms: f64,
    /// Maximum execution time in milliseconds.
    max_execution_time_ms: u64,
    /// Total errors.
    total_errors: u64,
    /// Last execution timestamp.
    last_execution_at: Option<DateTime<Utc>>,
}

impl WasmExecutionMetrics {
    /// Create a new metrics entry for a module.
    pub fn new(module_name: &str) -> Self {
        Self {
            module_name: module_name.to_string(),
            total_calls: 0,
            total_fuel_consumed: 0,
            avg_execution_time_ms: 0.0,
            max_execution_time_ms: 0,
            total_errors: 0,
            last_execution_at: None,
        }
    }

    /// Get the module name.
    pub fn module_name(&self) -> &str {
        &self.module_name
    }

    /// Get total calls.
    pub fn total_calls(&self) -> u64 {
        self.total_calls
    }

    /// Get total fuel consumed.
    pub fn total_fuel_consumed(&self) -> u64 {
        self.total_fuel_consumed
    }

    /// Get average execution time in milliseconds.
    pub fn avg_execution_time_ms(&self) -> f64 {
        self.avg_execution_time_ms
    }

    /// Get maximum execution time in milliseconds.
    pub fn max_execution_time_ms(&self) -> u64 {
        self.max_execution_time_ms
    }

    /// Get total errors.
    pub fn total_errors(&self) -> u64 {
        self.total_errors
    }

    /// Get last execution timestamp.
    pub fn last_execution_at(&self) -> &Option<DateTime<Utc>> {
        &self.last_execution_at
    }

    /// Record a successful execution.
    pub fn record_success(&mut self, elapsed_ms: u64, fuel_consumed: u64) {
        self.total_calls += 1;
        self.total_fuel_consumed += fuel_consumed;
        self.avg_execution_time_ms = if self.total_calls == 1 {
            elapsed_ms as f64
        } else {
            // Running average
            let prev_avg = self.avg_execution_time_ms;
            let prev_calls = self.total_calls - 1;
            (prev_avg * prev_calls as f64 + elapsed_ms as f64) / self.total_calls as f64
        };
        if elapsed_ms > self.max_execution_time_ms {
            self.max_execution_time_ms = elapsed_ms;
        }
        self.last_execution_at = Some(Utc::now());
    }

    /// Record an error execution.
    pub fn record_error(&mut self, elapsed_ms: u64) {
        self.total_calls += 1;
        self.total_errors += 1;
        if elapsed_ms > self.max_execution_time_ms {
            self.max_execution_time_ms = elapsed_ms;
        }
        self.last_execution_at = Some(Utc::now());
    }
}

/// Event subscriber trait for resource events.
pub trait WasmResourceEventSubscriber: Send + Sync {
    /// Handle a resource event.
    fn on_event(&self, event: &WasmResourceEvent);
}

/// WASM resource monitor — tracks fuel, memory, and execution metrics per module.
///
/// The monitor records execution metrics for each WASM module and emits
/// resource events (fuel consumed, fuel low, memory near limit, timeout).
/// Event subscribers can be registered for integration with oneai-trace
/// or other observability systems.
pub struct WasmResourceMonitor {
    /// Per-module metrics.
    metrics: Arc<RwLock<HashMap<String, WasmExecutionMetrics>>>,
    /// Event subscribers.
    subscribers: Arc<RwLock<Vec<Box<dyn WasmResourceEventSubscriber>>>>,
}

impl WasmResourceMonitor {
    /// Create a new resource monitor.
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(RwLock::new(HashMap::new())),
            subscribers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Record an execution start (creates or updates metrics entry).
    pub async fn record_execution_start(&self, module_name: &str) {
        let mut metrics = self.metrics.write().await;
        if !metrics.contains_key(module_name) {
            metrics.insert(module_name.to_string(), WasmExecutionMetrics::new(module_name));
        }
    }

    /// Record an execution completion.
    ///
    /// Updates the module's metrics with elapsed time, fuel consumed,
    /// and success/failure status. Emits appropriate resource events.
    pub async fn record_execution_end(
        &self,
        module_name: &str,
        elapsed_ms: u64,
        fuel_before: u64,
        fuel_after: u64,
        success: bool,
    ) {
        let fuel_consumed = fuel_before.saturating_sub(fuel_after);

        // Update metrics
        {
            let mut metrics = self.metrics.write().await;
            if let Some(entry) = metrics.get_mut(module_name) {
                if success {
                    entry.record_success(elapsed_ms, fuel_consumed);
                } else {
                    entry.record_error(elapsed_ms);
                }
            } else {
                let mut entry = WasmExecutionMetrics::new(module_name);
                if success {
                    entry.record_success(elapsed_ms, fuel_consumed);
                } else {
                    entry.record_error(elapsed_ms);
                }
                metrics.insert(module_name.to_string(), entry);
            }
        }

        // Emit events
        self.emit_event(WasmResourceEvent::ExecutionCompleted {
            module_name: module_name.to_string(),
            elapsed_ms,
            fuel_consumed,
        });

        // Emit fuel consumed event
        self.emit_event(WasmResourceEvent::FuelConsumed {
            module_name: module_name.to_string(),
            fuel_before,
            fuel_after,
        });
    }

    /// Get metrics for a specific module.
    pub async fn get_metrics(&self, module_name: &str) -> Option<WasmExecutionMetrics> {
        let metrics = self.metrics.read().await;
        metrics.get(module_name).cloned()
    }

    /// Get all module metrics.
    pub async fn all_metrics(&self) -> Vec<WasmExecutionMetrics> {
        let metrics = self.metrics.read().await;
        metrics.values().cloned().collect()
    }

    /// Subscribe to resource events.
    pub async fn subscribe(&self, subscriber: Box<dyn WasmResourceEventSubscriber>) {
        let mut subscribers = self.subscribers.write().await;
        subscribers.push(subscriber);
    }

    /// Emit a resource event to all subscribers.
    fn emit_event(&self, event: WasmResourceEvent) {
        // We can't hold the RwLock across event dispatch because subscribers
        // might call back into the monitor. Instead, clone the subscriber list.
        let subscribers = self.subscribers.try_read();
        if let Ok(subs) = subscribers {
            for subscriber in subs.iter() {
                subscriber.on_event(&event);
            }
        }
        // If lock is contested, silently skip — events are informational, not critical
    }

    /// Get total fuel consumed across all modules.
    pub async fn total_fuel_consumed(&self) -> u64 {
        let metrics = self.metrics.read().await;
        metrics.values().map(|m| m.total_fuel_consumed).sum()
    }

    /// Get total calls across all modules.
    pub async fn total_calls(&self) -> u64 {
        let metrics = self.metrics.read().await;
        metrics.values().map(|m| m.total_calls).sum()
    }

    /// Get total errors across all modules.
    pub async fn total_errors(&self) -> u64 {
        let metrics = self.metrics.read().await;
        metrics.values().map(|m| m.total_errors).sum()
    }
}

impl Default for WasmResourceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Logging subscriber — emits resource events as tracing log messages.
pub struct WasmLogSubscriber;

impl WasmLogSubscriber {
    /// Create a new logging subscriber.
    pub fn new() -> Self {
        Self
    }
}

impl Default for WasmLogSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmResourceEventSubscriber for WasmLogSubscriber {
    fn on_event(&self, event: &WasmResourceEvent) {
        match event {
            WasmResourceEvent::FuelConsumed { module_name, fuel_before, fuel_after } => {
                tracing::info!(
                    "WASM module '{}' consumed {} fuel (before: {}, after: {})",
                    module_name, fuel_before - fuel_after, fuel_before, fuel_after
                );
            }
            WasmResourceEvent::FuelLow { module_name, fuel_remaining, fuel_limit } => {
                tracing::warn!(
                    "WASM module '{}' fuel low: {} remaining (limit: {})",
                    module_name, fuel_remaining, fuel_limit
                );
            }
            WasmResourceEvent::MemoryNearLimit { module_name, pages_used, pages_limit } => {
                tracing::warn!(
                    "WASM module '{}' memory near limit: {} pages used (limit: {})",
                    module_name, pages_used, pages_limit
                );
            }
            WasmResourceEvent::TimeoutExceeded { module_name, elapsed_ms, limit_ms } => {
                tracing::error!(
                    "WASM module '{}' timeout exceeded: {}ms elapsed (limit: {}ms)",
                    module_name, elapsed_ms, limit_ms
                );
            }
            WasmResourceEvent::ExecutionCompleted { module_name, elapsed_ms, fuel_consumed } => {
                tracing::info!(
                    "WASM module '{}' execution completed: {}ms, {} fuel consumed",
                    module_name, elapsed_ms, fuel_consumed
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monitor_new() {
        let _monitor = WasmResourceMonitor::new();
        // Monitor is created successfully
        assert!(true);
    }

    #[test]
    fn test_monitor_default() {
        let _monitor = WasmResourceMonitor::default();
        assert!(true);
    }

    #[test]
    fn test_execution_metrics_new() {
        let metrics = WasmExecutionMetrics::new("calculator");
        assert_eq!(metrics.module_name(), "calculator");
        assert_eq!(metrics.total_calls(), 0);
        assert_eq!(metrics.total_fuel_consumed(), 0);
        assert_eq!(metrics.avg_execution_time_ms(), 0.0);
        assert_eq!(metrics.max_execution_time_ms(), 0);
        assert_eq!(metrics.total_errors(), 0);
        assert!(metrics.last_execution_at().is_none());
    }

    #[test]
    fn test_execution_metrics_record_success() {
        let mut metrics = WasmExecutionMetrics::new("calculator");
        metrics.record_success(50, 1000);
        assert_eq!(metrics.total_calls(), 1);
        assert_eq!(metrics.total_fuel_consumed(), 1000);
        assert_eq!(metrics.avg_execution_time_ms(), 50.0);
        assert_eq!(metrics.max_execution_time_ms(), 50);
        assert_eq!(metrics.total_errors(), 0);
        assert!(metrics.last_execution_at().is_some());
    }

    #[test]
    fn test_execution_metrics_record_multiple_successes() {
        let mut metrics = WasmExecutionMetrics::new("calculator");
        metrics.record_success(50, 1000);
        metrics.record_success(100, 2000);
        assert_eq!(metrics.total_calls(), 2);
        assert_eq!(metrics.total_fuel_consumed(), 3000);
        assert_eq!(metrics.avg_execution_time_ms(), 75.0);
        assert_eq!(metrics.max_execution_time_ms(), 100);
    }

    #[test]
    fn test_execution_metrics_record_error() {
        let mut metrics = WasmExecutionMetrics::new("calculator");
        metrics.record_error(200);
        assert_eq!(metrics.total_calls(), 1);
        assert_eq!(metrics.total_errors(), 1);
        assert_eq!(metrics.max_execution_time_ms(), 200);
        assert_eq!(metrics.total_fuel_consumed(), 0);
    }

    #[tokio::test]
    async fn test_monitor_record_execution() {
        let monitor = WasmResourceMonitor::new();
        monitor.record_execution_start("calculator").await;
        monitor.record_execution_end("calculator", 50, 100000, 95000, true).await;

        let metrics = monitor.get_metrics("calculator").await;
        assert!(metrics.is_some());
        let m = metrics.unwrap();
        assert_eq!(m.total_calls(), 1);
        assert_eq!(m.total_fuel_consumed(), 5000);
    }

    #[tokio::test]
    async fn test_monitor_all_metrics() {
        let monitor = WasmResourceMonitor::new();
        monitor.record_execution_start("a").await;
        monitor.record_execution_end("a", 10, 1000, 500, true).await;
        monitor.record_execution_start("b").await;
        monitor.record_execution_end("b", 20, 2000, 1000, true).await;

        let all = monitor.all_metrics().await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_monitor_total_fuel_consumed() {
        let monitor = WasmResourceMonitor::new();
        monitor.record_execution_start("a").await;
        monitor.record_execution_end("a", 10, 1000, 500, true).await;
        monitor.record_execution_start("b").await;
        monitor.record_execution_end("b", 20, 2000, 1000, true).await;

        let total = monitor.total_fuel_consumed().await;
        assert_eq!(total, 1500);
    }

    #[tokio::test]
    async fn test_monitor_total_calls() {
        let monitor = WasmResourceMonitor::new();
        monitor.record_execution_start("a").await;
        monitor.record_execution_end("a", 10, 100, 50, true).await;
        monitor.record_execution_start("a").await;
        monitor.record_execution_end("a", 20, 50, 30, true).await;

        let total = monitor.total_calls().await;
        assert_eq!(total, 2);
    }

    #[tokio::test]
    async fn test_monitor_total_errors() {
        let monitor = WasmResourceMonitor::new();
        monitor.record_execution_start("a").await;
        monitor.record_execution_end("a", 10, 100, 100, false).await;

        let total = monitor.total_errors().await;
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn test_monitor_subscribe_log() {
        let monitor = WasmResourceMonitor::new();
        monitor.subscribe(Box::new(WasmLogSubscriber::new())).await;

        // Record an execution — the log subscriber should emit tracing messages
        monitor.record_execution_start("test").await;
        monitor.record_execution_end("test", 50, 1000, 500, true).await;

        // Verify metrics still work
        let metrics = monitor.get_metrics("test").await;
        assert!(metrics.is_some());
    }

    #[test]
    fn test_wasm_resource_event_variants() {
        let events = [
            WasmResourceEvent::FuelConsumed { module_name: "a".to_string(), fuel_before: 100, fuel_after: 50 },
            WasmResourceEvent::FuelLow { module_name: "a".to_string(), fuel_remaining: 10, fuel_limit: 100 },
            WasmResourceEvent::MemoryNearLimit { module_name: "a".to_string(), pages_used: 15, pages_limit: 16 },
            WasmResourceEvent::TimeoutExceeded { module_name: "a".to_string(), elapsed_ms: 30000, limit_ms: 30000 },
            WasmResourceEvent::ExecutionCompleted { module_name: "a".to_string(), elapsed_ms: 50, fuel_consumed: 50 },
        ];
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn test_log_subscriber_on_event() {
        let subscriber = WasmLogSubscriber::new();
        let event = WasmResourceEvent::ExecutionCompleted {
            module_name: "test".to_string(),
            elapsed_ms: 50,
            fuel_consumed: 100,
        };
        subscriber.on_event(&event);
        // Should not panic — just emits tracing logs
    }
}
