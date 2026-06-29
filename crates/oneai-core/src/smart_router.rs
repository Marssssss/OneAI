//! Smart Model Router — production-grade latency/quality routing.
//!
//! The current `ModelRouter` (regex-based) is purely keyword-driven: it matches
//! task description patterns against routing rules to select a model tier. This
//! works for simple cases but fails in production scenarios where:
//!
//! 1. Latency matters — "user needs fast response, prefer Gemini Flash"
//! 2. Provider health matters — "Anthropic circuit is open, skip to OpenAI"
//! 3. Rate limits matter — "Anthropic rate limit hit, use Ollama instead"
//! 4. Context windows matter — "conversation is 180K tokens, use 200K model"
//!
//! The `SmartRouter` upgrades from regex-only routing to **multi-factor routing**:
//! - Latency scoring (from estimated response times per model)
//! - Quality scoring (from model tier: Cheap/Balanced/Powerful)
//! - Health scoring (from CircuitBreaker state)
//! - Rate scoring (from RateLimiter status)
//!
//! (USD cost scoring and budget-aware routing were removed — OneAI no longer
//! tracks dollar amounts. Termination is governed by `TokenBudget`.)
//!
//! The routing algorithm:
//! 1. Evaluate regex rules (existing ModelRouter, backward compatible)
//! 2. Validate regex result against runtime constraints (circuit, rate, context)
//! 3. If regex result fails validation, or strategy overrides regex:
//!    - Score all available providers on latency/quality dimensions
//!    - Weight scores by the configured strategy (LatencyOptimized/etc.)
//!    - Pick the highest-scoring available provider
//! 4. Return `SmartRouteDecision` with full rationale
//!
//! The actual `SmartRouter` implementation lives in `oneai-provider/src/smart_router.rs`
//! because it depends on concrete provider implementations and the ModelRouter.
//!
//! Key concepts:
//! - `RoutingStrategy`: Which dimension to prioritize (latency, quality, balanced)
//! - `SmartRouteConfig`: Configuration for the smart router (strategy, weights, thresholds)
//! - `SmartRouteDecision`: Full routing decision with rationale and factor analysis
//! - `SmartRouteFactor`: What factors influenced the routing decision
//! - `RoutingTier`: Capability tier classification (Cheap, Balanced, Powerful)
//! - `ModelQualityProfile`: Per-model quality/latency characteristics
//! - `SmartRoutingLog`: Audit trail for routing decisions (like FallbackLog)


use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};


// ─── RoutingStrategy ──────────────────────────────────────────────────────────

/// Which dimension to prioritize when routing model selection.
///
/// Each strategy defines different weight ratios for the two scoring dimensions:
/// latency and quality. The weights determine which providers/models
/// are preferred under that strategy.
///
/// | Strategy | Latency Wt | Quality Wt |
/// |----------|------------|------------|
/// | LatencyOptimized | 0.80 | 0.20 |
/// | QualityOptimized | 0.20 | 0.80 |
/// | Balanced | 0.40 | 0.60 |
/// | Custom | user-defined | user-defined |
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RoutingStrategy {
    /// Minimize latency — prefer faster models (Haiku, Gemini Flash, gpt-4o-mini).
    /// Weight: Latency=0.80, Quality=0.20
    LatencyOptimized,

    /// Maximize quality — prefer powerful models (Opus, o3-pro, Gemini Pro).
    /// Weight: Latency=0.20, Quality=0.80
    QualityOptimized,

    /// Balance both dimensions — moderate latency and quality.
    /// Weight: Latency=0.40, Quality=0.60
    Balanced,

    /// Custom weights — user-defined weight ratios.
    Custom {
        latency_weight: f64,
        quality_weight: f64,
    },
}

impl RoutingStrategy {
    /// Get the weight ratios for this strategy.
    ///
    /// Returns (latency_weight, quality_weight).
    /// Weights always sum to 1.0.
    pub fn weights(&self) -> (f64, f64) {
        match self {
            Self::LatencyOptimized => (0.80, 0.20),
            Self::QualityOptimized => (0.20, 0.80),
            Self::Balanced => (0.40, 0.60),
            Self::Custom { latency_weight, quality_weight } =>
                (*latency_weight, *quality_weight),
        }
    }

    /// Human-readable name of this strategy.
    pub fn name(&self) -> &str {
        match self {
            Self::LatencyOptimized => "Latency Optimized",
            Self::QualityOptimized => "Quality Optimized",
            Self::Balanced => "Balanced",
            Self::Custom { .. } => "Custom",
        }
    }

    /// Create a custom strategy with validated weights.
    ///
    /// Weights must be in [0.0, 1.0] and must sum to approximately 1.0.
    pub fn custom(latency_weight: f64, quality_weight: f64) -> Self {
        let total = latency_weight + quality_weight;
        assert!(total > 0.99 && total < 1.01,
            "Custom strategy weights must sum to 1.0 (got {})", total);
        Self::Custom { latency_weight, quality_weight }
    }
}

impl Default for RoutingStrategy {
    fn default() -> Self {
        Self::Balanced
    }
}

// ─── SmartRouteConfig ──────────────────────────────────────────────────────────

