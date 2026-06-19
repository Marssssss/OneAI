//! Provider pool — multi-provider fallback orchestration.
//!
//! When a primary LLM provider fails (network errors, API errors, timeouts,
//! rate limits, circuit breaker opens), the provider pool automatically
//! falls over to alternative providers. This creates the closed loop:
//!
//! CircuitBreaker detects failure → ProviderPool activates fallback →
//! inference succeeds on alternate provider → CircuitBreaker records success →
//! primary provider eventually recovers.
//!
//! ProviderPool implements the `LlmProvider` trait, so it can be used as
//! a drop-in replacement for a single provider in AgentLoop. No code changes
//! needed beyond replacing `Arc<dyn LlmProvider>` with `Arc<ProviderPool>`.
//!
//! Usage:
//! ```ignore
//! let pool = ProviderPool::new(
//!     vec![
//!         ProviderEntry::new("anthropic", anthropic_provider, 0),
//!         ProviderEntry::new("openai", openai_provider, 1),
//!         ProviderEntry::new("ollama", ollama_provider, 2),
//!     ],
//!     ProviderPoolConfig::default(),
//! );
//!
//! // Use pool as the provider in AgentLoop — fallback is automatic
//! let agent_loop = AgentLoop::new(
//!     Arc::new(pool) as Arc<dyn LlmProvider>,
//!     tools, parser, ...
//! );
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use std::pin::Pin;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tokio::sync::RwLock;

use oneai_core::{
    ContentBlock, InferenceRequest, InferenceResponse, InferenceStreamChunk,
    Message, ModelCapability, ModelConfig, Role, TokenUsage,
    CircuitBreaker, RateLimiter, CostTracker,
    FallbackEvent, FallbackReason, FallbackLog, InMemoryFallbackLog,
    ProviderPoolConfig, ProviderEntryConfig, DegradationRule,
    ProviderPoolStatus, ProviderHealthStatus,
};
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::LlmProvider;
use crate::ProviderFactory;

// ─── ProviderEntry ─────────────────────────────────────────────────────────────

/// A single provider entry in the fallback pool.
///
/// Wraps an `Arc<dyn LlmProvider>` with metadata for circuit breaker,
/// rate limiter, and cost tracking integration.
pub struct ProviderEntry {
    /// Provider name for circuit breaker / rate limiter / cost tracking.
    name: String,

    /// The LLM provider instance.
    provider: Arc<dyn LlmProvider>,

    /// Priority (0 = primary, higher = fallback).
    priority: u32,

    /// Cooldown after failure before retrying this provider (seconds).
    cooldown_secs: u64,

    /// Last failure timestamp (for cooldown tracking).
    last_failure: Arc<RwLock<Option<chrono::DateTime<chrono::Utc>>>>,
}

impl ProviderEntry {
    /// Create a new provider entry.
    pub fn new(
        name: impl Into<String>,
        provider: Arc<dyn LlmProvider>,
        priority: u32,
    ) -> Self {
        Self {
            name: name.into(),
            provider,
            priority,
            cooldown_secs: 30,
            last_failure: Arc::new(RwLock::new(None)),
        }
    }

    /// Create with custom cooldown.
    pub fn with_cooldown(mut self, secs: u64) -> Self {
        self.cooldown_secs = secs;
        self
    }

    /// Get the provider name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the model name from the provider config.
    pub fn model_name(&self) -> &str {
        self.provider.config().model_name.as_deref().unwrap_or("unknown")
    }

    /// Get the priority.
    pub fn priority(&self) -> u32 {
        self.priority
    }

    /// Whether this provider is in cooldown (recently failed, should be skipped).
    async fn is_in_cooldown(&self) -> bool {
        let last_failure = self.last_failure.read().await;
        if let Some(failure_time) = *last_failure {
            let elapsed = chrono::Utc::now().signed_duration_since(failure_time);
            elapsed.num_seconds() < self.cooldown_secs as i64
        } else {
            false
        }
    }

    /// Record a failure timestamp for cooldown tracking.
    async fn record_failure_time(&self) {
        let mut last_failure = self.last_failure.write().await;
        *last_failure = Some(chrono::Utc::now());
    }

    /// Clear the cooldown (after success).
    async fn clear_cooldown(&self) {
        let mut last_failure = self.last_failure.write().await;
        *last_failure = None;
    }
}

// ─── ProviderPool ──────────────────────────────────────────────────────────────

