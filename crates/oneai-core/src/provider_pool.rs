//! Provider pool — multi-provider fallback orchestration.
//!
//! When a primary LLM provider fails (network errors, API errors, timeouts,
//! rate limits, circuit breaker opens), the provider pool automatically
//! falls over to alternative providers without manual intervention.
//!
//! This creates the closed loop identified as the P5-3 gap:
//! CircuitBreaker detects failure → ProviderPool activates fallback →
//! inference succeeds on alternate provider → CircuitBreaker records success →
//! primary provider eventually recovers.
//!
//! Key concepts:
//! - `ProviderPoolConfig`: Configuration for the fallback chain
//! - `ProviderEntryConfig`: Single provider in the chain (name, config, priority, cooldown)
//! - `FallbackEvent`: Audit trail entry for observability
//! - `FallbackReason`: Why a fallback occurred
//! - `FallbackModelDegradation`: Within-provider model downgrade rules (Opus → Sonnet → Haiku)
//! - `ProviderPoolStatus`: Health monitoring snapshot
//!
//! The actual `ProviderPool` implementation (which implements `LlmProvider`)
//! lives in `oneai-provider/src/provider_pool.rs` because it depends on
//! concrete provider implementations.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ModelConfig;

// ─── ProviderEntryConfig ──────────────────────────────────────────────────────

/// Configuration for a single provider entry in the fallback chain.
///
/// Each entry specifies a provider's ModelConfig, a human-readable name
/// (for circuit breaker / rate limiter / cost tracking), a priority
/// (lower = primary, higher = fallback), and a cooldown period after
/// failure before this provider is retried.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderEntryConfig {
    /// Provider name for circuit breaker / rate limiter / cost tracking.
    /// Examples: "anthropic", "openai", "ollama", "gemini"
    pub name: String,

    /// ModelConfig for creating this provider.
    pub model_config: ModelConfig,

    /// Priority (0 = primary, higher = lower priority).
    /// Entries are tried in ascending priority order.
    pub priority: u32,

    /// Cooldown after failure before retrying this provider (seconds).
    /// Default: 30 seconds. Set to 0 for immediate retry (use with caution).
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
}

fn default_cooldown_secs() -> u64 { 30 }

impl ProviderEntryConfig {
    /// Create a new provider entry config.
    pub fn new(name: impl Into<String>, model_config: ModelConfig, priority: u32) -> Self {
        Self {
            name: name.into(),
            model_config,
            priority,
            cooldown_secs: 30,
        }
    }

    /// Create with custom cooldown.
    pub fn with_cooldown(mut self, secs: u64) -> Self {
        self.cooldown_secs = secs;
        self
    }

    /// Get the model name from the config.
    pub fn model_name(&self) -> &str {
        self.model_config.model_name.as_deref().unwrap_or("unknown")
    }
}

// ─── ProviderPoolConfig ────────────────────────────────────────────────────────

/// Configuration for the provider pool (fallback chain).
///
/// Defines an ordered list of provider entries (primary first),
/// maximum fallback attempts, and whether to degrade models
/// within the same provider family before cross-provider fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderPoolConfig {
    /// Ordered list of provider entries (primary first).
    /// Entries are tried in ascending priority order.
    pub entries: Vec<ProviderEntryConfig>,

    /// Maximum number of fallback attempts per inference call.
    /// Default: 3 (try primary, then 2 fallbacks).
    #[serde(default = "default_max_fallbacks")]
    pub max_fallbacks: usize,

    /// Whether to try degraded models on fallback (Opus → Sonnet → Haiku)
    /// within the same provider family before cross-provider fallback.
    #[serde(default)]
    pub degrade_on_fallback: bool,

    /// Model degradation rules — defines downgrade chains per provider family.
    /// When `degrade_on_fallback` is true, these rules are applied before
    /// cross-provider fallback.
    #[serde(default)]
    pub degradation_rules: Vec<DegradationRule>,
}

fn default_max_fallbacks() -> usize { 3 }

