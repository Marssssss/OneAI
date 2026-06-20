//! Circuit breaker for LLM provider failover.
//!
//! When a provider repeatedly fails (network errors, API errors, timeouts),
//! the circuit breaker opens and prevents further calls to that provider.
//! This allows failover to alternative providers instead of repeatedly
//! hitting a failing endpoint.
//!
//! Key concepts:
//! - `CircuitBreaker`: Trait for tracking provider health and controlling call flow
//! - `CircuitState`: Three states — Closed (healthy), Open (failing), HalfOpen (testing recovery)
//! - `ThresholdCircuitBreaker`: Classic threshold-based implementation
//! - `CircuitBreakerConfig`: Configuration for failure thresholds and timing

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ─── CircuitState ────────────────────────────────────────────────────────────

/// State of a circuit breaker for a specific provider.
///
/// Follows the classic three-state pattern:
/// - `Closed`: Provider is healthy, calls proceed normally
/// - `Open`: Provider has failed too many times, calls are rejected
/// - `HalfOpen`: Testing if the provider has recovered, limited calls allowed
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum CircuitState {
    /// Provider is healthy — calls proceed normally.
    Closed,

    /// Provider is failing — calls are rejected.
    ///
    /// The circuit opened at `since` after `failure_count` consecutive failures.
    /// It will transition to HalfOpen after `open_duration` seconds.
    Open {
        /// When the circuit opened.
        since: DateTime<Utc>,
        /// Number of consecutive failures that triggered the open state.
        failure_count: u64,
    },

    /// Provider might have recovered — limited calls allowed.
    ///
    /// If a test call succeeds, the circuit closes. If it fails,
    /// the circuit reopens for another `open_duration` period.
    HalfOpen {
        /// When the circuit entered HalfOpen state.
        since: DateTime<Utc>,
    },
}

impl CircuitState {
    /// Whether calls are allowed in this state.
    pub fn allows_calls(&self) -> bool {
        match self {
            Self::Closed => true,
            Self::Open { .. } => false,
            Self::HalfOpen { .. } => true, // limited calls allowed for testing
        }
    }

    /// Whether the provider is considered healthy.
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Closed)
    }

    /// Whether the provider is definitely failing (no calls allowed).
    pub fn is_failing(&self) -> bool {
        matches!(self, Self::Open { .. })
    }
}

impl Default for CircuitState {
    fn default() -> Self {
        Self::Closed
    }
}

// ─── CircuitBreaker trait ────────────────────────────────────────────────────

/// Trait for circuit breaker — provider failover control.
///
/// Tracks provider health and controls whether calls should proceed.
/// When a provider fails repeatedly, the circuit opens and prevents
/// further calls, allowing failover to alternative providers.
///
/// After a configurable timeout, the circuit enters HalfOpen state
/// to test if the provider has recovered. If the test succeeds,
/// the circuit closes. If it fails, the circuit reopens.
///
/// The default implementation is `ThresholdCircuitBreaker` —
/// classic threshold-based circuit breaker with configurable
/// failure/success thresholds and open duration.
pub trait CircuitBreaker: Send + Sync {
    /// Check the current circuit state for a provider.
    ///
    /// This method also handles state transitions:
    /// - If the circuit is Open and the open_duration has elapsed,
    ///   it transitions to HalfOpen.
    fn check(&self, provider: &str) -> CircuitState;

    /// Record a successful call to the provider.
    ///
    /// In HalfOpen state, this closes the circuit (provider recovered).
    /// In Closed state, this resets the failure counter.
    fn record_success(&self, provider: &str);

    /// Record a failed call to the provider.
    ///
    /// In Closed state, this increments the failure counter.
    ///   If it reaches the threshold, the circuit opens.
    /// In HalfOpen state, this reopens the circuit (provider still failing).
    fn record_failure(&self, provider: &str, error: &str);

    /// Manually reset the circuit for a provider (force Closed state).
    fn reset(&self, provider: &str);

    /// Reset all circuits.
    fn reset_all(&self);

    /// Get the failure count for a provider (for monitoring/reporting).
    fn failure_count(&self, provider: &str) -> u64;

    /// Get all provider circuit states (for monitoring/reporting).
    fn all_states(&self) -> HashMap<String, CircuitState>;
}