/// Multi-provider fallback pool — implements `LlmProvider`.
///
/// Holds an ordered list of providers (primary → fallbacks). When the
/// primary provider fails or the circuit breaker opens, the pool
/// automatically tries the next provider in the chain.
///
/// Integrates with CircuitBreaker, RateLimiter, CostTracker, and FallbackLog
/// for full production-grade resilience.
pub struct ProviderPool {
    /// Ordered provider entries (primary first).
    entries: Vec<ProviderEntry>,

    /// Pool configuration (max fallbacks, degradation rules, etc.).
    config: ProviderPoolConfig,

    /// Circuit breaker — skip providers with Open circuits.
    circuit_breaker: Option<Arc<dyn CircuitBreaker>>,

    /// Rate limiter — respect per-provider rate limits.
    rate_limiter: Option<Arc<dyn RateLimiter>>,

    /// Cost tracker — record usage for whichever provider succeeded.
    cost_tracker: Option<Arc<dyn CostTracker>>,

    /// Currently active provider index (Atomically updated on fallback).
    active_index: AtomicU32,

    /// Fallback event log — audit trail for observability.
    fallback_log: Arc<dyn FallbackLog>,
}

impl ProviderPool {
    /// Create a provider pool with the given entries and configuration.
    pub fn new(entries: Vec<ProviderEntry>, config: ProviderPoolConfig) -> Self {
        // Sort entries by priority (primary first)
        let mut sorted = entries;
        sorted.sort_by_key(|e| e.priority);

        Self {
            entries: sorted,
            config,
            circuit_breaker: None,
            rate_limiter: None,
            cost_tracker: None,
            active_index: AtomicU32::new(0),
            fallback_log: Arc::new(InMemoryFallbackLog::new()),
        }
    }

    /// Create a pool with just the configuration (entries built from entry configs).
    pub fn from_config(config: ProviderPoolConfig) -> Self {
        let entries: Vec<ProviderEntry> = config.entries.iter().map(|entry_config| {
            let provider = ProviderFactory::create(entry_config.model_config.clone());
            ProviderEntry::new(
                entry_config.name.clone(),
                Arc::from(provider),
                entry_config.priority,
            ).with_cooldown(entry_config.cooldown_secs)
        }).collect();

        Self::new(entries, config)
    }

    /// Create a minimal pool with a single provider (no fallback).
    pub fn single(provider: Arc<dyn LlmProvider>, name: impl Into<String>) -> Self {
        Self::new(
            vec![ProviderEntry::new(name, provider, 0)],
            ProviderPoolConfig::default(),
        )
    }

    /// Set the circuit breaker for provider health tracking.
    pub fn with_circuit_breaker(mut self, cb: Arc<dyn CircuitBreaker>) -> Self {
        self.circuit_breaker = Some(cb);
        self
    }

    /// Set the rate limiter for provider rate tracking.
    pub fn with_rate_limiter(mut self, rl: Arc<dyn RateLimiter>) -> Self {
        self.rate_limiter = Some(rl);
        self
    }

    /// Set the cost tracker for usage recording.
    pub fn with_cost_tracker(mut self, ct: Arc<dyn CostTracker>) -> Self {
        self.cost_tracker = Some(ct);
        self
    }

    /// Set a custom fallback log (for OTEL / database integration).
    pub fn with_fallback_log(mut self, log: Arc<dyn FallbackLog>) -> Self {
        self.fallback_log = log;
        self
    }

    /// Get the name of the currently active provider.
    pub fn active_provider_name(&self) -> String {
        let idx = self.active_index.load(Ordering::Relaxed) as usize;
        if idx < self.entries.len() {
            self.entries[idx].name.clone()
        } else {
            "unknown".to_string()
        }
    }

    /// Get the model name of the currently active provider.
    pub fn active_model_name(&self) -> String {
        let idx = self.active_index.load(Ordering::Relaxed) as usize;
        if idx < self.entries.len() {
            self.entries[idx].model_name().to_string()
        } else {
            "unknown".to_string()
        }
    }

    /// Get recent fallback events from the log.
    pub fn fallback_log_recent(&self, limit: usize) -> Vec<FallbackEvent> {
        self.fallback_log.recent_events(limit)
    }

    /// Get the total number of logged fallback events.
    pub fn fallback_log_count(&self) -> usize {
        self.fallback_log.total_count()
    }