/// Configuration for the smart model router.
///
/// Defines the routing strategy, dimension weights, and various constraints
/// that the router considers when making routing decisions.
///
/// The smart router evaluates multiple factors (latency, quality,
/// circuit breaker, rate limiter, context window) and produces
/// a weighted scoring that determines which provider/model to use.
///
/// **Usage**:
/// ```ignore
/// let config = SmartRouteConfig::balanced();
/// // or:
/// let config = SmartRouteConfig::latency_optimized();
/// // or:
/// let config = SmartRouteConfig::custom(RoutingStrategy::custom(0.5, 0.5));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SmartRouteConfig {
    /// The routing strategy — which dimension to prioritize.
    #[serde(default)]
    pub strategy: RoutingStrategy,

    /// Whether to consider provider health (circuit breaker) when routing.
    /// When true, the router skips providers with open circuits.
    #[serde(default = "default_true")]
    pub health_aware: bool,

    /// Whether to consider rate limits when routing.
    /// When true, the router skips providers that are rate-limited.
    #[serde(default = "default_true")]
    pub rate_aware: bool,

    /// Whether to consider context window limits when routing.
    /// When true, the router skips models whose context window is
    /// smaller than the current conversation token count.
    #[serde(default = "default_true")]
    pub context_aware: bool,

    /// Whether to use regex rules as a first-pass before multi-factor scoring.
    /// When true, the existing ModelRouter regex rules are evaluated first.
    /// If a regex rule matches and passes validation, its result is used.
    /// If no regex rule matches or validation fails, multi-factor scoring is used.
    #[serde(default = "default_true")]
    pub regex_first_pass: bool,

    /// Estimated maximum latency tolerance in milliseconds.
    /// Models with estimated latency above this threshold get a latency score of 0.
    /// Default: 30000ms (30 seconds).
    #[serde(default = "default_max_latency_ms")]
    pub max_latency_ms: u64,

    /// Conversation token count threshold for context overflow.
    /// If the conversation token count exceeds this percentage of a model's
    /// context window, the model is skipped.
    /// Default: 0.8 (80% — leave 20% headroom for new tokens).
    #[serde(default = "default_context_threshold")]
    pub context_overflow_threshold: f64,
}

fn default_true() -> bool { true }
fn default_max_latency_ms() -> u64 { 30000 }
fn default_context_threshold() -> f64 { 0.8 }

impl Default for SmartRouteConfig {
    fn default() -> Self {
        Self::balanced()
    }
}

impl SmartRouteConfig {
    /// Create a balanced config (default) — considers all factors equally.
    pub fn balanced() -> Self {
        Self {
            strategy: RoutingStrategy::Balanced,
            health_aware: true,
            rate_aware: true,
            context_aware: true,
            regex_first_pass: true,
            max_latency_ms: 30000,
            context_overflow_threshold: 0.8,
        }
    }

    /// Create a latency-optimized config — minimizes latency above all else.
    pub fn latency_optimized() -> Self {
        Self {
            strategy: RoutingStrategy::LatencyOptimized,
            health_aware: true,
            rate_aware: true,
            context_aware: true,
            regex_first_pass: true,
            max_latency_ms: 10000, // Only accept fast models
            context_overflow_threshold: 0.8,
        }
    }

    /// Create a quality-optimized config — maximizes quality above all else.
    pub fn quality_optimized() -> Self {
        Self {
            strategy: RoutingStrategy::QualityOptimized,
            health_aware: true,
            rate_aware: true,
            context_aware: true,
            regex_first_pass: true,
            max_latency_ms: 60000, // Accept slower responses for higher quality
            context_overflow_threshold: 0.8,
        }
    }

    /// Create a custom config with a specific strategy.
    pub fn with_strategy(strategy: RoutingStrategy) -> Self {
        Self {
            strategy,
            health_aware: true,
            rate_aware: true,
            context_aware: true,
            regex_first_pass: true,
            max_latency_ms: 30000,
            context_overflow_threshold: 0.8,
        }
    }

    /// Create a minimal config — only uses regex rules, no smart factors.
    ///
    /// This effectively disables the smart router and falls back to pure
    /// ModelRouter behavior. Useful for backward compatibility testing.
    pub fn regex_only() -> Self {
        Self {
            strategy: RoutingStrategy::Balanced,
            health_aware: false,
            rate_aware: false,
            context_aware: false,
            regex_first_pass: true,
            max_latency_ms: u64::MAX,
            context_overflow_threshold: 1.0,
        }
    }

    /// Set custom max latency tolerance.
    pub fn with_max_latency(mut self, ms: u64) -> Self {
        self.max_latency_ms = ms;
        self
    }

    /// Set custom context overflow threshold.
    pub fn with_context_threshold(mut self, threshold: f64) -> Self {
        self.context_overflow_threshold = threshold;
        self
    }

    /// Disable health awareness.
    pub fn without_health_awareness(mut self) -> Self {
        self.health_aware = false;
        self
    }

    /// Disable rate awareness.
    pub fn without_rate_awareness(mut self) -> Self {
        self.rate_aware = false;
        self
    }

    /// Disable context awareness.
    pub fn without_context_awareness(mut self) -> Self {
        self.context_aware = false;
        self
    }
}

