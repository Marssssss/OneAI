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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tokio::sync::RwLock;

use crate::ProviderFactory;
use crate::SmartRouter;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::LlmProvider;
use oneai_core::{
    CircuitBreaker, DegradationRule, FallbackEvent, FallbackLog, FallbackReason,
    InMemoryFallbackLog, InferenceRequest, InferenceResponse, InferenceStreamChunk,
    ModelCapability, ModelConfig, ProviderHealthStatus, ProviderPoolConfig, ProviderPoolStatus,
    RateLimiter, UsageTracker,
};

/// Injectable factory closure that builds a fresh `LlmProvider` from a
/// `ModelConfig` (used by within-family model degradation).
type ProviderBuilder = Arc<dyn Fn(&ModelConfig) -> Box<dyn LlmProvider> + Send + Sync>;

// ─── ProviderEntry ─────────────────────────────────────────────────────────────

/// A single provider entry in the fallback pool.
///
/// Wraps an `Arc<dyn LlmProvider>` with metadata for circuit breaker,
/// rate limiter, and usage tracking integration.
#[derive(Clone)]
pub struct ProviderEntry {
    /// Provider name for circuit breaker / rate limiter / usage tracking.
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
    pub fn new(name: impl Into<String>, provider: Arc<dyn LlmProvider>, priority: u32) -> Self {
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
        self.provider
            .config()
            .model_name
            .as_deref()
            .unwrap_or("unknown")
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
/// Integrates with CircuitBreaker, RateLimiter, UsageTracker, and FallbackLog
/// for full production-grade resilience.
pub struct ProviderPool {
    /// Ordered provider entries (primary first). Behind a sync `RwLock` so the
    /// pool can add/remove entries live (`add_entry`/`remove_entry`) without
    /// rebuilding — the app-server holds one pool as `App.provider` for its
    /// whole lifetime and mutates it via the `provider/*` RPCs. Read paths use
    /// `entries_snapshot()` (clone the `Vec` — cheap, `Arc` bumps — then drop
    /// the guard before any `.await`, so writes never block on a provider call).
    entries: Arc<std::sync::RwLock<Vec<ProviderEntry>>>,

    /// Pool configuration (max fallbacks, degradation rules, etc.).
    config: ProviderPoolConfig,

    /// Circuit breaker — skip providers with Open circuits.
    circuit_breaker: Option<Arc<dyn CircuitBreaker>>,

    /// Rate limiter — respect per-provider rate limits.
    rate_limiter: Option<Arc<dyn RateLimiter>>,

    /// Usage tracker — record usage for whichever provider succeeded.
    usage_tracker: Option<Arc<dyn UsageTracker>>,

    /// Smart router — intelligent primary selection based on latency/quality.
    /// When present, the pool uses the router to determine which provider to try
    /// first (instead of always trying the primary). If that provider fails,
    /// fallback continues as usual.
    smart_router: Option<Arc<SmartRouter>>,

    /// Currently active provider index (Atomically updated on fallback).
    active_index: AtomicU32,

    /// Fallback event log — audit trail for observability.
    fallback_log: Arc<dyn FallbackLog>,

    /// Builder for degraded providers from a (model-swapped) `ModelConfig`.
    ///
    /// Used by within-family model degradation: clones the failing entry's
    /// `ModelConfig`, swaps `model_name` to the next degraded tier, and builds
    /// a fresh provider through this builder. Defaults to `ProviderFactory`;
    /// injectable (via `with_provider_builder`) for testing or alternate
    /// factory wiring.
    provider_builder: ProviderBuilder,

    /// Bounded leak cache for `config()`: the `LlmProvider::config(&self) ->
    /// &ModelConfig` trait returns a reference valid for `&self`, but the entry
    /// list is behind a `RwLock` (live add/remove) so a read guard can't be
    /// returned. Instead we leak one `ModelConfig` per distinct provider name
    /// to `&'static` (mirrors the `FALLBACK_CONFIG` `OnceLock` pattern below
    /// for the empty-pool case). Bounded by the number of providers ever
    /// activated — a handful — each `ModelConfig` is a small struct.
    config_cache: std::sync::Mutex<HashMap<String, &'static ModelConfig>>,
}

/// Default degraded-provider builder — delegates to `ProviderFactory::create`.
fn default_provider_builder(cfg: &ModelConfig) -> Box<dyn LlmProvider> {
    ProviderFactory::create(cfg.clone())
}

impl ProviderPool {
    /// Create a provider pool with the given entries and configuration.
    pub fn new(entries: Vec<ProviderEntry>, config: ProviderPoolConfig) -> Self {
        // Sort entries by priority (primary first)
        let mut sorted = entries;
        sorted.sort_by_key(|e| e.priority);

        Self {
            entries: Arc::new(std::sync::RwLock::new(sorted)),
            config,
            circuit_breaker: None,
            rate_limiter: None,
            usage_tracker: None,
            smart_router: None,
            active_index: AtomicU32::new(0),
            fallback_log: Arc::new(InMemoryFallbackLog::new()),
            provider_builder: Arc::new(default_provider_builder),
            config_cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Create a pool with just the configuration (entries built from entry configs).
    pub fn from_config(config: ProviderPoolConfig) -> Self {
        let entries: Vec<ProviderEntry> = config
            .entries
            .iter()
            .map(|entry_config| {
                let provider = ProviderFactory::create(entry_config.model_config.clone());
                ProviderEntry::new(
                    entry_config.name.clone(),
                    Arc::from(provider),
                    entry_config.priority,
                )
                .with_cooldown(entry_config.cooldown_secs)
            })
            .collect();

        Self::new(entries, config)
    }

    /// Create a minimal pool with a single provider (no fallback).
    pub fn single(provider: Arc<dyn LlmProvider>, name: impl Into<String>) -> Self {
        Self::new(
            vec![ProviderEntry::new(name, provider, 0)],
            ProviderPoolConfig::default(),
        )
    }

    /// Snapshot the entries vec under a brief read lock (clone — cheap `Arc`
    /// bumps) then drop the guard. Every read path uses this so no `.await`
    /// ever runs while holding the lock.
    fn entries_snapshot(&self) -> Vec<ProviderEntry> {
        self.entries.read().expect("entries lock poisoned").clone()
    }

    /// Inference order for the no-smart-router path: lead with the user-
    /// selected active entry (issue #37 — `provider/set_active` must survive
    /// inference), then the remaining entries in priority order for fallback.
    /// Without leading with the active entry, the chain always starts at
    /// priority-order index 0 and the success handler snaps `active_index`
    /// back there, silently undoing the manual selection.
    fn active_first_indices(&self, entries: &[ProviderEntry]) -> Vec<usize> {
        let active = self.active_index.load(Ordering::Relaxed) as usize;
        let mut indices: Vec<usize> = Vec::with_capacity(entries.len());
        if active < entries.len() {
            indices.push(active);
        }
        for idx in 0..entries.len() {
            if !indices.contains(&idx) {
                indices.push(idx);
            }
        }
        indices
    }

    /// Live-switch the active provider by name (atomic `active_index` store).
    /// No `App.provider` swap — the pool IS the provider; it routes to the
    /// active entry. Returns Err if the name isn't in the pool.
    pub fn set_active_by_name(&self, name: &str) -> std::result::Result<(), String> {
        let entries = self.entries_snapshot();
        let idx = entries
            .iter()
            .position(|e| e.name == name)
            .ok_or_else(|| format!("unknown provider: {name}"))?;
        self.active_index.store(idx as u32, Ordering::Relaxed);
        Ok(())
    }

    /// Add a provider entry live (push under a write lock). If an entry with the
    /// same name exists, it's replaced. The added provider is immediately
    /// selectable via `set_active_by_name`.
    pub fn add_entry(&self, entry: ProviderEntry) {
        let mut entries = self.entries.write().expect("entries lock poisoned");
        if let Some(pos) = entries.iter().position(|e| e.name == entry.name) {
            entries[pos] = entry;
        } else {
            entries.push(entry);
        }
    }

    /// Replace a provider entry's provider in place (write lock), preserving
    /// its position and priority. Unlike [`ProviderPool::add_entry`] — which
    /// replaces by name but rewrites priority to the caller-supplied value —
    /// this keeps the existing priority so a live model/endpoint edit
    /// (`provider/update` · `provider/set_model`) doesn't reorder the pool.
    /// Returns `true` if an entry with `name` existed (and was replaced).
    pub fn replace_entry(&self, name: &str, provider: Arc<dyn LlmProvider>) -> bool {
        let mut entries = self.entries.write().expect("entries lock poisoned");
        let Some(pos) = entries.iter().position(|e| e.name == name) else {
            return false;
        };
        let priority = entries[pos].priority();
        entries[pos] = ProviderEntry::new(name.to_string(), provider, priority);
        true
    }

    /// Remove a provider entry by name (write lock + retain). If the removed
    /// entry was active, `active_index` is reset to 0 (the next primary).
    pub fn remove_entry(&self, name: &str) {
        let mut entries = self.entries.write().expect("entries lock poisoned");
        let active_idx = self.active_index.load(Ordering::Relaxed) as usize;
        if let Some(pos) = entries.iter().position(|e| e.name == name) {
            entries.remove(pos);
            // Adjust active_index: if the removed entry was before the active
            // one, the active shifts left by one; if it WAS the active one,
            // fall back to 0; otherwise clamp to the new length.
            let new_active = if entries.is_empty() {
                0
            } else if pos < active_idx {
                active_idx - 1
            } else if pos == active_idx {
                0
            } else {
                active_idx.min(entries.len() - 1)
            };
            self.active_index
                .store(new_active as u32, Ordering::Relaxed);
        }
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

    /// Set the usage tracker for usage recording.
    pub fn with_usage_tracker(mut self, ct: Arc<dyn UsageTracker>) -> Self {
        self.usage_tracker = Some(ct);
        self
    }

    /// Set the smart router for intelligent primary selection.
    ///
    /// When a smart router is attached, the pool uses it to determine
    /// which provider to try first for each inference call. Instead of
    /// always trying the primary provider, the pool starts with the
    /// smart router's recommendation (which considers latency, quality,
    /// health, and context constraints).
    ///
    /// If the smart router's chosen provider fails, fallback continues
    /// as usual (trying next providers in priority order).
    ///
    /// Without a smart router, the pool behavior is unchanged (backward compat).
    pub fn with_smart_router(mut self, router: Arc<SmartRouter>) -> Self {
        self.smart_router = Some(router);
        self
    }

    /// Set a custom fallback log (for OTEL / database integration).
    pub fn with_fallback_log(mut self, log: Arc<dyn FallbackLog>) -> Self {
        self.fallback_log = log;
        self
    }

    /// Override the provider builder used for within-family model degradation.
    ///
    /// The default builds a fresh provider via `ProviderFactory::create` from
    /// the failing entry's `ModelConfig` (with `model_name` swapped to the
    /// degraded tier). Inject a custom builder for testing or to route
    /// degraded construction through a different factory.
    pub fn with_provider_builder<F>(mut self, builder: F) -> Self
    where
        F: Fn(&ModelConfig) -> Box<dyn LlmProvider> + Send + Sync + 'static,
    {
        self.provider_builder = Arc::new(builder);
        self
    }

    /// Get the name of the currently active provider.
    pub fn active_provider_name(&self) -> String {
        let entries = self.entries_snapshot();
        let idx = self.active_index.load(Ordering::Relaxed) as usize;
        if idx < entries.len() {
            entries[idx].name.clone()
        } else {
            "unknown".to_string()
        }
    }

    /// Get the model name of the currently active provider.
    pub fn active_model_name(&self) -> String {
        let entries = self.entries_snapshot();
        let idx = self.active_index.load(Ordering::Relaxed) as usize;
        if idx < entries.len() {
            entries[idx].model_name().to_string()
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
        let entries = self.entries_snapshot();
        let active_idx = self.active_index.load(Ordering::Relaxed) as usize;
        let active_name = if active_idx < entries.len() {
            entries[active_idx].name.clone()
        } else {
            "unknown".to_string()
        };
        let active_model = if active_idx < entries.len() {
            entries[active_idx].model_name().to_string()
        } else {
            "unknown".to_string()
        };

        let mut provider_health = HashMap::new();
        for entry in &entries {
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

            let failure_count = self
                .circuit_breaker
                .as_ref()
                .map(|cb| cb.failure_count(&entry.name));

            let actually_available = if let Some(cb) = &self.circuit_breaker {
                is_available && cb.check(&entry.name).allows_calls()
            } else {
                is_available
            };

            provider_health.insert(
                entry.name.clone(),
                ProviderHealthStatus::new(
                    entry.name.clone(),
                    entry.model_name(),
                    entry.priority,
                    actually_available,
                    circuit_state,
                    failure_count,
                ),
            );
        }

        let recent_fallback_count = self.fallback_log.total_count();
        let last_fallback = self.fallback_log.recent_events(1).first().cloned();

        ProviderPoolStatus::new(active_name, active_model, entries.len()).into_status_with_health(
            provider_health,
            recent_fallback_count,
            last_fallback,
        )
    }

    // ─── Within-family model degradation ──────────────────────────────────────

    /// Compute the within-family degraded model chain for an entry.
    ///
    /// Returns the ordered list of degraded models to try *excluding* the
    /// entry's current model. Empty when `degrade_on_fallback` is disabled,
    /// no degradation rules are configured, or the entry's name has no
    /// matching rule (the entry name doubles as the provider family).
    fn degradation_chain(&self, entry_idx: usize) -> Vec<String> {
        if !self.config.degrade_on_fallback || self.config.degradation_rules.is_empty() {
            return Vec::new();
        }
        let entries = self.entries_snapshot();
        let Some(entry) = entries.get(entry_idx) else {
            return Vec::new();
        };
        let Some(rule) =
            DegradationRule::find_for_provider(&self.config.degradation_rules, &entry.name)
        else {
            return Vec::new();
        };
        let mut chain = Vec::new();
        let mut current = entry.model_name().to_string();
        while let Some(next) = rule.next_degraded_model(&current) {
            chain.push(next.clone());
            current = next;
        }
        chain
    }

    /// Build a fresh degraded provider for `entry_idx` with `model_name` swapped in.
    ///
    /// Clones the entry's own `ModelConfig` (preserving api key, base_url,
    /// provider kind) and overrides only `model_name`, then constructs the
    /// provider through the injectable `provider_builder`.
    fn build_degraded_provider(
        &self,
        entry_idx: usize,
        model_name: &str,
    ) -> Option<Box<dyn LlmProvider>> {
        let entries = self.entries_snapshot();
        let entry = entries.get(entry_idx)?;
        let mut cfg = entry.provider.config().clone();
        cfg.model_name = Some(model_name.to_string());
        Some((self.provider_builder)(&cfg))
    }

    /// Try within-family model degradation after the primary model failed.
    ///
    /// Iterates the degradation chain, building a fresh provider for each
    /// degraded tier and retrying inference. On the first success: logs a
    /// `ModelDegradation` fallback event (same provider, lower tier),
    /// records success in the circuit breaker, clears cooldown, records
    /// usage, and returns the response. Returns `None` when no degraded
    /// model is configured or every degraded tier also failed — in which
    /// case the caller proceeds to cross-provider fallback.
    async fn attempt_degradation(
        &self,
        entry_idx: usize,
        request: &InferenceRequest,
    ) -> Option<InferenceResponse> {
        let entries = self.entries_snapshot();
        let entry = entries.get(entry_idx)?.clone();
        let from_model = entry.model_name().to_string();
        let chain = self.degradation_chain(entry_idx);
        if chain.is_empty() {
            return None;
        }

        for next_model in chain {
            let Some(degraded) = self.build_degraded_provider(entry_idx, &next_model) else {
                continue;
            };
            tracing::info!(
                "Attempting within-family degradation for {}: {} → {}",
                entry.name,
                from_model,
                next_model
            );
            match degraded.infer(request.clone()).await {
                Ok(response) => {
                    tracing::info!(
                        "Inference succeeded on degraded model {} (family {})",
                        next_model,
                        entry.name
                    );
                    self.fallback_log.log_fallback(FallbackEvent::new(
                        entry.name.clone(),
                        entry.name.clone(),
                        FallbackReason::ModelDegradation {
                            from_model: from_model.clone(),
                            to_model: next_model.clone(),
                        },
                        from_model.clone(),
                        next_model.clone(),
                    ));
                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_success(&entry.name);
                    }
                    entry.clear_cooldown().await;
                    if let Some(ct) = &self.usage_tracker {
                        let record = oneai_core::UsageRecord::new(
                            request.conversation.id.clone(),
                            response.model.clone(),
                            entry.name.clone(),
                            response.usage.prompt_tokens,
                            response.usage.completion_tokens,
                        );
                        let _ = ct.record_usage(record).await;
                    }
                    self.active_index.store(entry_idx as u32, Ordering::Relaxed);
                    return Some(response);
                }
                Err(e) => {
                    tracing::warn!(
                        "Degraded model {} also failed for {}: {}",
                        next_model,
                        entry.name,
                        e
                    );
                }
            }
        }
        None
    }

    /// Streaming counterpart of `attempt_degradation`.
    ///
    /// Same chain walk, but opens a stream rather than a blocking response.
    /// Like the cross-provider streaming path, degradation only triggers when
    /// `infer_stream` itself fails — a mid-stream error propagates as-is.
    async fn attempt_degradation_stream(
        &self,
        entry_idx: usize,
        request: &InferenceRequest,
    ) -> Option<Pin<Box<dyn Stream<Item = InferenceStreamChunk> + Send>>> {
        let entries = self.entries_snapshot();
        let entry = entries.get(entry_idx)?.clone();
        let from_model = entry.model_name().to_string();
        let chain = self.degradation_chain(entry_idx);
        if chain.is_empty() {
            return None;
        }

        for next_model in chain {
            let Some(degraded) = self.build_degraded_provider(entry_idx, &next_model) else {
                continue;
            };
            tracing::info!(
                "Attempting within-family degradation (stream) for {}: {} → {}",
                entry.name,
                from_model,
                next_model
            );
            match degraded.infer_stream(request.clone()).await {
                Ok(stream) => {
                    self.fallback_log.log_fallback(FallbackEvent::new(
                        entry.name.clone(),
                        entry.name.clone(),
                        FallbackReason::ModelDegradation {
                            from_model: from_model.clone(),
                            to_model: next_model.clone(),
                        },
                        from_model.clone(),
                        next_model.clone(),
                    ));
                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_success(&entry.name);
                    }
                    entry.clear_cooldown().await;
                    self.active_index.store(entry_idx as u32, Ordering::Relaxed);
                    return Some(stream);
                }
                Err(e) => {
                    tracing::warn!(
                        "Degraded model {} (stream) also failed for {}: {}",
                        next_model,
                        entry.name,
                        e
                    );
                }
            }
        }
        None
    }

    /// Try inference with fallback chain.
    ///
    /// Iterates through providers in priority order (or smart router order
    /// if a smart router is attached), skipping providers that are in cooldown,
    /// have open circuits, or are rate-limited.
    /// On success, records success in circuit breaker and usage tracker.
    /// On failure, records failure and logs a FallbackEvent.
    async fn infer_with_fallback(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        let entries = self.entries_snapshot();
        let max_attempts = self.config.max_fallbacks.min(entries.len());
        let mut attempts = 0;

        // If a smart router is attached, use it to determine provider order
        // Otherwise, use the default priority order
        let ordered_indices: Vec<usize> = if let Some(router) = &self.smart_router {
            // Use smart router to pick the best provider first
            let decision = router
                .route_for_pool(
                    "",      // No task description available at pool level
                    "react", // Default paradigm
                    &self.config,
                    None, // No session context at pool level
                    None, // No conversation token count at pool level
                )
                .await;

            // Reorder: start with the smart router's recommendation,
            // then continue with remaining providers in priority order
            let recommended_name = decision.provider;
            let mut indices: Vec<usize> = Vec::new();

            // Find the recommended provider's index
            if let Some(idx) = entries.iter().position(|e| e.name == recommended_name) {
                indices.push(idx);
            }

            // Add remaining providers in priority order
            for (idx, _entry) in entries.iter().enumerate() {
                if !indices.contains(&idx) {
                    indices.push(idx);
                }
            }

            tracing::info!(
                "SmartRouter recommends provider '{}' (model '{}'), reordered provider chain",
                recommended_name,
                decision.model,
            );

            indices
        } else {
            // Default: lead with the active entry (issue #37), then fall back
            // through the remaining entries in priority order.
            self.active_first_indices(&entries)
        };

        for idx in ordered_indices {
            if attempts >= max_attempts {
                break;
            }

            let entry = &entries[idx];
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
            tracing::info!(
                "Attempting inference with provider {} (attempt {})",
                entry.name,
                attempts
            );

            let result = entry.provider.infer(request.clone()).await;

            match result {
                Ok(response) => {
                    // Success!
                    tracing::info!("Inference succeeded with provider {}", entry.name);

                    // Update active index
                    self.active_index.store(
                        entries
                            .iter()
                            .position(|e| e.name == entry.name)
                            .unwrap_or(0) as u32,
                        Ordering::Relaxed,
                    );

                    // Record success in circuit breaker
                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_success(&entry.name);
                    }

                    // Clear cooldown
                    entry.clear_cooldown().await;

                    // Record usage in usage tracker
                    if let Some(ct) = &self.usage_tracker {
                        let record = oneai_core::UsageRecord::new(
                            request.conversation.id.clone(),
                            response.model.clone(),
                            entry.name.clone(),
                            response.usage.prompt_tokens,
                            response.usage.completion_tokens,
                        );
                        let _ = ct.record_usage(record).await;
                    }

                    return Ok(response);
                }
                Err(error) => {
                    // Failure — record and try degradation, then cross-provider fallback
                    tracing::warn!("Inference failed with provider {}: {}", entry.name, error);

                    // Record failure in circuit breaker
                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_failure(&entry.name, &error.to_string());
                    }

                    // Record failure time for cooldown
                    entry.record_failure_time().await;

                    // ── Within-family model degradation (same provider, lower tier)
                    if let Some(response) = self.attempt_degradation(idx, &request).await {
                        return Ok(response);
                    }

                    // ── Cross-provider fallback — log and continue to next entry
                    let next_idx = entries.iter().position(|e| e.priority > entry.priority);
                    let (next_name, next_model) = if let Some(idx) = next_idx {
                        (
                            entries[idx].name.clone(),
                            entries[idx].model_name().to_string(),
                        )
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
        Err(OneAIError::Fallback(format!(
            "All providers exhausted after {} attempts (pool has {} providers)",
            attempts,
            entries.len()
        )))
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
        let entries = self.entries_snapshot();
        let max_attempts = self.config.max_fallbacks.min(entries.len());
        let mut attempts = 0;

        // If a smart router is attached, use it to determine provider order
        let ordered_indices: Vec<usize> = if let Some(router) = &self.smart_router {
            let decision = router
                .route_for_pool("", "react", &self.config, None, None)
                .await;

            let recommended_name = decision.provider;
            let mut indices: Vec<usize> = Vec::new();

            if let Some(idx) = entries.iter().position(|e| e.name == recommended_name) {
                indices.push(idx);
            }

            for (idx, _entry) in entries.iter().enumerate() {
                if !indices.contains(&idx) {
                    indices.push(idx);
                }
            }

            tracing::info!(
                "SmartRouter recommends '{}' for streaming, reordered chain",
                recommended_name,
            );

            indices
        } else {
            // Default: lead with the active entry (issue #37), then fall back
            // through the remaining entries in priority order.
            self.active_first_indices(&entries)
        };

        for idx in ordered_indices {
            if attempts >= max_attempts {
                break;
            }

            let entry = &entries[idx];
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
            tracing::info!(
                "Attempting streaming inference with provider {} (attempt {})",
                entry.name,
                attempts
            );

            let result = entry.provider.infer_stream(request.clone()).await;

            match result {
                Ok(stream) => {
                    // Success — update active index and record success
                    self.active_index.store(
                        entries
                            .iter()
                            .position(|e| e.name == entry.name)
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
                    // Failure — record and try degradation, then cross-provider fallback
                    tracing::warn!(
                        "Streaming inference failed with provider {}: {}",
                        entry.name,
                        error
                    );

                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_failure(&entry.name, &error.to_string());
                    }
                    entry.record_failure_time().await;

                    // ── Within-family model degradation (same provider, lower tier)
                    if let Some(stream) = self.attempt_degradation_stream(idx, &request).await {
                        return Ok(stream);
                    }

                    // ── Cross-provider fallback — log and continue to next entry
                    let next_idx = entries.iter().position(|e| e.priority > entry.priority);
                    let (next_name, next_model) = if let Some(idx) = next_idx {
                        (
                            entries[idx].name.clone(),
                            entries[idx].model_name().to_string(),
                        )
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

        Err(OneAIError::Fallback(format!(
            "All providers exhausted for streaming after {} attempts",
            attempts
        )))
    }

    /// Get the number of providers in the pool.
    pub fn provider_count(&self) -> usize {
        self.entries_snapshot().len()
    }

    /// Get provider names in priority order.
    pub fn provider_names(&self) -> Vec<String> {
        self.entries_snapshot()
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    /// Snapshot every entry as `(name, model_config)` — used by the app-server
    /// probe to report the live provider list (with the active marker resolved
    /// from `active_index`). Reads each provider's `config()` by reference
    /// under the brief snapshot lock, then clones out.
    pub fn provider_entries_view(&self) -> Vec<(String, ModelConfig)> {
        self.entries_snapshot()
            .iter()
            .map(|e| (e.name.clone(), e.provider.config().clone()))
            .collect()
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
        let entries = self.entries_snapshot();
        let idx = self.active_index.load(Ordering::Relaxed) as usize;
        if idx < entries.len() {
            entries[idx].provider.capabilities()
        } else if !entries.is_empty() {
            // Fallback to first entry's capabilities
            entries[0].provider.capabilities()
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

    /// List the models available at the ACTIVE entry's endpoint (delegates to
    /// the entry's own `list_models`). Empty when the pool has no entries.
    async fn list_models(&self) -> Vec<String> {
        let entries = self.entries_snapshot();
        let idx = self.active_index.load(Ordering::Relaxed) as usize;
        let entry = if idx < entries.len() {
            &entries[idx]
        } else {
            return Vec::new();
        };
        entry.provider.list_models().await
    }

    /// Get the model config of the currently active provider.
    ///
    /// The trait returns `&ModelConfig` valid for `&self`, but the entry list
    /// is behind a `RwLock` (live add/remove) — a read guard can't be returned
    /// as `&`. So we leak one `ModelConfig` per distinct provider name to
    /// `&'static` (cached in `config_cache`), mirroring the `FALLBACK_CONFIG`
    /// `OnceLock` below for the empty-pool case. Bounded, safe, consistent.
    fn config(&self) -> &ModelConfig {
        let entries = self.entries_snapshot();
        let idx = self.active_index.load(Ordering::Relaxed) as usize;
        let provider = if idx < entries.len() {
            &entries[idx]
        } else if !entries.is_empty() {
            &entries[0]
        } else {
            // No providers — this shouldn't happen in practice
            // Return a reference from a leaked Box as a last resort
            // (only used in error paths, never in normal operation)
            static FALLBACK_CONFIG: std::sync::OnceLock<ModelConfig> = std::sync::OnceLock::new();
            return FALLBACK_CONFIG.get_or_init(|| ModelConfig {
                provider_type: oneai_core::ProviderType::Cloud,
                cloud_kind: None,
                api_key: None,
                base_url: None,
                port: None,
                model_name: Some("unknown".to_string()),
                model_path: None,
                extra: std::collections::HashMap::new(),
            });
        };
        let name = provider.name.clone();
        let mut cache = self.config_cache.lock().expect("config cache poisoned");
        if let Some(cfg) = cache.get(&name) {
            return cfg;
        }
        let leaked: &'static ModelConfig = Box::leak(Box::new(provider.provider.config().clone()));
        cache.insert(name, leaked);
        leaked
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt; // test-only (boxed-stream .collect in tests)
                            // These core types are referenced only by the tests below; kept here (not at
                            // module scope) to avoid unused-import warnings in the non-test build.
    use oneai_core::circuit_breaker::{CircuitBreakerConfig, ThresholdCircuitBreaker};
    use oneai_core::rate_limiter::{RateLimitConfig, TokenWindowRateLimiter};
    use oneai_core::{ContentBlock, Message, Role, TokenUsage};

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
                (
                    oneai_core::ProviderType::Cloud,
                    Some(oneai_core::CloudProviderKind::Anthropic),
                )
            } else if name == "openai" {
                (
                    oneai_core::ProviderType::Cloud,
                    Some(oneai_core::CloudProviderKind::OpenAI),
                )
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

        /// Build a succeeding TestProvider straight from a ModelConfig — used by
        /// the injectable degraded-provider builder in degradation tests.
        fn from_config(config: ModelConfig) -> Self {
            Self {
                config,
                should_fail: std::sync::Mutex::new(false),
                fail_message: "Provider error".to_string(),
                call_count: std::sync::Mutex::new(0),
            }
        }

        /// Build a failing TestProvider from a ModelConfig.
        fn from_config_failing(config: ModelConfig, message: &str) -> Self {
            let mut provider = Self::from_config(config);
            *provider.should_fail.lock().unwrap() = true;
            provider.fail_message = message.to_string();
            provider
        }

        fn set_failing(&self, fail: bool) {
            *self.should_fail.lock().unwrap() = fail;
        }

        #[allow(dead_code)] // test-fixture accessor for ad-hoc pool diagnostics
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
                        text: format!(
                            "Response from {}",
                            self.config.model_name.as_deref().unwrap_or("unknown")
                        ),
                    }],
                    metadata: HashMap::new(),
                },
                usage: TokenUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    ..Default::default()
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
                    })
                    .await
                    .ok();
                }
                tx.send(InferenceStreamChunk {
                    content: vec![],
                    is_final: true,
                    usage: Some(response.usage.clone()),
                    model: Some(response.model.clone()),
                })
                .await
                .ok();
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
        assert_eq!(pool.active_provider_name(), "anthropic");
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
        let _response = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "anthropic");
        assert_eq!(pool.fallback_log_count(), 0);
    }

    #[tokio::test]
    async fn test_pool_circuit_breaker_skips_open_provider() {
        let cb = Arc::new(ThresholdCircuitBreaker::with_config(
            CircuitBreakerConfig::new(1, 1, 60), // Open after 1 failure
        ));

        let primary = failing_test_provider("API error");
        let secondary = openai_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default(),
        )
        .with_circuit_breaker(cb.clone());

        // First call — anthropic fails, circuit opens, fallback to openai
        let _response = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "openai");

        // Circuit should be open for anthropic
        let state = cb.check("anthropic");
        assert!(state.is_failing());
    }

    #[tokio::test]
    async fn test_pool_rate_limiter_skips_rate_limited_provider() {
        // Create a rate limiter with very low limit for anthropic
        let rl = Arc::new(TokenWindowRateLimiter::with_config(
            RateLimitConfig::new().with_provider_limit(
                "anthropic",
                oneai_core::rate_limiter::ProviderRateLimit::new(1, 100),
            ),
        ));

        let primary = anthropic_test_provider();
        let secondary = openai_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", primary, 0),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default(),
        )
        .with_rate_limiter(rl.clone());

        // First call — anthropic succeeds (rate limit allows 1 call)
        let _response1 = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "anthropic");

        // Record the call manually to exhaust the rate limit
        rl.record_call("anthropic").await.unwrap();

        // Second call — anthropic rate limited, fallback to openai
        let _response2 = pool.infer(test_request()).await.unwrap();
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

        let _response = pool.infer(test_request()).await.unwrap();

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
        )
        .with_circuit_breaker(cb.clone());

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
        assert!(!response.message.content.is_empty());
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
        let _response = pool.infer(test_request()).await.unwrap();
        assert_eq!(pool.active_provider_name(), "openai");

        // Now primary should be in cooldown
        let entries = pool.entries_snapshot();
        let entry = &entries[0];
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
        assert_eq!(
            config.cloud_kind,
            Some(oneai_core::CloudProviderKind::Anthropic)
        );
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

    // ─── Degradation wiring tests ─────────────────────────────────────────────

    /// Primary model fails → within-family degradation recovers on the next
    /// tier (Opus → Sonnet), no cross-provider fallback needed.
    #[tokio::test]
    async fn degradation_recovers_within_family_when_primary_fails() {
        // Primary: an Anthropic-family Opus provider that always fails.
        let primary = TestProvider::new("anthropic", "claude-opus-4-8");
        primary.set_failing(true);

        let pool = ProviderPool::new(
            vec![ProviderEntry::new("anthropic", Arc::new(primary), 0)],
            ProviderPoolConfig::default().with_default_degradation(),
        )
        // Degraded providers are built by the factory from the swapped config;
        // inject a builder that returns a succeeding TestProvider so the chain
        // recovers deterministically without real HTTP.
        .with_provider_builder(|cfg: &ModelConfig| {
            Box::new(TestProvider::from_config(cfg.clone()))
        });

        let response = pool
            .infer(test_request())
            .await
            .expect("degradation should recover");
        assert_eq!(response.model, "claude-sonnet-4-6-20250514");
        assert_eq!(pool.active_provider_name(), "anthropic");

        // Exactly one fallback event — ModelDegradation Opus → Sonnet.
        assert_eq!(pool.fallback_log_count(), 1);
        let event = &pool.fallback_log_recent(1)[0];
        assert_eq!(event.from_provider, "anthropic");
        assert_eq!(event.to_provider, "anthropic");
        assert_eq!(
            event.reason,
            FallbackReason::ModelDegradation {
                from_model: "claude-opus-4-8".to_string(),
                to_model: "claude-sonnet-4-6-20250514".to_string(),
            }
        );
        assert_eq!(event.model_before, "claude-opus-4-8");
        assert_eq!(event.model_after, "claude-sonnet-4-6-20250514");
    }

    /// Primary fails AND every degraded tier also fails → fall through to the
    /// next cross-provider (no spurious recovery, ModelDegradation not logged).
    #[tokio::test]
    async fn degradation_falls_through_to_cross_provider_when_chain_exhausted() {
        let primary = TestProvider::new("anthropic", "claude-opus-4-8");
        primary.set_failing(true);
        let secondary = openai_test_provider();

        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("anthropic", Arc::new(primary), 0),
                ProviderEntry::new("openai", secondary, 1),
            ],
            ProviderPoolConfig::default().with_default_degradation(),
        )
        // Every degraded tier also fails → degradation exhausts the chain.
        .with_provider_builder(|cfg: &ModelConfig| {
            Box::new(TestProvider::from_config_failing(
                cfg.clone(),
                "degraded tier failed",
            ))
        });

        let response = pool
            .infer(test_request())
            .await
            .expect("cross-provider fallback should recover");
        assert_eq!(response.model, "gpt-4o");
        assert_eq!(pool.active_provider_name(), "openai");

        // No ModelDegradation event — degraded attempts all failed, so only the
        // cross-provider ProviderError event is logged.
        assert_eq!(pool.fallback_log_count(), 1);
        let event = &pool.fallback_log_recent(1)[0];
        assert!(matches!(event.reason, FallbackReason::ProviderError(_)));
        assert_eq!(event.from_provider, "anthropic");
        assert_eq!(event.to_provider, "openai");
    }