    /// Get the current pool status (health snapshot).
    pub async fn status(&self) -> ProviderPoolStatus {
        let active_idx = self.active_index.load(Ordering::Relaxed) as usize;
        let active_name = if active_idx < self.entries.len() {
            self.entries[active_idx].name.clone()
        } else {
            "unknown".to_string()
        };
        let active_model = if active_idx < self.entries.len() {
            self.entries[active_idx].model_name().to_string()
        } else {
            "unknown".to_string()
        };

        let mut provider_health = HashMap::new();
        for entry in &self.entries {
            let is_available = !entry.is_in_cooldown().await;

            // Check circuit breaker state if configured
            let circuit_state = if let Some(cb) = &self.circuit_breaker {
                let state = cb.check(&entry.name);
                let state_str = match state {
                    oneai_core::CircuitState::Closed => "closed",
                    oneai_core::CircuitState::Open { .. } => "open",
                    oneai_core::CircuitState::HalfOpen { .. } => "half_open",
                    _ => "unknown",
                };
                Some(state_str.to_string())
            } else {
                None
            };

            let failure_count = if let Some(cb) = &self.circuit_breaker {
                Some(cb.failure_count(&entry.name))
            } else {
                None
            };

            let actually_available = if let Some(cb) = &self.circuit_breaker {
                is_available && cb.check(&entry.name).allows_calls()
            } else {
                is_available
            };

            provider_health.insert(entry.name.clone(), ProviderHealthStatus::new(
                entry.name.clone(),
                entry.model_name(),
                entry.priority,
                actually_available,
                circuit_state,
                failure_count,
            ));
        }

        let recent_fallback_count = self.fallback_log.total_count();
        let last_fallback = self.fallback_log.recent_events(1).first().cloned();

        ProviderPoolStatus::new(active_name, active_model, self.entries.len())
            .into_status_with_health(provider_health, recent_fallback_count, last_fallback)
    }