// ─── RoutingTier ───────────────────────────────────────────────────────────────

/// Capability tier classification for models.
///
/// Models are classified into three tiers based on their capability/size:
/// - **Cheap**: small, fast — for simple/explore tasks (Haiku, gpt-4o-mini, Gemini Flash)
/// - **Balanced**: mid-range — for implement/debug tasks (Sonnet, gpt-4o, Gemini 2.5-flash)
/// - **Powerful**: high-capability — for architect/research tasks (Opus, o3-pro, Gemini Pro)
///
/// The tier determines the quality score in multi-factor routing:
/// Cheap=0.3, Balanced=0.7, Powerful=1.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RoutingTier {
    /// Small/fast model — for simple tasks and exploration.
    Cheap,
    /// Balanced model — for implementation and debugging.
    Balanced,
    /// Powerful model — for architecture, planning, and research.
    Powerful,
}

impl RoutingTier {
    /// Quality score for this tier (used in multi-factor routing).
    ///
    /// Cheap=0.3, Balanced=0.7, Powerful=1.0.
    pub fn quality_score(&self) -> f64 {
        match self {
            Self::Cheap => 0.3,
            Self::Balanced => 0.7,
            Self::Powerful => 1.0,
        }
    }

    /// Infer tier from a model name.
    ///
    /// Uses naming conventions to classify models:
    /// - "haiku", "mini", "flash" → Cheap
    /// - "sonnet", "4o", "2.5-flash" → Balanced
    /// - "opus", "o3-pro", "2.5-pro" → Powerful
    pub fn from_model_name(model: &str) -> Self {
        let lower = model.to_lowercase();
        if lower.contains("haiku") || lower.contains("mini") || lower.contains("0.5b") || lower.contains("nano") {
            Self::Cheap
        } else if lower.contains("opus") || lower.contains("o3-pro") || lower.contains("2.5-pro") || lower.contains("deepseek-r1:14b") {
            Self::Powerful
        } else {
            // Default to balanced for sonnet, 4o, 7b, etc.
            Self::Balanced
        }
    }

    /// Human-readable name of this tier.
    pub fn name(&self) -> &str {
        match self {
            Self::Cheap => "Cheap",
            Self::Balanced => "Balanced",
            Self::Powerful => "Powerful",
        }
    }
}

// ─── SmartRouteFactor ──────────────────────────────────────────────────────────

/// Factors that influenced a smart routing decision.
///
/// Each routing decision may be influenced by multiple factors.
/// The `SmartRouteDecision.factors` field lists which factors were
/// considered and how they affected the outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum SmartRouteFactor {
    /// Circuit breaker state influenced the decision.
    /// Includes the provider name and whether the circuit was open.
    CircuitOpen {
        provider: String,
        was_open: bool,
    },

    /// Rate limiter status influenced the decision.
    /// Includes the provider name and whether the rate was exceeded.
    RateLimited {
        provider: String,
        was_exceeded: bool,
    },

    /// Context window overflow influenced the decision.
    /// Includes the conversation token count and the model's context limit.
    ContextOverflow {
        conversation_tokens: u64,
        model_context_window: u64,
        would_overflow: bool,
    },

    /// Quality requirement influenced the decision.
    /// Includes the required quality tier and the actual model tier.
    QualityRequirement {
        required_tier: RoutingTier,
        actual_tier: RoutingTier,
    },

    /// Latency requirement influenced the decision.
    /// Includes the max latency tolerance and the estimated latency.
    LatencyRequirement {
        max_latency_ms: u64,
        estimated_latency_ms: u64,
        within_tolerance: bool,
    },

    /// User override — a specific provider/model was explicitly requested.
    UserOverride {
        requested_provider: String,
        requested_model: String,
    },

    /// Regex rule match influenced the decision.
    RegexMatch {
        rule_description: String,
        matched: bool,
    },

    /// Multi-factor scoring result.
    ScoringResult {
        provider: String,
        total_score: f64,
        latency_score: f64,
        quality_score: f64,
    },
}

impl SmartRouteFactor {
    /// Human-readable description of this factor.
    pub fn description(&self) -> String {
        match self {
            Self::CircuitOpen { provider, was_open } =>
                format!("Circuit {}: {}", provider, if *was_open { "OPEN — skip" } else { "closed — OK" }),
            Self::RateLimited { provider, was_exceeded } =>
                format!("Rate {}: {}", provider, if *was_exceeded { "EXCEEDED — skip" } else { "OK" }),
            Self::ContextOverflow { conversation_tokens, model_context_window, would_overflow } =>
                format!("Context: {}K/{}K tokens ({})", conversation_tokens / 1000, model_context_window / 1000,
                    if *would_overflow { "OVERFLOW — skip" } else { "OK" }),
            Self::QualityRequirement { required_tier, actual_tier } =>
                format!("Quality: required={}, actual={}", required_tier.name(), actual_tier.name()),
            Self::LatencyRequirement { max_latency_ms, estimated_latency_ms, within_tolerance } =>
                format!("Latency: est {}ms / max {}ms ({})", estimated_latency_ms, max_latency_ms,
                    if *within_tolerance { "OK" } else { "OVER LIMIT" }),
            Self::UserOverride { requested_provider, requested_model } =>
                format!("User override: {} / {}", requested_provider, requested_model),
            Self::RegexMatch { rule_description, matched } =>
                format!("Regex rule: {} ({})", rule_description, if *matched { "matched" } else { "no match" }),
            Self::ScoringResult { provider, total_score, latency_score, quality_score } =>
                format!("Score: {} = {:.2} (latency={:.2}, quality={:.2})",
                    provider, total_score, latency_score, quality_score),
        }
    }
}