    /// Streaming counterpart — primary stream setup fails, degraded tier opens.
    #[tokio::test]
    async fn degradation_stream_recovers_within_family_when_primary_fails() {
        let primary = TestProvider::new("anthropic", "claude-opus-4-8");
        primary.set_failing(true);

        let pool = ProviderPool::new(
            vec![ProviderEntry::new("anthropic", Arc::new(primary), 0)],
            ProviderPoolConfig::default().with_default_degradation(),
        )
        .with_provider_builder(|cfg: &ModelConfig| {
            Box::new(TestProvider::from_config(cfg.clone()))
        });

        use futures::StreamExt;
        let mut stream = pool
            .infer_stream(test_request())
            .await
            .expect("degraded stream should open");
        // Drain to completion to ensure the degraded stream is live.
        while stream.next().await.is_some() {}

        assert_eq!(pool.active_provider_name(), "anthropic");
        assert_eq!(pool.fallback_log_count(), 1);
        assert!(matches!(
            pool.fallback_log_recent(1)[0].reason,
            FallbackReason::ModelDegradation { .. }
        ));
    }

    // ── Live mutation (provider/* RPCs) ──────────────────────────────────────

    fn two_entry_pool() -> ProviderPool {
        let a = ProviderEntry::new(
            "openai",
            Arc::new(TestProvider::from_config(ModelConfig {
                model_name: Some("gpt-4".into()),
                ..openai_cfg()
            })),
            0,
        );
        let b = ProviderEntry::new(
            "anthropic",
            Arc::new(TestProvider::from_config(ModelConfig {
                model_name: Some("claude".into()),
                ..anthropic_cfg()
            })),
            1,
        );
        ProviderPool::new(vec![a, b], ProviderPoolConfig::default())
    }