impl Default for ProviderPoolConfig {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            max_fallbacks: 3,
            degrade_on_fallback: false,
            degradation_rules: Vec::new(),
        }
    }
}

impl ProviderPoolConfig {
    /// Create a new config with the given entries.
    pub fn new(entries: Vec<ProviderEntryConfig>) -> Self {
        Self {
            entries,
            max_fallbacks: 3,
            degrade_on_fallback: false,
            degradation_rules: Vec::new(),
        }
    }

    /// Create with custom max fallbacks.
    pub fn with_max_fallbacks(mut self, max: usize) -> Self {
        self.max_fallbacks = max;
        self
    }

    /// Enable model degradation within provider families.
    pub fn with_degradation(mut self, rules: Vec<DegradationRule>) -> Self {
        self.degrade_on_fallback = true;
        self.degradation_rules = rules;
        self
    }

    /// Enable model degradation with default preset rules.
    pub fn with_default_degradation(mut self) -> Self {
        self.degrade_on_fallback = true;
        self.degradation_rules = DegradationRule::default_presets();
        self
    }

    /// Add a provider entry to the chain.
    pub fn add_entry(mut self, entry: ProviderEntryConfig) -> Self {
        self.entries.push(entry);
        self
    }

    /// Anthropic-primary preset pool: Anthropic Sonnet → OpenAI gpt-4o → Ollama qwen2.5.
    pub fn anthropic_primary(
        anthropic_key: Option<String>,
        openai_key: Option<String>,
    ) -> Self {
        let mut entries = Vec::new();

        // Primary: Anthropic Sonnet
        if anthropic_key.is_some() {
            entries.push(ProviderEntryConfig::new(
                "anthropic",
                ModelConfig::anthropic(anthropic_key.unwrap(), "claude-sonnet-4-6-20250514".to_string()),
                0,
            ));
        }

        // Secondary: OpenAI gpt-4o
        if openai_key.is_some() {
            entries.push(ProviderEntryConfig::new(
                "openai",
                ModelConfig::openai(openai_key.unwrap(), "gpt-4o".to_string()),
                1,
            ));
        }

        // Tertiary: Ollama (always available if local)
        entries.push(ProviderEntryConfig::new(
            "ollama",
            ModelConfig::ollama("qwen2.5:7b".to_string()),
            2,
        ).with_cooldown(5));

        Self::new(entries).with_default_degradation()
    }

    /// OpenAI-primary preset pool: OpenAI gpt-4o → Anthropic Sonnet → Ollama qwen2.5.
    pub fn openai_primary(
        openai_key: Option<String>,
        anthropic_key: Option<String>,
    ) -> Self {
        let mut entries = Vec::new();

        if openai_key.is_some() {
            entries.push(ProviderEntryConfig::new(
                "openai",
                ModelConfig::openai(openai_key.unwrap(), "gpt-4o".to_string()),
                0,
            ));
        }

        if anthropic_key.is_some() {
            entries.push(ProviderEntryConfig::new(
                "anthropic",
                ModelConfig::anthropic(anthropic_key.unwrap(), "claude-sonnet-4-6-20250514".to_string()),
                1,
            ));
        }

        entries.push(ProviderEntryConfig::new(
            "ollama",
            ModelConfig::ollama("qwen2.5:7b".to_string()),
            2,
        ).with_cooldown(5));

        Self::new(entries).with_default_degradation()
    }

    /// Local-first preset pool: Ollama → OpenAI gpt-4o-mini → Anthropic Haiku.
    pub fn local_first(openai_key: Option<String>, anthropic_key: Option<String>) -> Self {
        let mut entries = Vec::new();

        entries.push(ProviderEntryConfig::new(
            "ollama",
            ModelConfig::ollama("qwen2.5:7b".to_string()),
            0,
        ).with_cooldown(5));

        if openai_key.is_some() {
            entries.push(ProviderEntryConfig::new(
                "openai",
                ModelConfig::openai(openai_key.unwrap(), "gpt-4o-mini".to_string()),
                1,
            ));
        }

        if anthropic_key.is_some() {
            entries.push(ProviderEntryConfig::new(
                "anthropic",
                ModelConfig::anthropic(anthropic_key.unwrap(), "claude-haiku-4-5-20251001".to_string()),
                2,
            ));
        }

        Self::new(entries)
    }