// ─── ProviderScore ──────────────────────────────────────────────────────────────

/// Score result for a single provider in multi-factor routing.
///
/// Each provider is scored on two dimensions (latency, quality),
/// then the scores are weighted by the routing strategy to produce a total score.
/// The provider with the highest total score wins the routing decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderScore {
    /// Provider name.
    pub provider_name: String,

    /// Model name.
    pub model_name: String,

    /// Routing tier (Cheap/Balanced/Powerful).
    pub tier: RoutingTier,

    /// Latency dimension score (0.0 to 1.0).
    /// Higher = faster. 0.0 = exceeds latency tolerance.
    pub latency_score: f64,

    /// Quality dimension score (0.0 to 1.0).
    /// Based on tier: Cheap=0.3, Balanced=0.7, Powerful=1.0.
    pub quality_score: f64,

    /// Total weighted score (0.0 to 1.0).
    /// `latency_score * latency_weight + quality_score * quality_weight`.
    pub total_score: f64,

    /// Whether this provider is available (not circuit-open, not rate-limited, not context-overflow).
    pub is_available: bool,

    /// Reason this provider was skipped (if `is_available = false`).
    pub skip_reason: Option<String>,
}

impl ProviderScore {
    /// Create a new provider score.
    pub fn new(
        provider_name: impl Into<String>,
        model_name: impl Into<String>,
        tier: RoutingTier,
        latency_score: f64,
        quality_score: f64,
        strategy: &RoutingStrategy,
    ) -> Self {
        let (lw, qw) = strategy.weights();
        let total_score = latency_score * lw + quality_score * qw;

        Self {
            provider_name: provider_name.into(),
            model_name: model_name.into(),
            tier,
            latency_score,
            quality_score,
            total_score,
            is_available: true,
            skip_reason: None,
        }
    }

    /// Create a skipped provider score (not available).
    pub fn skipped(
        provider_name: impl Into<String>,
        model_name: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            provider_name: provider_name.into(),
            model_name: model_name.into(),
            tier: RoutingTier::Balanced,
            latency_score: 0.0,
            quality_score: 0.0,
            total_score: 0.0,
            is_available: false,
            skip_reason: Some(reason.into()),
        }
    }

    /// Human-readable summary of this score.
    pub fn summary(&self) -> String {
        if !self.is_available {
            format!("{} — SKIPPED: {}", self.provider_name, self.skip_reason.as_deref().unwrap_or("unknown"))
        } else {
            format!("{} ({}) — total={:.2} [latency={:.2}, quality={:.2}]",
                self.provider_name, self.tier.name(), self.total_score, self.latency_score, self.quality_score)
        }
    }
}

// ─── ModelQualityProfile ──────────────────────────────────────────────────────

/// Per-model quality/latency characteristics for routing scoring.
///
/// Derived from `ModelCapability` (context window) and tier classification.
/// Estimated latency values are based on typical observed response times for
/// each model tier — these are approximate and should be calibrated with
/// real-world measurements.
///
/// | Tier | Est. Latency (ms) | Context Window | Quality Score |
/// |------|-------------------|----------------|---------------|
/// | Cheap (Haiku/mini/flash) | 500-2000 | 128K-200K | 0.3 |
/// | Balanced (Sonnet/4o) | 2000-5000 | 128K-200K | 0.7 |
/// | Powerful (Opus/o3-pro) | 5000-30000 | 128K-200K | 1.0 |
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelQualityProfile {
    /// Model name (e.g., "claude-opus-4-8", "gpt-4o").
    pub model_name: String,

    /// Provider family (e.g., "anthropic", "openai").
    pub provider_family: String,

    /// Routing tier classification.
    pub tier: RoutingTier,

    /// Estimated average response latency in milliseconds.
    /// These are approximate values based on typical observed response times.
    pub estimated_latency_ms: u64,

    /// Maximum context window size in tokens.
    pub context_window_tokens: u64,

    /// Maximum output tokens.
    pub max_output_tokens: u32,

    /// Quality score (from tier: Cheap=0.3, Balanced=0.7, Powerful=1.0).
    pub quality_score: f64,
}

impl ModelQualityProfile {
    /// Create a profile from known model characteristics.
    ///
    /// The `estimated_latency_ms` and `context_window_tokens` values are
    /// approximate defaults based on model tier classification.
    pub fn new(
        model_name: impl Into<String>,
        provider_family: impl Into<String>,
        tier: RoutingTier,
    ) -> Self {
        let model_str = model_name.into();
        let quality_score = tier.quality_score();

        // Default latency estimates based on tier
        let estimated_latency_ms = match tier {
            RoutingTier::Cheap => 1500,   // ~1.5s average
            RoutingTier::Balanced => 3500, // ~3.5s average
            RoutingTier::Powerful => 15000, // ~15s average
        };

        // Default context window based on model name patterns
        let context_window_tokens = Self::infer_context_window(&model_str);
        let max_output_tokens = Self::infer_max_output(&model_str);

        Self {
            model_name: model_str,
            provider_family: provider_family.into(),
            tier,
            estimated_latency_ms,
            context_window_tokens,
            max_output_tokens,
            quality_score,
        }
    }