// ─── ProviderCircuitState ────────────────────────────────────────────────────

/// Internal tracking state for a single provider circuit.
#[derive(Debug, Clone)]
struct ProviderCircuitState {
    /// Current circuit state.
    state: CircuitState,

    /// Consecutive failure count (in Closed state).
    consecutive_failures: u64,

    /// Consecutive success count (in HalfOpen state).
    consecutive_successes: u64,

    /// Last error message (for logging/debugging).
    last_error: Option<String>,
}

impl Default for ProviderCircuitState {
    fn default() -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            consecutive_successes: 0,
            last_error: None,
        }
    }
}

// ─── CircuitBreakerConfig ────────────────────────────────────────────────────

/// Configuration for the threshold-based circuit breaker.
///
/// The circuit opens after `failure_threshold` consecutive failures
/// and closes after `success_threshold` consecutive successes in HalfOpen state.
/// The circuit stays in Open state for `open_duration_secs` before
/// transitioning to HalfOpen.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before the circuit opens.
    pub failure_threshold: u64,

    /// Number of consecutive successes needed to close the circuit
    /// from HalfOpen state.
    pub success_threshold: u64,

    /// How long the circuit stays in Open state before transitioning
    /// to HalfOpen (in seconds).
    pub open_duration_secs: u64,

    /// Per-provider configuration overrides.
    #[serde(default)]
    pub per_provider_config: HashMap<String, CircuitBreakerConfig>,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 3,
            open_duration_secs: 60,
            per_provider_config: HashMap::new(),
        }
    }
}

impl CircuitBreakerConfig {
    /// Create a config with custom thresholds.
    pub fn new(failure_threshold: u64, success_threshold: u64, open_duration_secs: u64) -> Self {
        Self {
            failure_threshold,
            success_threshold,
            open_duration_secs,
            per_provider_config: HashMap::new(),
        }
    }

    /// Add a per-provider config override.
    pub fn with_provider_config(mut self, provider: impl Into<String>, config: CircuitBreakerConfig) -> Self {
        self.per_provider_config.insert(provider.into(), config);
        self
    }

    /// Get the effective config for a provider (override or global default).
    pub fn effective_config(&self, provider: &str) -> Self {
        self.per_provider_config
            .get(provider)
            .cloned()
            .unwrap_or(Self {
                failure_threshold: self.failure_threshold,
                success_threshold: self.success_threshold,
                open_duration_secs: self.open_duration_secs,
                per_provider_config: HashMap::new(),
            })
    }
}

// ─── ThresholdCircuitBreaker ─────────────────────────────────────────────────

/// Classic threshold-based circuit breaker implementation.
///
/// - **Closed state**: Calls proceed normally. Failures are counted.
///   After `failure_threshold` consecutive failures, the circuit opens.
/// - **Open state**: Calls are rejected. After `open_duration_secs`,
///   the circuit transitions to HalfOpen.
/// - **HalfOpen state**: Limited calls allowed. If `success_threshold`
///   consecutive calls succeed, the circuit closes. If any call fails,
///   the circuit reopens.
///
/// Thread-safe via internal `RwLock`.
pub struct ThresholdCircuitBreaker {
    /// Global configuration (with per-provider overrides).
    config: CircuitBreakerConfig,

    /// Per-provider circuit states.
    states: std::sync::RwLock<HashMap<String, ProviderCircuitState>>,
}