    /// Get the number of entries in the pool.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Get entries sorted by priority.
    pub fn sorted_entries(&self) -> Vec<&ProviderEntryConfig> {
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_by_key(|e| e.priority);
        entries
    }
}

// ─── FallbackReason ────────────────────────────────────────────────────────────

/// Why a fallback occurred — audit trail reason.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum FallbackReason {
    /// Circuit breaker is Open for this provider — too many recent failures.
    CircuitOpen,

    /// Rate limit exceeded — too many calls in the current window.
    RateLimitExceeded,

    /// Provider returned an error (API error, network error, etc.).
    ProviderError(String),

    /// Request timed out waiting for the provider.
    Timeout,

    /// Budget exceeded — session cost limit reached.
    BudgetExceeded,

    /// Model degradation — downgraded within same provider family.
    ModelDegradation {
        from_model: String,
        to_model: String,
    },
}

impl FallbackReason {
    /// Whether this fallback was triggered by a provider failure.
    pub fn is_provider_failure(&self) -> bool {
        matches!(self, Self::CircuitOpen | Self::ProviderError(_) | Self::Timeout)
    }

    /// Whether this fallback was triggered by a policy (rate/budget).
    pub fn is_policy_trigger(&self) -> bool {
        matches!(self, Self::RateLimitExceeded | Self::BudgetExceeded)
    }

    /// Human-readable description of the reason.
    pub fn description(&self) -> String {
        match self {
            Self::CircuitOpen => "Circuit breaker open — provider failing".to_string(),
            Self::RateLimitExceeded => "Rate limit exceeded".to_string(),
            Self::ProviderError(e) => format!("Provider error: {}", e),
            Self::Timeout => "Request timed out".to_string(),
            Self::BudgetExceeded => "Budget exceeded".to_string(),
            Self::ModelDegradation { from_model, to_model } =>
                format!("Model degradation: {} → {}", from_model, to_model),
        }
    }
}

// ─── FallbackEvent ──────────────────────────────────────────────────────────────

/// Audit trail entry for a fallback event.
///
/// Every time the pool falls over to an alternative provider,
/// a FallbackEvent is logged. This provides observability into
/// which provider failures occurred, when they happened, and
/// what the fallback target was.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct FallbackEvent {
    /// When the fallback occurred.
    pub timestamp: DateTime<Utc>,

    /// The provider that failed (or was skipped).
    pub from_provider: String,

    /// The provider that was used instead.
    pub to_provider: String,

    /// Why the fallback occurred.
    pub reason: FallbackReason,

    /// The model that was being used before fallback.
    pub model_before: String,

    /// The model that was used after fallback.
    pub model_after: String,

    /// The iteration number (for tracking within a session).
    #[serde(default)]
    pub iteration: u64,
}

impl FallbackEvent {
    /// Create a new fallback event.
    pub fn new(
        from_provider: impl Into<String>,
        to_provider: impl Into<String>,
        reason: FallbackReason,
        model_before: impl Into<String>,
        model_after: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            from_provider: from_provider.into(),
            to_provider: to_provider.into(),
            reason,
            model_before: model_before.into(),
            model_after: model_after.into(),
            iteration: 0,
        }
    }

    /// Create with iteration number.
    pub fn with_iteration(mut self, iteration: u64) -> Self {
        self.iteration = iteration;
        self
    }

    /// Human-readable summary of this event.
    pub fn summary(&self) -> String {
        format!(
            "[{}] {} → {} ({}, model: {} → {})",
            self.timestamp.format("%H:%M:%S"),
            self.from_provider,
            self.to_provider,
            self.reason.description(),
            self.model_before,
            self.model_after,
        )
    }
}