    /// Infer context window size from model name.
    fn infer_context_window(model: &str) -> u64 {
        let lower = model.to_lowercase();
        if lower.contains("opus") || lower.contains("sonnet") || lower.contains("gpt-4") || lower.contains("o3") {
            200_000
        } else if lower.contains("haiku") || lower.contains("mini") || lower.contains("nano") {
            128_000
        } else if lower.contains("gemini") {
            1_000_000 // Gemini has very large context windows
        } else if lower.contains("qwen") || lower.contains("llama") || lower.contains("ollama") {
            32_000 // Local models typically have smaller context windows
        } else {
            128_000 // Default
        }
    }

    /// Infer max output tokens from model name.
    fn infer_max_output(model: &str) -> u32 {
        let lower = model.to_lowercase();
        if lower.contains("opus") || lower.contains("o3-pro") {
            16_384
        } else if lower.contains("sonnet") || lower.contains("gpt-4o") || lower.contains("gemini") {
            8_192
        } else {
            4_096 // Default
        }
    }

    /// Build default profiles for all known models.
    ///
    /// These profiles provide baseline quality/latency data for
    /// the smart router. Custom profiles can be added via `add_profile()`.
    pub fn default_profiles() -> Vec<Self> {
        vec![
            // Anthropic family
            Self::new("claude-haiku-4-5-20251001", "anthropic", RoutingTier::Cheap),
            Self::new("claude-sonnet-4-6-20250514", "anthropic", RoutingTier::Balanced),
            Self::new("claude-opus-4-8", "anthropic", RoutingTier::Powerful),
            // OpenAI family
            Self::new("gpt-4o-mini", "openai", RoutingTier::Cheap),
            Self::new("gpt-4o", "openai", RoutingTier::Balanced),
            Self::new("o3-pro", "openai", RoutingTier::Powerful),
            // Gemini family
            Self::new("gemini-2.0-flash", "google", RoutingTier::Cheap),
            Self::new("gemini-2.5-flash", "google", RoutingTier::Balanced),
            Self::new("gemini-2.5-pro", "google", RoutingTier::Powerful),
            // Ollama family
            Self::new("qwen2.5:0.5b", "ollama", RoutingTier::Cheap),
            Self::new("qwen2.5:7b", "ollama", RoutingTier::Balanced),
            Self::new("deepseek-r1:14b", "ollama", RoutingTier::Powerful),
        ]
    }
}

// ─── SmartRouteDecision ────────────────────────────────────────────────────────

/// Full routing decision from the smart router — includes rationale and factor analysis.
///
/// Unlike the simple `RouteDecision` (which only has model, provider, reason),
/// `SmartRouteDecision` includes detailed scoring data, latency estimates,
/// and which factors were considered in making the decision.
///
/// This provides full observability into routing decisions — useful for
/// debugging and performance tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SmartRouteDecision {
    /// The selected model name.
    pub model: String,

    /// The selected provider kind.
    pub provider: String,

    /// Optional max_tokens override for this route.
    pub max_tokens: Option<u32>,

    /// Routing tier of the selected model.
    pub tier: RoutingTier,

    /// The routing strategy used.
    pub strategy: RoutingStrategy,

    /// Estimated latency for this inference call (milliseconds).
    pub estimated_latency_ms: u64,

    /// Quality score of the selected model (0.0 to 1.0).
    pub quality_score: f64,

    /// Total weighted score of the selected provider.
    pub total_score: f64,

    /// Factors that influenced this decision.
    pub factors: Vec<SmartRouteFactor>,

    /// Primary reason for this routing decision (human-readable).
    pub reason: String,

    /// Whether this decision came from a regex rule match.
    pub from_regex: bool,

    /// All provider scores evaluated (for debugging/observability).
    pub all_scores: Vec<ProviderScore>,

    /// Timestamp of this decision.
    pub timestamp: DateTime<Utc>,
}

impl SmartRouteDecision {
    /// Create a new smart route decision.
    pub fn new(
        model: impl Into<String>,
        provider: impl Into<String>,
        tier: RoutingTier,
        strategy: RoutingStrategy,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            provider: provider.into(),
            max_tokens: None,
            tier,
            strategy,
            estimated_latency_ms: 0,
            quality_score: tier.quality_score(),
            total_score: 0.0,
            factors: Vec::new(),
            reason: reason.into(),
            from_regex: false,
            all_scores: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    /// Create from a regex rule match (backward compat with ModelRouter).
    pub fn from_regex_match(
        model: impl Into<String>,
        provider: impl Into<String>,
        tier: RoutingTier,
        rule_description: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            provider: provider.into(),
            max_tokens: None,
            tier,
            strategy: RoutingStrategy::Balanced, // Regex doesn't know strategy
            estimated_latency_ms: 0,
            quality_score: tier.quality_score(),
            total_score: 0.0,
            factors: vec![SmartRouteFactor::RegexMatch {
                rule_description: rule_description.into(),
                matched: true,
            }],
            reason: "Regex rule match".to_string(),
            from_regex: true,
            all_scores: Vec::new(),
            timestamp: Utc::now(),
        }
    }