    fn openai_cfg() -> ModelConfig {
        ModelConfig {
            provider_type: oneai_core::ProviderType::Cloud,
            cloud_kind: Some(oneai_core::CloudProviderKind::OpenAI),
            api_key: Some("k".into()),
            base_url: None,
            port: None,
            model_name: Some("gpt-4".into()),
            model_path: None,
            extra: std::collections::HashMap::new(),
        }
    }

    fn anthropic_cfg() -> ModelConfig {
        ModelConfig {
            provider_type: oneai_core::ProviderType::Cloud,
            cloud_kind: Some(oneai_core::CloudProviderKind::Anthropic),
            api_key: Some("k".into()),
            base_url: None,
            port: None,
            model_name: Some("claude".into()),
            model_path: None,
            extra: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn set_active_by_name_live_switches() {
        let pool = two_entry_pool();
        assert_eq!(pool.active_provider_name(), "openai");
        pool.set_active_by_name("anthropic").expect("found");
        assert_eq!(pool.active_provider_name(), "anthropic");
        assert_eq!(pool.active_model_name(), "claude");
        // Unknown name errors.
        assert!(pool.set_active_by_name("nope").is_err());
    }

    #[tokio::test]
    async fn add_entry_is_live_and_replaceable() {
        let pool = two_entry_pool();
        assert_eq!(pool.provider_names().len(), 2);
        let third = ProviderEntry::new(
            "ollama",
            Arc::new(TestProvider::from_config(ModelConfig {
                model_name: Some("llama".into()),
                ..openai_cfg()
            })),
            2,
        );
        pool.add_entry(third);
        assert_eq!(pool.provider_names().len(), 3);
        assert!(pool.provider_names().contains(&"ollama".to_string()));
        // Adding a same-name entry replaces (not appends).
        let dup = ProviderEntry::new(
            "ollama",
            Arc::new(TestProvider::from_config(ModelConfig {
                model_name: Some("llama3".into()),
                ..openai_cfg()
            })),
            3,
        );
        pool.add_entry(dup);
        assert_eq!(pool.provider_names().len(), 3);
        // The new entry is selectable live.
        pool.set_active_by_name("ollama").expect("selectable");
        assert_eq!(pool.active_model_name(), "llama3");
    }

    #[tokio::test]
    async fn remove_entry_adjusts_active_index() {
        let pool = two_entry_pool();
        pool.set_active_by_name("anthropic").expect("switch");
        assert_eq!(pool.active_provider_name(), "anthropic");
        // Remove the active entry → falls back to index 0 (openai).
        pool.remove_entry("anthropic");
        assert_eq!(pool.provider_names().len(), 1);
        assert_eq!(pool.active_provider_name(), "openai");
    }

    /// `replace_entry` swaps the provider in place while keeping priority (and
    /// therefore position) — the backing for `provider/update`/`set_model`.
    #[tokio::test]
    async fn replace_entry_preserves_priority_and_position() {
        let pool = two_entry_pool();
        // anthropic is at priority 1 (index 1); replace it with a new model.
        let replaced = TestProvider::from_config(ModelConfig {
            model_name: Some("claude-opus-4-8".into()),
            ..anthropic_cfg()
        });
        assert!(pool.replace_entry("anthropic", Arc::new(replaced)));
        // Position + priority preserved: openai (0) still first, anthropic (1) second.
        assert_eq!(pool.provider_names(), vec!["openai", "anthropic"]);
        assert_eq!(pool.active_model_name(), "gpt-4"); // active still openai
                                                       // Selecting the replaced entry now reports the new model.
        pool.set_active_by_name("anthropic").expect("selectable");
        assert_eq!(pool.active_model_name(), "claude-opus-4-8");
        // Unknown name → false.
        assert!(!pool.replace_entry("nope", openai_test_provider()));
    }

    // ── issue #37: a manual `set_active` must survive inference ────────────
    //
    // Regression: without a smart router the fallback chain used to lead with
    // priority-order entry 0 unconditionally, and the success handler snapped
    // `active_index` back there — so a provider selected via `provider/
    // set_active` reverted after the first inference.

    /// `two_entry_pool` has openai at priority/index 0 and anthropic at 1.
    /// Selecting anthropic then inferring must keep anthropic active (and the
    /// response must actually come from it), not snap back to openai.
    #[tokio::test]
    async fn set_active_survives_inference() {
        let pool = two_entry_pool();
        assert_eq!(pool.active_provider_name(), "openai");
        pool.set_active_by_name("anthropic").expect("switch");
        assert_eq!(pool.active_provider_name(), "anthropic");

        let response = pool.infer(test_request()).await.unwrap();
        // Active stays on the manually-selected provider…
        assert_eq!(pool.active_provider_name(), "anthropic");
        // …and the inference really ran on it (anthropic's model is "claude").
        assert_eq!(response.model, "claude");
        // No fallback happened — the active provider answered first try.
        assert_eq!(pool.fallback_log_count(), 0);
    }

    /// Streaming counterpart of the same guarantee.
    #[tokio::test]
    async fn set_active_survives_streaming_inference() {
        use futures::StreamExt;
        let pool = two_entry_pool();
        pool.set_active_by_name("anthropic").expect("switch");

        let mut stream = pool.infer_stream(test_request()).await.unwrap();
        while stream.next().await.is_some() {}
        assert_eq!(pool.active_provider_name(), "anthropic");
        assert_eq!(pool.fallback_log_count(), 0);
    }

    /// Fallback is preserved: if the manually-selected active provider fails,
    /// inference still falls back to the next entry and tracks the survivor.
    #[tokio::test]
    async fn set_active_provider_failing_still_falls_back() {
        // Active entry "anthropic" fails; "openai" (index 0) is the fallback.
        let failing = TestProvider::new("anthropic", "claude");
        failing.set_failing(true);
        let pool = ProviderPool::new(
            vec![
                ProviderEntry::new("openai", openai_test_provider(), 0),
                ProviderEntry::new("anthropic", Arc::new(failing), 1),
            ],
            ProviderPoolConfig::default(),
        );
        pool.set_active_by_name("anthropic").expect("switch");
        assert_eq!(pool.active_provider_name(), "anthropic");

        let response = pool.infer(test_request()).await.unwrap();
        // Fell back to openai and tracks it now.
        assert_eq!(response.model, "gpt-4o");
        assert_eq!(pool.active_provider_name(), "openai");
        assert_eq!(pool.fallback_log_count(), 1);
    }
}