// ─── DegradationRule ────────────────────────────────────────────────────────────

/// Model degradation rule — defines a downgrade chain within a provider family.
///
/// When `degrade_on_fallback` is enabled in ProviderPoolConfig, these rules
/// are applied before cross-provider fallback. This is cheaper (same provider,
/// lower tier model) and often sufficient.
///
/// Example: Anthropic family degradation:
/// - Opus → Sonnet → Haiku (powerful → balanced → cheap)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DegradationRule {
    /// Provider family name (e.g., "anthropic", "openai").
    pub provider_family: String,

    /// Ordered degradation chain — models to try in order.
    /// First entry is the most powerful, last is the cheapest.
    pub chain: Vec<String>,
}

impl DegradationRule {
    /// Create a degradation rule.
    pub fn new(provider_family: impl Into<String>, chain: Vec<String>) -> Self {
        Self {
            provider_family: provider_family.into(),
            chain,
        }
    }

    /// Anthropic degradation: Opus → Sonnet → Haiku.
    pub fn anthropic() -> Self {
        Self::new("anthropic", vec![
            "claude-opus-4-8".to_string(),
            "claude-sonnet-4-6-20250514".to_string(),
            "claude-haiku-4-5-20251001".to_string(),
        ])
    }

    /// OpenAI degradation: o3-pro → gpt-4o → gpt-4o-mini.
    pub fn openai() -> Self {
        Self::new("openai", vec![
            "o3-pro".to_string(),
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
        ])
    }

    /// Gemini degradation: 2.5-pro → 2.5-flash → 2.0-flash.
    pub fn gemini() -> Self {
        Self::new("gemini", vec![
            "gemini-2.5-pro".to_string(),
            "gemini-2.5-flash".to_string(),
            "gemini-2.0-flash".to_string(),
        ])
    }

    /// Default preset rules for all major providers.
    pub fn default_presets() -> Vec<Self> {
        vec![
            Self::anthropic(),
            Self::openai(),
            Self::gemini(),
        ]
    }

    /// Get the next degraded model after the given model in this chain.
    ///
    /// Returns None if the model is already the cheapest in the chain,
    /// or if the model is not found in the chain.
    pub fn next_degraded_model(&self, current_model: &str) -> Option<String> {
        // Find the current model in the chain
        let position = self.chain.iter().position(|m| {
            // Partial match: "claude-opus-4-8" matches "claude-opus-4-8-20250620"
            current_model.starts_with(m) || m.starts_with(current_model)
        });

        if let Some(pos) = position {
            if pos + 1 < self.chain.len() {
                Some(self.chain[pos + 1].clone())
            } else {
                None // Already at cheapest model
            }
        } else {
            None // Model not in chain
        }
    }

    /// Find the degradation rule for a given provider family.
    pub fn find_for_provider<'a>(rules: &'a [DegradationRule], provider: &str) -> Option<&'a DegradationRule> {
        rules.iter().find(|r| r.provider_family == provider)
    }
}

// ─── ProviderPoolStatus ────────────────────────────────────────────────────────

/// Health monitoring snapshot for the provider pool.
///
/// Provides a summary of which providers are available, which are
/// currently active, circuit breaker states, and recent fallback events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderPoolStatus {
    /// The name of the currently active provider.
    pub active_provider: String,

    /// The model of the currently active provider.
    pub active_model: String,

    /// Total number of providers in the pool.
    pub total_providers: usize,

    /// Per-provider health status.
    pub provider_health: HashMap<String, ProviderHealthStatus>,

    /// Number of fallback events in the last 24 hours.
    pub recent_fallback_count: usize,

    /// Last fallback event (if any).
    pub last_fallback: Option<FallbackEvent>,
}

/// Health status for a single provider in the pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderHealthStatus {
    /// Provider name.
    pub name: String,

    /// Model name.
    pub model: String,

    /// Priority in the fallback chain.
    pub priority: u32,

    /// Whether this provider is currently usable (circuit closed, rate OK).
    pub is_available: bool,

    /// Circuit breaker state (if circuit breaker is configured).
    pub circuit_state: Option<String>,

    /// Recent failure count (from circuit breaker).
    pub failure_count: Option<u64>,
}