    /// Create from multi-factor scoring.
    pub fn from_scoring(
        best_score: &ProviderScore,
        strategy: RoutingStrategy,
        factors: Vec<SmartRouteFactor>,
        all_scores: Vec<ProviderScore>,
    ) -> Self {
        Self {
            model: best_score.model_name.clone(),
            provider: best_score.provider_name.clone(),
            max_tokens: None,
            tier: best_score.tier,
            strategy,
            estimated_latency_ms: 0,
            quality_score: best_score.quality_score,
            total_score: best_score.total_score,
            factors,
            reason: format!("Multi-factor scoring: {} (score={:.2})",
                best_score.provider_name, best_score.total_score),
            from_regex: false,
            all_scores,
            timestamp: Utc::now(),
        }
    }

    /// Set estimated latency.
    pub fn with_estimated_latency(mut self, latency_ms: u64) -> Self {
        self.estimated_latency_ms = latency_ms;
        self
    }

    /// Set max_tokens override.
    pub fn with_max_tokens(mut self, tokens: u32) -> Self {
        self.max_tokens = Some(tokens);
        self
    }

    /// Add a factor to the decision.
    pub fn with_factor(mut self, factor: SmartRouteFactor) -> Self {
        self.factors.push(factor);
        self
    }

    /// Human-readable summary of this decision.
    pub fn summary(&self) -> String {
        let source = if self.from_regex { "regex" } else { "scoring" };
        format!(
            "[{}] {} → {} (tier={}, strategy={}, latency≈{}ms, score={:.2})",
            source,
            self.provider,
            self.model,
            self.tier.name(),
            self.strategy.name(),
            self.estimated_latency_ms,
            self.total_score,
        )
    }

    /// Detailed factor analysis.
    pub fn factor_analysis(&self) -> String {
        self.factors.iter()
            .map(|f| f.description())
            .collect::<Vec<_>>()
            .join("\n  ")
    }
}

// ─── SmartRoutingLog trait ────────────────────────────────────────────────────

/// Trait for logging smart routing decisions — for observability integration.
///
/// Every routing decision is logged via this trait. This provides observability
/// into which factors influenced routing, what scores were computed, and why
/// a particular provider was selected.
///
/// Follows the same pattern as `FallbackLog` in `oneai-core/src/provider_pool.rs`.
pub trait SmartRoutingLog: Send + Sync {
    /// Log a smart routing decision.
    fn log_decision(&self, decision: SmartRouteDecision);

    /// Get recent routing decisions (last N decisions).
    fn recent_decisions(&self, limit: usize) -> Vec<SmartRouteDecision>;

    /// Get the total number of logged decisions.
    fn total_count(&self) -> usize;

    /// Clear all logged decisions.
    fn clear(&self);
}

// ─── InMemorySmartRoutingLog ──────────────────────────────────────────────────

/// In-memory smart routing log — stores decisions in a Vec for observability.
pub struct InMemorySmartRoutingLog {
    decisions: std::sync::RwLock<Vec<SmartRouteDecision>>,
}

impl InMemorySmartRoutingLog {
    /// Create a new in-memory routing log.
    pub fn new() -> Self {
        Self {
            decisions: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// Create with capacity pre-allocation.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            decisions: std::sync::RwLock::new(Vec::with_capacity(capacity)),
        }
    }
}

impl Default for InMemorySmartRoutingLog {
    fn default() -> Self {
        Self::new()
    }
}

impl SmartRoutingLog for InMemorySmartRoutingLog {
    fn log_decision(&self, decision: SmartRouteDecision) {
        let mut decisions = self.decisions.write().unwrap();
        decisions.push(decision);
        // Keep only last 1000 decisions to avoid unbounded growth
        if decisions.len() > 1000 {
            let drain_count = decisions.len() - 1000;
            decisions.drain(0..drain_count);
        }
    }

    fn recent_decisions(&self, limit: usize) -> Vec<SmartRouteDecision> {
        let decisions = self.decisions.read().unwrap();
        decisions.iter().rev().take(limit).cloned().collect()
    }

    fn total_count(&self) -> usize {
        self.decisions.read().unwrap().len()
    }