impl ThresholdCircuitBreaker {
    /// Create a circuit breaker with default configuration.
    pub fn new() -> Self {
        Self {
            config: CircuitBreakerConfig::default(),
            states: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Create a circuit breaker with custom configuration.
    pub fn with_config(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            states: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Get or create the circuit state for a provider.
    fn get_or_create_state(&self, provider: &str) -> ProviderCircuitState {
        let states = self.states.read().unwrap();
        states.get(provider).cloned().unwrap_or_default()
    }

    /// Update the circuit state for a provider.
    fn update_state(&self, provider: &str, new_state: ProviderCircuitState) {
        let mut states = self.states.write().unwrap();
        states.insert(provider.to_string(), new_state);
    }

    /// Check if an Open circuit should transition to HalfOpen.
    fn check_half_open_transition(state: &ProviderCircuitState, config: &CircuitBreakerConfig) -> bool {
        if let CircuitState::Open { since, .. } = &state.state {
            let now = Utc::now();
            let elapsed = now.signed_duration_since(*since);
            elapsed.num_seconds() >= config.open_duration_secs as i64
        } else {
            false
        }
    }
}

impl Default for ThresholdCircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CircuitBreaker for ThresholdCircuitBreaker {
    fn check(&self, provider: &str) -> CircuitState {
        let config = self.config.effective_config(provider);
        let mut state = self.get_or_create_state(provider);

        // Check Open → HalfOpen transition
        if Self::check_half_open_transition(&state, &config) {
            state.state = CircuitState::HalfOpen { since: Utc::now() };
            state.consecutive_successes = 0;
            self.update_state(provider, state.clone());
        }

        state.state.clone()
    }

    fn record_success(&self, provider: &str) {
        let config = self.config.effective_config(provider);
        let mut state = self.get_or_create_state(provider);

        match &state.state {
            CircuitState::Closed => {
                // Reset failure counter on success
                state.consecutive_failures = 0;
            }
            CircuitState::HalfOpen { .. } => {
                state.consecutive_successes += 1;
                // If we've had enough consecutive successes, close the circuit
                if state.consecutive_successes >= config.success_threshold {
                    state.state = CircuitState::Closed;
                    state.consecutive_failures = 0;
                    state.consecutive_successes = 0;
                    state.last_error = None;
                }
            }
            CircuitState::Open { .. } => {
                // Success recorded while Open? This shouldn't happen normally,
                // but handle it gracefully — reset to Closed
                state.state = CircuitState::Closed;
                state.consecutive_failures = 0;
                state.last_error = None;
            }
        }

        self.update_state(provider, state);
    }

    fn record_failure(&self, provider: &str, error: &str) {
        let config = self.config.effective_config(provider);
        let mut state = self.get_or_create_state(provider);

        state.last_error = Some(error.to_string());

        match &state.state {
            CircuitState::Closed => {
                state.consecutive_failures += 1;
                // If we've reached the failure threshold, open the circuit
                if state.consecutive_failures >= config.failure_threshold {
                    state.state = CircuitState::Open {
                        since: Utc::now(),
                        failure_count: state.consecutive_failures,
                    };
                }
            }
            CircuitState::HalfOpen { .. } => {
                // Failure in HalfOpen — reopen the circuit
                state.state = CircuitState::Open {
                    since: Utc::now(),
                    failure_count: state.consecutive_failures + 1,
                };
                state.consecutive_successes = 0;
            }
            CircuitState::Open { .. } => {
                // Failure while Open — just update the failure count and error
                state.consecutive_failures += 1;
            }
        }

        self.update_state(provider, state);
    }

    fn reset(&self, provider: &str) {
        let mut states = self.states.write().unwrap();
        states.remove(provider);
    }

    fn reset_all(&self) {
        let mut states = self.states.write().unwrap();
        states.clear();
    }

    fn failure_count(&self, provider: &str) -> u64 {
        let state = self.get_or_create_state(provider);
        state.consecutive_failures
    }

    fn all_states(&self) -> HashMap<String, CircuitState> {
        let states = self.states.read().unwrap();
        states.iter().map(|(k, v)| (k.clone(), v.state.clone())).collect()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_circuit_state_default() {
        let state = CircuitState::default();
        assert_eq!(state, CircuitState::Closed);
        assert!(state.allows_calls());
        assert!(state.is_healthy());
    }

    #[test]
    fn test_circuit_state_open() {
        let state = CircuitState::Open {
            since: Utc::now(),
            failure_count: 5,
        };
        assert!(!state.allows_calls());
        assert!(!state.is_healthy());
        assert!(state.is_failing());
    }

    #[test]
    fn test_circuit_state_half_open() {
        let state = CircuitState::HalfOpen { since: Utc::now() };
        assert!(state.allows_calls());
        assert!(!state.is_healthy());
        assert!(!state.is_failing());
    }

    #[test]
    fn test_circuit_breaker_config_default() {
        let config = CircuitBreakerConfig::default();
        assert_eq!(config.failure_threshold, 5);
        assert_eq!(config.success_threshold, 3);
        assert_eq!(config.open_duration_secs, 60);
    }

    #[test]
    fn test_circuit_breaker_config_with_provider_override() {
        let config = CircuitBreakerConfig::default()
            .with_provider_config("anthropic", CircuitBreakerConfig::new(3, 2, 30));

        let effective = config.effective_config("anthropic");
        assert_eq!(effective.failure_threshold, 3);
        assert_eq!(effective.open_duration_secs, 30);

        let default = config.effective_config("openai");
        assert_eq!(default.failure_threshold, 5);
    }

    #[test]
    fn test_threshold_circuit_breaker_closed_to_open() {
        let breaker = ThresholdCircuitBreaker::new();

        // Initially closed
        assert_eq!(breaker.check("openai"), CircuitState::Closed);

        // Record failures until threshold (5)
        for i in 0..4 {
            breaker.record_failure("openai", &format!("error {}", i));
            assert_eq!(breaker.check("openai"), CircuitState::Closed);
        }

        // 5th failure opens the circuit
        breaker.record_failure("openai", "final error");
        let state = breaker.check("openai");
        assert!(state.is_failing());
        assert_eq!(breaker.failure_count("openai"), 5);
    }

    #[test]
    fn test_threshold_circuit_breaker_success_resets_failures() {
        let breaker = ThresholdCircuitBreaker::new();

        // Record 3 failures (not enough to open)
        for i in 0..3 {
            breaker.record_failure("openai", &format!("error {}", i));
        }
        assert_eq!(breaker.failure_count("openai"), 3);

        // Success resets failure count
        breaker.record_success("openai");
        assert_eq!(breaker.failure_count("openai"), 0);
        assert!(breaker.check("openai").is_healthy());
    }

    #[test]
    fn test_threshold_circuit_breaker_half_open_to_closed() {
        let config = CircuitBreakerConfig::new(3, 2, 1); // 1s open duration for testing
        let breaker = ThresholdCircuitBreaker::with_config(config);

        // Open the circuit with 3 failures
        for i in 0..3 {
            breaker.record_failure("test", &format!("error {}", i));
        }
        assert!(breaker.check("test").is_failing());

        // Wait for open duration (1 second)
        thread::sleep(Duration::from_secs(2));

        // Should transition to HalfOpen
        let state = breaker.check("test");
        assert!(state.allows_calls()); // HalfOpen allows calls

        // 2 successes should close the circuit
        breaker.record_success("test");
        breaker.record_success("test");
        assert!(breaker.check("test").is_healthy());
    }

    #[test]
    fn test_threshold_circuit_breaker_half_open_failure_reopens() {
        let config = CircuitBreakerConfig::new(3, 2, 1);
        let breaker = ThresholdCircuitBreaker::with_config(config);

        // Open the circuit
        for i in 0..3 {
            breaker.record_failure("test", &format!("error {}", i));
        }

        // Wait for open duration
        thread::sleep(Duration::from_secs(2));

        // Should be HalfOpen
        let state = breaker.check("test");
        assert!(state.allows_calls());

        // Failure in HalfOpen reopens
        breaker.record_failure("test", "still failing");
        assert!(breaker.check("test").is_failing());
    }

    #[test]
    fn test_threshold_circuit_breaker_reset() {
        let breaker = ThresholdCircuitBreaker::new();

        for i in 0..5 {
            breaker.record_failure("openai", &format!("error {}", i));
        }
        assert!(breaker.check("openai").is_failing());

        // Reset specific provider
        breaker.reset("openai");
        assert!(breaker.check("openai").is_healthy());

        // Reset all
        breaker.reset_all();
        assert!(breaker.check("anthropic").is_healthy());
    }

    #[test]
    fn test_threshold_circuit_breaker_all_states() {
        let breaker = ThresholdCircuitBreaker::new();

        // No providers tracked initially
        assert!(breaker.all_states().is_empty());

        // Add some providers
        breaker.record_failure("openai", "error");
        breaker.record_success("anthropic");

        let all = breaker.all_states();
        assert_eq!(all.len(), 2);
        assert!(all.get("openai").unwrap().is_healthy()); // Only 1 failure, still closed
        assert!(all.get("anthropic").unwrap().is_healthy());
    }
}