impl ProviderHealthStatus {
    /// Create a new provider health status.
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        priority: u32,
        is_available: bool,
        circuit_state: Option<String>,
        failure_count: Option<u64>,
    ) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
            priority,
            is_available,
            circuit_state,
            failure_count,
        }
    }
}

impl ProviderPoolStatus {
    /// Create a new pool status.
    pub fn new(
        active_provider: impl Into<String>,
        active_model: impl Into<String>,
        total_providers: usize,
    ) -> Self {
        Self {
            active_provider: active_provider.into(),
            active_model: active_model.into(),
            total_providers,
            provider_health: HashMap::new(),
            recent_fallback_count: 0,
            last_fallback: None,
        }
    }

    /// Whether the pool has at least one healthy provider.
    pub fn has_healthy_provider(&self) -> bool {
        self.provider_health.values().any(|h| h.is_available)
    }

    /// Number of healthy providers.
    pub fn healthy_provider_count(&self) -> usize {
        self.provider_health.values().filter(|h| h.is_available).count()
    }
}

// ─── FallbackLog trait ─────────────────────────────────────────────────────────

/// Trait for logging fallback events — for observability integration.
///
/// The ProviderPool logs every fallback event via this trait.
/// Default implementation is an in-memory log (Vec<FallbackEvent>).
/// Custom implementations can write to OTEL, a database, or a file.
pub trait FallbackLog: Send + Sync {
    /// Log a fallback event.
    fn log_fallback(&self, event: FallbackEvent);

    /// Get recent fallback events (last N events).
    fn recent_events(&self, limit: usize) -> Vec<FallbackEvent>;

    /// Get the total number of logged events.
    fn total_count(&self) -> usize;

    /// Clear all logged events.
    fn clear(&self);
}

// ─── InMemoryFallbackLog ───────────────────────────────────────────────────────

/// In-memory fallback log — stores events in a Vec for simple observability.
pub struct InMemoryFallbackLog {
    events: std::sync::RwLock<Vec<FallbackEvent>>,
}

impl InMemoryFallbackLog {
    /// Create a new in-memory fallback log.
    pub fn new() -> Self {
        Self {
            events: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// Create with capacity pre-allocation.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: std::sync::RwLock::new(Vec::with_capacity(capacity)),
        }
    }
}

impl Default for InMemoryFallbackLog {
    fn default() -> Self {
        Self::new()
    }
}

impl FallbackLog for InMemoryFallbackLog {
    fn log_fallback(&self, event: FallbackEvent) {
        let mut events = self.events.write().unwrap();
        events.push(event);
        // Keep only last 1000 events to avoid unbounded growth
        if events.len() > 1000 {
            let drain_count = events.len() - 1000;
            events.drain(0..drain_count);
        }
    }

    fn recent_events(&self, limit: usize) -> Vec<FallbackEvent> {
        let events = self.events.read().unwrap();
        events.iter().rev().take(limit).cloned().collect()
    }

    fn total_count(&self) -> usize {
        self.events.read().unwrap().len()
    }