    /// Try inference with fallback chain.
    ///
    /// Iterates through providers in priority order, skipping providers
    /// that are in cooldown, have open circuits, or are rate-limited.
    /// On success, records success in circuit breaker and cost tracker.
    /// On failure, records failure and logs a FallbackEvent.
    async fn infer_with_fallback(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        let max_attempts = self.config.max_fallbacks.min(self.entries.len());
        let mut attempts = 0;

        for entry in &self.entries {
            if attempts >= max_attempts {
                break;
            }

            // ── Check cooldown ──────────────────────────────────────────────
            if entry.is_in_cooldown().await {
                tracing::debug!("Provider {} is in cooldown, skipping", entry.name);
                continue;
            }

            // ── Check circuit breaker ───────────────────────────────────────
            if let Some(cb) = &self.circuit_breaker {
                let state = cb.check(&entry.name);
                if state.is_failing() {
                    tracing::warn!("Circuit breaker OPEN for {}, skipping", entry.name);
                    self.fallback_log.log_fallback(FallbackEvent::new(
                        entry.name.clone(),
                        "next_provider".to_string(), // Will be updated on actual fallback
                        FallbackReason::CircuitOpen,
                        entry.model_name(),
                        "unknown".to_string(),
                    ));
                    continue;
                }
            }

            // ── Check rate limiter ──────────────────────────────────────────
            if let Some(rl) = &self.rate_limiter {
                let status = rl.check_rate(&entry.name).await?;
                if !status.is_allowed() {
                    tracing::warn!("Rate limit exceeded for {}, skipping", entry.name);
                    self.fallback_log.log_fallback(FallbackEvent::new(
                        entry.name.clone(),
                        "next_provider".to_string(),
                        FallbackReason::RateLimitExceeded,
                        entry.model_name(),
                        "unknown".to_string(),
                    ));
                    continue;
                }
                // Record the call in the rate limiter
                let _ = rl.record_call(&entry.name).await;
            }

            attempts += 1;

            // ── Attempt inference ───────────────────────────────────────────
            tracing::info!("Attempting inference with provider {} (attempt {})", entry.name, attempts);

            let result = entry.provider.infer(request.clone()).await;

            match result {
                Ok(response) => {
                    // Success!
                    tracing::info!("Inference succeeded with provider {}", entry.name);

                    // Update active index
                    self.active_index.store(
                        self.entries.iter().position(|e| e.name == entry.name)
                            .unwrap_or(0) as u32,
                        Ordering::Relaxed,
                    );

                    // Record success in circuit breaker
                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_success(&entry.name);
                    }

                    // Clear cooldown
                    entry.clear_cooldown().await;

                    // Record usage in cost tracker
                    if let Some(ct) = &self.cost_tracker {
                        let cost = if let Some(catalog) = &self.config.degradation_rules.first() {
                            // Use pricing catalog if available (but we don't have it here directly)
                            0.0 // CostTracker handles this with its own pricing
                        } else {
                            0.0
                        };
                        let record = oneai_core::UsageRecord::new(
                            request.conversation.id.clone(),
                            response.model.clone(),
                            entry.name.clone(),
                            response.usage.prompt_tokens,
                            response.usage.completion_tokens,
                            cost,
                        );
                        let _ = ct.record_usage(record).await;
                    }

                    return Ok(response);
                }
                Err(error) => {
                    // Failure — record and try next provider
                    tracing::warn!("Inference failed with provider {}: {}", entry.name, error);

                    // Record failure in circuit breaker
                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_failure(&entry.name, &error.to_string());
                    }

                    // Record failure time for cooldown
                    entry.record_failure_time().await;

                    // Check if there's a next provider for the fallback log
                    let next_idx = self.entries.iter().position(|e| e.priority > entry.priority);
                    let (next_name, next_model) = if let Some(idx) = next_idx {
                        (self.entries[idx].name.clone(), self.entries[idx].model_name().to_string())
                    } else {
                        ("none".to_string(), "none".to_string())
                    };

                    self.fallback_log.log_fallback(FallbackEvent::new(
                        entry.name.clone(),
                        next_name,
                        FallbackReason::ProviderError(error.to_string()),
                        entry.model_name(),
                        next_model,
                    ));
                }
            }
        }

        // All providers exhausted
        tracing::error!("All providers exhausted after {} attempts", attempts);
        Err(OneAIError::Fallback(
            format!("All providers exhausted after {} attempts (pool has {} providers)",
                attempts, self.entries.len())
        ))
    }

    /// Try streaming inference with fallback chain.
    ///
    /// Same logic as infer_with_fallback, but returns a stream.
    /// Fallback happens before the stream is opened — if the stream
    /// starts but errors mid-stream, the error propagates (we don't
    /// retry mid-stream).
    async fn infer_stream_with_fallback(
        &self,
        request: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>> {
        let max_attempts = self.config.max_fallbacks.min(self.entries.len());
        let mut attempts = 0;

        for entry in &self.entries {
            if attempts >= max_attempts {
                break;
            }

            // ── Check cooldown ──────────────────────────────────────────────
            if entry.is_in_cooldown().await {
                continue;
            }

            // ── Check circuit breaker ───────────────────────────────────────
            if let Some(cb) = &self.circuit_breaker {
                let state = cb.check(&entry.name);
                if state.is_failing() {
                    self.fallback_log.log_fallback(FallbackEvent::new(
                        entry.name.clone(),
                        "next_provider".to_string(),
                        FallbackReason::CircuitOpen,
                        entry.model_name(),
                        "unknown".to_string(),
                    ));
                    continue;
                }
            }

            // ── Check rate limiter ──────────────────────────────────────────
            if let Some(rl) = &self.rate_limiter {
                let status = rl.check_rate(&entry.name).await?;
                if !status.is_allowed() {
                    self.fallback_log.log_fallback(FallbackEvent::new(
                        entry.name.clone(),
                        "next_provider".to_string(),
                        FallbackReason::RateLimitExceeded,
                        entry.model_name(),
                        "unknown".to_string(),
                    ));
                    continue;
                }
                let _ = rl.record_call(&entry.name).await;
            }

            attempts += 1;

            // ── Attempt streaming inference ─────────────────────────────────
            tracing::info!("Attempting streaming inference with provider {} (attempt {})", entry.name, attempts);

            let result = entry.provider.infer_stream(request.clone()).await;

            match result {
                Ok(stream) => {
                    // Success — update active index and record success
                    self.active_index.store(
                        self.entries.iter().position(|e| e.name == entry.name)
                            .unwrap_or(0) as u32,
                        Ordering::Relaxed,
                    );

                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_success(&entry.name);
                    }
                    entry.clear_cooldown().await;

                    return Ok(stream);
                }
                Err(error) => {
                    // Failure — record and try next
                    tracing::warn!("Streaming inference failed with provider {}: {}", entry.name, error);

                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_failure(&entry.name, &error.to_string());
                    }
                    entry.record_failure_time().await;

                    let next_idx = self.entries.iter().position(|e| e.priority > entry.priority);
                    let (next_name, next_model) = if let Some(idx) = next_idx {
                        (self.entries[idx].name.clone(), self.entries[idx].model_name().to_string())
                    } else {
                        ("none".to_string(), "none".to_string())
                    };

                    self.fallback_log.log_fallback(FallbackEvent::new(
                        entry.name.clone(),
                        next_name,
                        FallbackReason::ProviderError(error.to_string()),
                        entry.model_name(),
                        next_model,
                    ));
                }
            }
        }

        Err(OneAIError::Fallback(
            format!("All providers exhausted for streaming after {} attempts", attempts)
        ))
    }

    /// Get the number of providers in the pool.
    pub fn provider_count(&self) -> usize {
        self.entries.len()
    }

    /// Get provider names in priority order.
    pub fn provider_names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }
}