    fn clear(&self) {
        self.decisions.write().unwrap().clear();
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── RoutingStrategy tests ──────────────────────────────────────────────

    #[test]
    fn test_routing_strategy_weights() {
        assert_eq!(RoutingStrategy::LatencyOptimized.weights(), (0.80, 0.20));
        assert_eq!(RoutingStrategy::QualityOptimized.weights(), (0.20, 0.80));
        assert_eq!(RoutingStrategy::Balanced.weights(), (0.40, 0.60));
    }

    #[test]
    fn test_routing_strategy_custom() {
        let custom = RoutingStrategy::custom(0.5, 0.5);
        assert_eq!(custom.weights(), (0.5, 0.5));
    }

    #[test]
    fn test_routing_strategy_names() {
        assert_eq!(RoutingStrategy::LatencyOptimized.name(), "Latency Optimized");
        assert_eq!(RoutingStrategy::QualityOptimized.name(), "Quality Optimized");
        assert_eq!(RoutingStrategy::Balanced.name(), "Balanced");
        assert_eq!(RoutingStrategy::Custom { latency_weight: 0.5, quality_weight: 0.5 }.name(), "Custom");
    }

    #[test]
    fn test_routing_strategy_default() {
        assert_eq!(RoutingStrategy::default(), RoutingStrategy::Balanced);
    }

    // ─── RoutingTier tests ────────────────────────────────────────────────

    #[test]
    fn test_routing_tier_quality_scores() {
        assert_eq!(RoutingTier::Cheap.quality_score(), 0.3);
        assert_eq!(RoutingTier::Balanced.quality_score(), 0.7);
        assert_eq!(RoutingTier::Powerful.quality_score(), 1.0);
    }

    #[test]
    fn test_routing_tier_from_model_name() {
        assert_eq!(RoutingTier::from_model_name("claude-haiku-4-5-20251001"), RoutingTier::Cheap);
        assert_eq!(RoutingTier::from_model_name("gpt-4o-mini"), RoutingTier::Cheap);
        assert_eq!(RoutingTier::from_model_name("gemini-2.0-flash"), RoutingTier::Cheap);
        assert_eq!(RoutingTier::from_model_name("claude-sonnet-4-6-20250514"), RoutingTier::Balanced);
        assert_eq!(RoutingTier::from_model_name("gpt-4o"), RoutingTier::Balanced);
        assert_eq!(RoutingTier::from_model_name("claude-opus-4-8"), RoutingTier::Powerful);
        assert_eq!(RoutingTier::from_model_name("o3-pro"), RoutingTier::Powerful);
        assert_eq!(RoutingTier::from_model_name("qwen2.5:7b"), RoutingTier::Balanced);
    }

    // ─── SmartRouteConfig tests ────────────────────────────────────────────

    #[test]
    fn test_smart_route_config_balanced() {
        let config = SmartRouteConfig::balanced();
        assert_eq!(config.strategy, RoutingStrategy::Balanced);
        assert!(config.health_aware);
        assert!(config.rate_aware);
        assert!(config.context_aware);
        assert!(config.regex_first_pass);
        assert_eq!(config.max_latency_ms, 30000);
        assert_eq!(config.context_overflow_threshold, 0.8);
    }

    #[test]
    fn test_smart_route_config_latency_optimized() {
        let config = SmartRouteConfig::latency_optimized();
        assert_eq!(config.strategy, RoutingStrategy::LatencyOptimized);
        assert_eq!(config.max_latency_ms, 10000);
    }

    #[test]
    fn test_smart_route_config_regex_only() {
        let config = SmartRouteConfig::regex_only();
        assert!(!config.health_aware);
        assert!(!config.rate_aware);
        assert!(!config.context_aware);
        assert!(config.regex_first_pass);
    }

    #[test]
    fn test_smart_route_config_custom_modifiers() {
        let config = SmartRouteConfig::balanced()
            .with_max_latency(5000)
            .with_context_threshold(0.9);
        assert_eq!(config.max_latency_ms, 5000);
        assert_eq!(config.context_overflow_threshold, 0.9);
    }

    // ─── SmartRouteFactor tests ──────────────────────────────────────────

    #[test]
    fn test_smart_route_factor_descriptions() {
        let circuit = SmartRouteFactor::CircuitOpen { provider: "anthropic".to_string(), was_open: true };
        assert!(circuit.description().contains("OPEN"));

        let scoring = SmartRouteFactor::ScoringResult {
            provider: "anthropic".to_string(),
            total_score: 0.85,
            latency_score: 0.2,
            quality_score: 0.8,
        };
        assert!(scoring.description().contains("0.85"));
    }

    // ─── ProviderScore tests ──────────────────────────────────────────────

    #[test]
    fn test_provider_score_creation() {
        let score = ProviderScore::new(
            "anthropic", "claude-sonnet-4-6-20250514",
            RoutingTier::Balanced,
            0.6, 0.7,
            &RoutingStrategy::Balanced,
        );
        // Total = 0.6*0.4 + 0.7*0.6 = 0.24 + 0.42 = 0.66
        assert!((score.total_score - 0.66).abs() < 0.01);
        assert!(score.is_available);
        assert!(score.skip_reason.is_none());
    }

    #[test]
    fn test_provider_score_skipped() {
        let score = ProviderScore::skipped("anthropic", "claude-opus", "Circuit breaker open");
        assert!(!score.is_available);
        assert_eq!(score.skip_reason.as_deref(), Some("Circuit breaker open"));
        assert_eq!(score.total_score, 0.0);
    }

    // ─── ModelQualityProfile tests ──────────────────────────────────────

    #[test]
    fn test_model_quality_profile_creation() {
        let profile = ModelQualityProfile::new(
            "claude-sonnet-4-6-20250514", "anthropic",
            RoutingTier::Balanced,
        );
        assert_eq!(profile.model_name, "claude-sonnet-4-6-20250514");
        assert_eq!(profile.tier, RoutingTier::Balanced);
        assert_eq!(profile.quality_score, 0.7);
        assert_eq!(profile.estimated_latency_ms, 3500);
        assert_eq!(profile.context_window_tokens, 200_000);
    }

    #[test]
    fn test_model_quality_profile_default_profiles() {
        let profiles = ModelQualityProfile::default_profiles();
        assert!(profiles.len() >= 12); // At least 4 providers × 3 tiers

        // Check Anthropic profiles exist
        let haiku = profiles.iter().find(|p| p.model_name.contains("haiku"));
        assert!(haiku.is_some());
        assert_eq!(haiku.unwrap().tier, RoutingTier::Cheap);

        let opus = profiles.iter().find(|p| p.model_name.contains("opus"));
        assert!(opus.is_some());
        assert_eq!(opus.unwrap().tier, RoutingTier::Powerful);
    }

    // ─── SmartRouteDecision tests ────────────────────────────────────────

    #[test]
    fn test_smart_route_decision_new() {
        let decision = SmartRouteDecision::new(
            "claude-sonnet-4-6-20250514", "anthropic",
            RoutingTier::Balanced, RoutingStrategy::Balanced,
            "Multi-factor scoring",
        );
        assert_eq!(decision.model, "claude-sonnet-4-6-20250514");
        assert_eq!(decision.provider, "anthropic");
        assert_eq!(decision.tier, RoutingTier::Balanced);
        assert_eq!(decision.strategy, RoutingStrategy::Balanced);
        assert!(!decision.from_regex);
    }

    #[test]
    fn test_smart_route_decision_from_regex() {
        let decision = SmartRouteDecision::from_regex_match(
            "claude-haiku-4-5-20251001", "anthropic",
            RoutingTier::Cheap, "Simple task → cheap model",
        );
        assert!(decision.from_regex);
        assert_eq!(decision.tier, RoutingTier::Cheap);
        assert!(decision.factors.iter().any(|f| matches!(f, SmartRouteFactor::RegexMatch { matched: true, .. })));
    }

    #[test]
    fn test_smart_route_decision_from_scoring() {
        let best = ProviderScore::new(
            "anthropic", "claude-sonnet-4-6-20250514",
            RoutingTier::Balanced, 0.6, 0.7,
            &RoutingStrategy::Balanced,
        );
        let all_scores = vec![best.clone()];

        let decision = SmartRouteDecision::from_scoring(
            &best, RoutingStrategy::Balanced,
            vec![SmartRouteFactor::ScoringResult {
                provider: "anthropic".to_string(),
                total_score: best.total_score,
                latency_score: best.latency_score,
                quality_score: best.quality_score,
            }],
            all_scores,
        );
        assert!(!decision.from_regex);
        assert_eq!(decision.model, "claude-sonnet-4-6-20250514");
        assert!((decision.total_score - 0.66).abs() < 0.01);
    }

    #[test]
    fn test_smart_route_decision_modifiers() {
        let decision = SmartRouteDecision::new(
            "gpt-4o", "openai",
            RoutingTier::Balanced, RoutingStrategy::LatencyOptimized,
            "Latency routing",
        )
            .with_estimated_latency(3500)
            .with_max_tokens(4096)
            .with_factor(SmartRouteFactor::CircuitOpen { provider: "openai".to_string(), was_open: false });

        assert_eq!(decision.estimated_latency_ms, 3500);
        assert_eq!(decision.max_tokens, Some(4096));
        assert_eq!(decision.factors.len(), 1);
    }

    // ─── InMemorySmartRoutingLog tests ──────────────────────────────────────

    #[test]
    fn test_in_memory_smart_routing_log() {
        let log = InMemorySmartRoutingLog::new();

        let d1 = SmartRouteDecision::new("gpt-4o", "openai", RoutingTier::Balanced, RoutingStrategy::Balanced, "test1");
        let d2 = SmartRouteDecision::new("claude-opus-4-8", "anthropic", RoutingTier::Powerful, RoutingStrategy::QualityOptimized, "test2");

        log.log_decision(d1);
        log.log_decision(d2);

        assert_eq!(log.total_count(), 2);

        let recent = log.recent_decisions(1);
        assert_eq!(recent.len(), 1);
        // Most recent first (reverse order)
        assert_eq!(recent[0].model, "claude-opus-4-8");
    }

    #[test]
    fn test_in_memory_smart_routing_log_clear() {
        let log = InMemorySmartRoutingLog::new();
        log.log_decision(SmartRouteDecision::new("gpt-4o", "openai", RoutingTier::Balanced, RoutingStrategy::Balanced, "test"));
        assert_eq!(log.total_count(), 1);
        log.clear();
        assert_eq!(log.total_count(), 0);
    }

    #[test]
    fn test_in_memory_smart_routing_log_cap() {
        let log = InMemorySmartRoutingLog::new();
        for i in 0..1100 {
            log.log_decision(SmartRouteDecision::new(
                format!("model{}", i), "provider",
                RoutingTier::Balanced, RoutingStrategy::Balanced,
                format!("decision {}", i),
            ));
        }
        assert_eq!(log.total_count(), 1000);
    }
}