    fn clear(&self) {
        self.events.write().unwrap().clear();
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_config(key: &str) -> ModelConfig {
        ModelConfig::anthropic(key.to_string(), "claude-sonnet-4-6-20250514".to_string())
    }

    fn openai_config(key: &str) -> ModelConfig {
        ModelConfig::openai(key.to_string(), "gpt-4o".to_string())
    }

    fn ollama_config() -> ModelConfig {
        ModelConfig::ollama("qwen2.5:7b".to_string())
    }

    // ─── ProviderEntryConfig tests ─────────────────────────────────────────────

    #[test]
    fn test_entry_config_creation() {
        let entry = ProviderEntryConfig::new("anthropic", anthropic_config("key"), 0);
        assert_eq!(entry.name, "anthropic");
        assert_eq!(entry.priority, 0);
        assert_eq!(entry.cooldown_secs, 30);
        assert_eq!(entry.model_name(), "claude-sonnet-4-6-20250514");
    }

    #[test]
    fn test_entry_config_with_cooldown() {
        let entry = ProviderEntryConfig::new("ollama", ollama_config(), 2)
            .with_cooldown(5);
        assert_eq!(entry.cooldown_secs, 5);
    }

    // ─── ProviderPoolConfig tests ───────────────────────────────────────────────

    #[test]
    fn test_pool_config_default() {
        let config = ProviderPoolConfig::default();
        assert!(config.entries.is_empty());
        assert_eq!(config.max_fallbacks, 3);
        assert!(!config.degrade_on_fallback);
        assert!(config.degradation_rules.is_empty());
    }

    #[test]
    fn test_pool_config_new() {
        let entries = vec![
            ProviderEntryConfig::new("anthropic", anthropic_config("key"), 0),
            ProviderEntryConfig::new("openai", openai_config("key"), 1),
        ];
        let config = ProviderPoolConfig::new(entries);
        assert_eq!(config.entry_count(), 2);
        assert_eq!(config.max_fallbacks, 3);
    }

    #[test]
    fn test_pool_config_add_entry() {
        let config = ProviderPoolConfig::default()
            .add_entry(ProviderEntryConfig::new("anthropic", anthropic_config("key"), 0))
            .add_entry(ProviderEntryConfig::new("openai", openai_config("key"), 1));
        assert_eq!(config.entry_count(), 2);
    }

    #[test]
    fn test_pool_config_with_max_fallbacks() {
        let config = ProviderPoolConfig::default().with_max_fallbacks(5);
        assert_eq!(config.max_fallbacks, 5);
    }

    #[test]
    fn test_pool_config_with_default_degradation() {
        let config = ProviderPoolConfig::default().with_default_degradation();
        assert!(config.degrade_on_fallback);
        assert_eq!(config.degradation_rules.len(), 3); // anthropic + openai + gemini
    }

    #[test]
    fn test_pool_config_sorted_entries() {
        let config = ProviderPoolConfig::default()
            .add_entry(ProviderEntryConfig::new("openai", openai_config("key"), 1))
            .add_entry(ProviderEntryConfig::new("anthropic", anthropic_config("key"), 0))
            .add_entry(ProviderEntryConfig::new("ollama", ollama_config(), 2));
        let sorted = config.sorted_entries();
        assert_eq!(sorted[0].name, "anthropic");
        assert_eq!(sorted[1].name, "openai");
        assert_eq!(sorted[2].name, "ollama");
    }

    #[test]
    fn test_pool_config_anthropic_primary() {
        let config = ProviderPoolConfig::anthropic_primary(
            Some("sk-ant-test".to_string()),
            Some("sk-test".to_string()),
        );
        assert_eq!(config.entry_count(), 3); // anthropic + openai + ollama
        assert!(config.degrade_on_fallback);
        let sorted = config.sorted_entries();
        assert_eq!(sorted[0].name, "anthropic");
        assert_eq!(sorted[1].name, "openai");
        assert_eq!(sorted[2].name, "ollama");
    }

    #[test]
    fn test_pool_config_openai_primary() {
        let config = ProviderPoolConfig::openai_primary(
            Some("sk-test".to_string()),
            Some("sk-ant-test".to_string()),
        );
        assert_eq!(config.entry_count(), 3);
        let sorted = config.sorted_entries();
        assert_eq!(sorted[0].name, "openai");
        assert_eq!(sorted[1].name, "anthropic");
        assert_eq!(sorted[2].name, "ollama");
    }

    #[test]
    fn test_pool_config_local_first() {
        let config = ProviderPoolConfig::local_first(
            Some("sk-test".to_string()),
            Some("sk-ant-test".to_string()),
        );
        assert_eq!(config.entry_count(), 3);
        let sorted = config.sorted_entries();
        assert_eq!(sorted[0].name, "ollama");
    }

    // ─── FallbackReason tests ──────────────────────────────────────────────────

    #[test]
    fn test_fallback_reason_variants() {
        let circuit = FallbackReason::CircuitOpen;
        assert!(circuit.is_provider_failure());
        assert!(!circuit.is_policy_trigger());

        let rate = FallbackReason::RateLimitExceeded;
        assert!(!rate.is_provider_failure());
        assert!(rate.is_policy_trigger());

        let error = FallbackReason::ProviderError("timeout".to_string());
        assert!(error.is_provider_failure());

        let budget = FallbackReason::BudgetExceeded;
        assert!(budget.is_policy_trigger());

        let degradation = FallbackReason::ModelDegradation {
            from_model: "opus".to_string(),
            to_model: "sonnet".to_string(),
        };
        assert!(!degradation.is_provider_failure());
        assert!(!degradation.is_policy_trigger());
    }

    #[test]
    fn test_fallback_reason_description() {
        assert_eq!(
            FallbackReason::CircuitOpen.description(),
            "Circuit breaker open — provider failing"
        );
        assert_eq!(
            FallbackReason::ProviderError("502".to_string()).description(),
            "Provider error: 502"
        );
        assert_eq!(
            FallbackReason::ModelDegradation {
                from_model: "opus".to_string(),
                to_model: "sonnet".to_string(),
            }.description(),
            "Model degradation: opus → sonnet"
        );
    }

    // ─── FallbackEvent tests ────────────────────────────────────────────────────

    #[test]
    fn test_fallback_event_creation() {
        let event = FallbackEvent::new(
            "anthropic",
            "openai",
            FallbackReason::CircuitOpen,
            "claude-sonnet",
            "gpt-4o",
        );
        assert_eq!(event.from_provider, "anthropic");
        assert_eq!(event.to_provider, "openai");
        assert_eq!(event.reason, FallbackReason::CircuitOpen);
        assert_eq!(event.model_before, "claude-sonnet");
        assert_eq!(event.model_after, "gpt-4o");
        assert_eq!(event.iteration, 0);
    }

    #[test]
    fn test_fallback_event_with_iteration() {
        let event = FallbackEvent::new(
            "anthropic", "openai",
            FallbackReason::ProviderError("503".to_string()),
            "claude-opus", "gpt-4o",
        ).with_iteration(5);
        assert_eq!(event.iteration, 5);
    }

    #[test]
    fn test_fallback_event_summary() {
        let event = FallbackEvent::new(
            "anthropic", "openai",
            FallbackReason::CircuitOpen,
            "claude-sonnet", "gpt-4o",
        );
        let summary = event.summary();
        assert!(summary.contains("anthropic → openai"));
        assert!(summary.contains("Circuit breaker open"));
        assert!(summary.contains("claude-sonnet → gpt-4o"));
    }

    // ─── DegradationRule tests ──────────────────────────────────────────────────

    #[test]
    fn test_degradation_rule_anthropic() {
        let rule = DegradationRule::anthropic();
        assert_eq!(rule.provider_family, "anthropic");
        assert_eq!(rule.chain.len(), 3);
        assert_eq!(rule.chain[0], "claude-opus-4-8");
        assert_eq!(rule.chain[1], "claude-sonnet-4-6-20250514");
        assert_eq!(rule.chain[2], "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_degradation_rule_openai() {
        let rule = DegradationRule::openai();
        assert_eq!(rule.provider_family, "openai");
        assert_eq!(rule.chain.len(), 3);
        assert_eq!(rule.chain[0], "o3-pro");
    }

    #[test]
    fn test_degradation_rule_next_model() {
        let rule = DegradationRule::anthropic();

        // Opus → Sonnet
        let next = rule.next_degraded_model("claude-opus-4-8");
        assert_eq!(next, Some("claude-sonnet-4-6-20250514".to_string()));

        // Sonnet → Haiku
        let next = rule.next_degraded_model("claude-sonnet-4-6-20250514");
        assert_eq!(next, Some("claude-haiku-4-5-20251001".to_string()));

        // Haiku → None (already cheapest)
        let next = rule.next_degraded_model("claude-haiku-4-5-20251001");
        assert_eq!(next, None);
    }

    #[test]
    fn test_degradation_rule_partial_match() {
        let rule = DegradationRule::anthropic();

        // "claude-opus-4-8-20250620" should match "claude-opus-4-8" via partial match
        let next = rule.next_degraded_model("claude-opus-4-8-20250620");
        assert_eq!(next, Some("claude-sonnet-4-6-20250514".to_string()));
    }

    #[test]
    fn test_degradation_rule_unknown_model() {
        let rule = DegradationRule::anthropic();
        let next = rule.next_degraded_model("gpt-4o");
        assert_eq!(next, None);
    }

    #[test]
    fn test_degradation_rule_find_for_provider() {
        let presets = DegradationRule::default_presets();

        let anthropic = DegradationRule::find_for_provider(&presets, "anthropic");
        assert!(anthropic.is_some());

        let openai = DegradationRule::find_for_provider(&presets, "openai");
        assert!(openai.is_some());

        let unknown = DegradationRule::find_for_provider(&presets, "unknown_provider");
        assert!(unknown.is_none());
    }

    // ─── ProviderPoolStatus tests ───────────────────────────────────────────────

    #[test]
    fn test_pool_status_creation() {
        let status = ProviderPoolStatus::new("anthropic", "claude-sonnet", 3);
        assert_eq!(status.active_provider, "anthropic");
        assert_eq!(status.active_model, "claude-sonnet");
        assert_eq!(status.total_providers, 3);
    }

    #[test]
    fn test_pool_status_healthy_provider() {
        let mut status = ProviderPoolStatus::new("anthropic", "claude-sonnet", 3);
        status.provider_health.insert("anthropic".to_string(), ProviderHealthStatus::new(
            "anthropic", "claude-sonnet", 0, true, Some("closed".to_string()), Some(0),
        ));
        status.provider_health.insert("openai".to_string(), ProviderHealthStatus::new(
            "openai", "gpt-4o", 1, false, Some("open".to_string()), Some(5),
        ));
        assert!(status.has_healthy_provider());
        assert_eq!(status.healthy_provider_count(), 1);
    }

    // ─── InMemoryFallbackLog tests ──────────────────────────────────────────────

    #[test]
    fn test_in_memory_fallback_log() {
        let log = InMemoryFallbackLog::new();

        let event1 = FallbackEvent::new(
            "anthropic", "openai",
            FallbackReason::CircuitOpen,
            "claude-sonnet", "gpt-4o",
        );
        let event2 = FallbackEvent::new(
            "openai", "ollama",
            FallbackReason::ProviderError("timeout".to_string()),
            "gpt-4o", "qwen2.5",
        );

        log.log_fallback(event1);
        log.log_fallback(event2);

        assert_eq!(log.total_count(), 2);

        let recent = log.recent_events(1);
        assert_eq!(recent.len(), 1);
        // Most recent first (reverse order)
        assert_eq!(recent[0].from_provider, "openai");
    }

    #[test]
    fn test_in_memory_fallback_log_clear() {
        let log = InMemoryFallbackLog::new();
        log.log_fallback(FallbackEvent::new(
            "anthropic", "openai",
            FallbackReason::CircuitOpen,
            "claude-sonnet", "gpt-4o",
        ));
        assert_eq!(log.total_count(), 1);
        log.clear();
        assert_eq!(log.total_count(), 0);
    }

    #[test]
    fn test_in_memory_fallback_log_cap() {
        let log = InMemoryFallbackLog::new();
        // Log more than 1000 events — should cap at 1000
        for i in 0..1100 {
            log.log_fallback(FallbackEvent::new(
                format!("p{}", i), format!("p{}", i + 1),
                FallbackReason::ProviderError(format!("err {}", i)),
                format!("m{}", i), format!("m{}", i + 1),
            ));
        }
        assert_eq!(log.total_count(), 1000);
    }
}