// ─── ProviderPoolStatus helper ─────────────────────────────────────────────────

/// Helper trait to build ProviderPoolStatus with health details.
trait StatusBuilder {
    fn into_status_with_health(
        self,
        provider_health: HashMap<String, ProviderHealthStatus>,
        recent_fallback_count: usize,
        last_fallback: Option<FallbackEvent>,
    ) -> ProviderPoolStatus;
}

impl StatusBuilder for ProviderPoolStatus {
    fn into_status_with_health(
        mut self,
        provider_health: HashMap<String, ProviderHealthStatus>,
        recent_fallback_count: usize,
        last_fallback: Option<FallbackEvent>,
    ) -> ProviderPoolStatus {
        self.provider_health = provider_health;
        self.recent_fallback_count = recent_fallback_count;
        self.last_fallback = last_fallback;
        self
    }
}

// ─── LlmProvider implementation ─────────────────────────────────────────────────

#[async_trait]
impl LlmProvider for ProviderPool {
    /// Perform inference with automatic fallback on provider failure.
    async fn infer(&self, req: InferenceRequest) -> Result<InferenceResponse> {
        self.infer_with_fallback(req).await
    }

    /// Perform streaming inference with automatic fallback on provider failure.
    async fn infer_stream(
        &self,
        req: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>> {
        self.infer_stream_with_fallback(req).await
    }

    /// Get capabilities of the currently active provider.
    fn capabilities(&self) -> ModelCapability {
        let idx = self.active_index.load(Ordering::Relaxed) as usize;
        if idx < self.entries.len() {
            self.entries[idx].provider.capabilities()
        } else if !self.entries.is_empty() {
            // Fallback to first entry's capabilities
            self.entries[0].provider.capabilities()
        } else {
            // No providers — return minimal capabilities
            ModelCapability {
                supports_streaming: true,
                supports_tools: true,
                supports_multimodal: false,
                context_window_size: 128000,
                max_output_tokens: 4096,
            }
        }
    }

    /// Get the model config of the currently active provider.
    fn config(&self) -> &ModelConfig {
        if !self.entries.is_empty() {
            let idx = self.active_index.load(Ordering::Relaxed) as usize;
            if idx < self.entries.len() {
                self.entries[idx].provider.config()
            } else {
                self.entries[0].provider.config()
            }
        } else {
            // No providers — this shouldn't happen in practice
            // Return a reference from a leaked Box as a last resort
            // (only used in error paths, never in normal operation)
            static FALLBACK_CONFIG: std::sync::OnceLock<ModelConfig> = std::sync::OnceLock::new();
            FALLBACK_CONFIG.get_or_init(|| ModelConfig {
                provider_type: oneai_core::ProviderType::Cloud,
                cloud_kind: Some(oneai_core::CloudProviderKind::OpenAI),
                api_key: None,
                base_url: None,
                port: None,
                model_name: None,
                model_path: None,
                extra: HashMap::new(),
            })
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::circuit_breaker::{ThresholdCircuitBreaker, CircuitBreakerConfig};
    use oneai_core::rate_limiter::{TokenWindowRateLimiter, RateLimitConfig};

    /// Simple mock provider for pool testing.
    /// Can be configured to succeed or fail deterministically.
    struct TestProvider {
        config: ModelConfig,
        should_fail: std::sync::Mutex<bool>,
        fail_message: String,
        call_count: std::sync::Mutex<usize>,
    }

    impl TestProvider {
        fn new(name: &str, model: &str) -> Self {
            let (provider_type, cloud_kind) = if name == "anthropic" {
                (oneai_core::ProviderType::Cloud, Some(oneai_core::CloudProviderKind::Anthropic))
            } else if name == "openai" {
                (oneai_core::ProviderType::Cloud, Some(oneai_core::CloudProviderKind::OpenAI))
            } else {
                (oneai_core::ProviderType::Local, None)
            };

            Self {
                config: ModelConfig {
                    provider_type,
                    cloud_kind,
                    api_key: Some(format!("mock-key-{}", name)),
                    base_url: None,
                    port: None,
                    model_name: Some(model.to_string()),
                    model_path: None,
                    extra: HashMap::new(),
                },
                should_fail: std::sync::Mutex::new(false),
                fail_message: "Provider error".to_string(),
                call_count: std::sync::Mutex::new(0),
            }
        }

        fn failing(message: &str) -> Self {
            let mut provider = Self::new("mock", "mock-failing-model");
            *provider.should_fail.lock().unwrap() = true;
            provider.fail_message = message.to_string();
            provider
        }

        fn set_failing(&self, fail: bool) {
            *self.should_fail.lock().unwrap() = fail;
        }

        fn call_count(&self) -> usize {
            *self.call_count.lock().unwrap()
        }
    }

    #[async_trait]
    impl LlmProvider for TestProvider {
        async fn infer(&self, _req: InferenceRequest) -> Result<InferenceResponse> {
            *self.call_count.lock().unwrap() += 1;

            if *self.should_fail.lock().unwrap() {
                return Err(OneAIError::Provider(self.fail_message.clone()));
            }

            Ok(InferenceResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: format!("Response from {}", self.config.model_name.as_deref().unwrap_or("unknown")),
                    }],
                    metadata: HashMap::new(),
                },
                usage: TokenUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                },
                model: self.config.model_name.clone().unwrap_or_default(),
                metadata: HashMap::new(),
            })
        }

        async fn infer_stream(
            &self,
            req: InferenceRequest,
        ) -> Result<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>> {
            // For simplicity, just do a regular infer and convert to stream
            let response = self.infer(req).await?;

            let (tx, rx) = tokio::sync::mpsc::channel(10);
            tokio::spawn(async move {
                for block in &response.message.content {
                    tx.send(InferenceStreamChunk {
                        content: vec![block.clone()],
                        is_final: false,
                        usage: None,
                        model: Some(response.model.clone()),
                    }).await.ok();
                }
                tx.send(InferenceStreamChunk {
                    content: vec![],
                    is_final: true,
                    usage: Some(response.usage.clone()),
                    model: Some(response.model.clone()),
                }).await.ok();
            });

            Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }

        fn capabilities(&self) -> ModelCapability {
            ModelCapability::claude_class()
        }

        fn config(&self) -> &ModelConfig {
            &self.config
        }
    }

    fn anthropic_test_provider() -> Arc<dyn LlmProvider> {
        Arc::new(TestProvider::new("anthropic", "claude-sonnet-4-6-20250514"))
    }

    fn openai_test_provider() -> Arc<dyn LlmProvider> {
        Arc::new(TestProvider::new("openai", "gpt-4o"))
    }

    fn ollama_test_provider() -> Arc<dyn LlmProvider> {
        Arc::new(TestProvider::new("ollama", "qwen2.5:7b"))
    }

    fn failing_test_provider(msg: &str) -> Arc<dyn LlmProvider> {
        Arc::new(TestProvider::failing(msg))
    }

    fn test_request() -> InferenceRequest {
        InferenceRequest {
            conversation: oneai_core::Conversation::new(),
            tools: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            constrained_output: None,
            thinking_budget: None,
            metadata: HashMap::new(),
        }
    }

    // ─── Basic pool tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_pool_primary_succeeds_no_fallback() {
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", anthropic_test_provider(), 0),
                ProviderEntry::new("openai", openai_test_provider(), 1),
            ],
            ProviderPoolConfig::default(),
        );

        let response = pool.infer(test_request()).await.unwrap();
        assert_eq!(
            pool.active_provider_name(),
            "anthropic"
        );
        // Response should come from anthropic
        let text = match &response.message.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => "unknown".to_string(),
        };
        assert!(text.contains("claude-sonnet"));
        assert_eq!(pool.fallback_log_count(), 0);
    }

    #[tokio::test]
    async fn test_pool_primary_fails_fallback_to_secondary() {
        let primary = failing_test_provider("Anthropic API error 503");
        let secondary = openai_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default(),
        );

        let response = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "openai");

        let text = match &response.message.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => "unknown".to_string(),
        };
        assert!(text.contains("gpt-4o"));

        // Should have logged a fallback event
        assert_eq!(pool.fallback_log_count(), 1);
        let events = pool.fallback_log_recent(1);
        assert_eq!(events[0].from_provider, "anthropic");
        assert_eq!(events[0].to_provider, "openai");
    }

    #[tokio::test]
    async fn test_pool_all_providers_fail_returns_error() {
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", failing_test_provider("error 1"), 0),
                ProviderEntry::new("openai", failing_test_provider("error 2"), 1),
            ],
            ProviderPoolConfig::default().with_max_fallbacks(3),
        );

        let result = pool.infer(test_request()).await;
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(matches!(error, OneAIError::Fallback(_)));
        assert!(error.to_string().contains("All providers exhausted"));

        // Should have logged 2 fallback events
        assert_eq!(pool.fallback_log_count(), 2);
    }

    #[tokio::test]
    async fn test_pool_three_providers_second_fails() {
        let primary = anthropic_test_provider();
        let secondary = failing_test_provider("OpenAI timeout");
        let tertiary = ollama_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
                ProviderEntry::new("ollama", tertiary, 2),
            ],
            ProviderPoolConfig::default(),
        );

        // Primary succeeds — no fallback
        let response = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "anthropic");
        assert_eq!(pool.fallback_log_count(), 0);
    }

    #[tokio::test]
    async fn test_pool_circuit_breaker_skips_open_provider() {
        let cb = Arc::new(ThresholdCircuitBreaker::with_config(
            CircuitBreakerConfig::new(1, 1, 60) // Open after 1 failure
        ));

        let primary = failing_test_provider("API error");
        let secondary = openai_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default(),
        ).with_circuit_breaker(cb.clone());

        // First call — anthropic fails, circuit opens, fallback to openai
        let response = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "openai");

        // Circuit should be open for anthropic
        let state = cb.check("anthropic");
        assert!(state.is_failing());
    }

    #[tokio::test]
    async fn test_pool_rate_limiter_skips_rate_limited_provider() {
        // Create a rate limiter with very low limit for anthropic
        let rl = Arc::new(TokenWindowRateLimiter::with_config(
            RateLimitConfig::new()
                .with_provider_limit("anthropic", oneai_core::rate_limiter::ProviderRateLimit::new(1, 100))
        ));

        let primary = anthropic_test_provider();
        let secondary = openai_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default(),
        ).with_rate_limiter(rl.clone());

        // First call — anthropic succeeds (rate limit allows 1 call)
        let response1 = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "anthropic");

        // Record the call manually to exhaust the rate limit
        rl.record_call("anthropic").await.unwrap();

        // Second call — anthropic rate limited, fallback to openai
        let response2 = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "openai");
    }

    #[tokio::test]
    async fn test_pool_streaming_fallback() {
        let primary = failing_test_provider("Stream error");
        let secondary = openai_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default(),
        );

        let stream = pool.infer_stream(test_request()).await.unwrap();
        let chunks: Vec<InferenceStreamChunk> = stream.collect().await;

        // Should have at least 2 chunks (content + final)
        assert!(chunks.len() >= 2);
        assert!(chunks.last().unwrap().is_final);
        assert_eq!(pool.active_provider_name(), "openai");
    }

    #[tokio::test]
    async fn test_pool_streaming_primary_succeeds() {
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", anthropic_test_provider(), 0),
                ProviderEntry::new("openai", openai_test_provider(), 1),
            ],
            ProviderPoolConfig::default(),
        );

        let stream = pool.infer_stream(test_request()).await.unwrap();
        let chunks: Vec<InferenceStreamChunk> = stream.collect().await;

        assert!(chunks.len() >= 2);
        assert_eq!(pool.active_provider_name(), "anthropic");
        assert_eq!(pool.fallback_log_count(), 0);
    }

    #[tokio::test]
    async fn test_pool_active_provider_name_tracking() {
        let primary = failing_test_provider("error");
        let secondary = openai_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default(),
        );

        // Initially should track primary
        assert_eq!(pool.active_provider_name(), "anthropic");

        // After fallback, should track secondary
        pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "openai");
    }

    #[tokio::test]
    async fn test_pool_fallback_log_audit_trail() {
        let primary = failing_test_provider("error 1");
        let secondary = failing_test_provider("error 2");
        let tertiary = ollama_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
                ProviderEntry::new("ollama", tertiary, 2),
            ],
            ProviderPoolConfig::default(),
        );

        let response = pool.infer(test_request()).await.unwrap();

        // Should have 2 fallback events
        assert_eq!(pool.fallback_log_count(), 2);
        let events = pool.fallback_log_recent(2);

        // First event: anthropic → openai
        assert_eq!(events[1].from_provider, "anthropic");
        assert_eq!(events[1].to_provider, "openai");

        // Second event: openai → ollama
        assert_eq!(events[0].from_provider, "openai");
        assert_eq!(events[0].to_provider, "ollama");

        // Final response from ollama
        assert_eq!(pool.active_provider_name(), "ollama");
    }

    #[tokio::test]
    async fn test_pool_status() {
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", anthropic_test_provider(), 0),
                ProviderEntry::new("openai", openai_test_provider(), 1),
            ],
            ProviderPoolConfig::default(),
        );

        let status = pool.status().await;
        assert_eq!(status.active_provider, "anthropic");
        assert_eq!(status.total_providers, 2);
        assert!(status.has_healthy_provider());
        assert_eq!(status.healthy_provider_count(), 2);
    }

    #[tokio::test]
    async fn test_pool_status_with_circuit_breaker() {
        let cb = Arc::new(ThresholdCircuitBreaker::new());
        // Open circuit for anthropic
        for _ in 0..5 {
            cb.record_failure("anthropic", "error");
        }

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", anthropic_test_provider(), 0),
                ProviderEntry::new("openai", openai_test_provider(), 1),
            ],
            ProviderPoolConfig::default(),
        ).with_circuit_breaker(cb.clone());

        let status = pool.status().await;
        // Anthropic should show as open circuit
        let anthropic_health = status.provider_health.get("anthropic").unwrap();
        assert!(!anthropic_health.is_available);
        assert_eq!(anthropic_health.circuit_state, Some("open".to_string()));
    }

    #[tokio::test]
    async fn test_pool_single_provider() {
        let pool = ProviderPool::single(anthropic_test_provider(), "anthropic");
        assert_eq!(pool.provider_count(), 1);
        assert_eq!(pool.active_provider_name(), "anthropic");

        let response = pool.infer(test_request()).await.unwrap();
        assert!(response.message.content.len() > 0);
    }

    #[tokio::test]
    async fn test_pool_from_config() {
        let config = ProviderPoolConfig::anthropic_primary(
            Some("sk-ant-test".to_string()),
            Some("sk-test".to_string()),
        );

        let pool = ProviderPool::from_config(config);
        assert_eq!(pool.provider_count(), 3);
        assert_eq!(pool.provider_names()[0], "anthropic");
    }

    #[tokio::test]
    async fn test_pool_cooldown_skips_recently_failed_provider() {
        let primary = Arc::new(TestProvider::new("anthropic", "claude-sonnet"));
        let secondary = openai_test_provider();

        // Make primary fail once
        primary.set_failing(true);

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary.clone(), 0).with_cooldown(10),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default(),
        );

        // First call — anthropic fails (in cooldown for 10 seconds)
        let response = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "openai");

        // Now primary should be in cooldown
        let entry = &pool.entries[0];
        assert!(entry.is_in_cooldown().await);

        // Fix primary
        primary.set_failing(false);

        // Even though primary is now healthy, it's still in cooldown
        // (but the cooldown check happens in infer_with_fallback, not separately)
    }

    #[tokio::test]
    async fn test_pool_max_fallbacks_limits_attempts() {
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", failing_test_provider("err"), 0),
                ProviderEntry::new("openai", failing_test_provider("err"), 1),
                ProviderEntry::new("ollama", failing_test_provider("err"), 2),
            ],
            ProviderPoolConfig::default().with_max_fallbacks(2), // Only try 2 providers
        );

        let result = pool.infer(test_request()).await;
        assert!(result.is_err());
        // Should have 2 fallback events (not 3, because max_fallbacks = 2)
        assert_eq!(pool.fallback_log_count(), 2);
    }

    #[tokio::test]
    async fn test_pool_capabilities_delegates_to_active() {
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", anthropic_test_provider(), 0),
                ProviderEntry::new("openai", openai_test_provider(), 1),
            ],
            ProviderPoolConfig::default(),
        );

        let caps = pool.capabilities();
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
    }

    #[test]
    fn test_pool_config_delegates_to_primary() {
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", anthropic_test_provider(), 0),
                ProviderEntry::new("openai", openai_test_provider(), 1),
            ],
            ProviderPoolConfig::default(),
        );

        let config = pool.config();
        assert_eq!(config.cloud_kind, Some(oneai_core::CloudProviderKind::Anthropic));
    }

    #[test]
    fn test_pool_provider_names() {
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("openai", openai_test_provider(), 1),
                ProviderEntry::new("anthropic", anthropic_test_provider(), 0),
            ],
            ProviderPoolConfig::default(),
        );

        // Should be sorted by priority
        let names = pool.provider_names();
        assert_eq!(names, vec!["anthropic", "openai"]);
    }
}
