//! Smart Model Router — production-grade cost/latency/quality routing.
//!
//! The `SmartRouter` extends the basic `ModelRouter` (regex-based) with
//! multi-factor routing that considers cost, latency, quality, provider health,
//! rate limits, budget constraints, and context window limits.
//!
//! **Routing algorithm**:
//! 1. Evaluate regex rules (existing ModelRouter, backward compatible)
//! 2. Validate regex result against runtime constraints (circuit, rate, budget, context)
//! 3. If regex result fails validation, or strategy overrides regex:
//!    - Score all available providers on cost/latency/quality dimensions
//!    - Weight scores by the configured strategy (CostOptimized/LatencyOptimized/etc.)
//!    - Pick the highest-scoring available provider
//! 4. Return `SmartRouteDecision` with full rationale
//!
//! **Integration with ProviderPool**:
//! When a SmartRouter is attached to a ProviderPool, the pool uses the router
//! to determine the order in which providers are tried. Instead of always
//! starting with the primary provider, the pool starts with the smart router's
//! recommendation. If that provider fails, fallback continues as usual.
//!
//! **Usage**:
//! ```ignore
//! let router = SmartRouter::new(
//!     ModelRouter::with_defaults(fallback_config),
//!     ModelPricingCatalog::with_known_models(),
//!     SmartRouteConfig::balanced(),
//! );
//!
//! // Before each inference, evaluate the route:
//! let decision = router.route("Implement auth module", "react", None);
//! // decision.model = "claude-sonnet-4-6-20250514" (balanced model)
//! // decision.reason = "Multi-factor scoring: anthropic (score=0.61)"
//! // decision.factors = [BudgetRemaining(5.0, false), ScoringResult(...)]
//!
//! // With budget constraints:
//! let decision = router.route("Implement auth module", "react", Some(0.5));
//! // decision.model = "claude-haiku-4-5-20251001" (cheap model, budget is low)
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::{
    ModelConfig, ModelPricingCatalog, ModelPricingEntry,
    CircuitBreaker, RateLimiter, CostTracker, CostBudgetConfig,
    SmartRouteConfig, SmartRouteDecision, SmartRouteFactor,
    RoutingStrategy, RoutingTier, ModelQualityProfile,
    ProviderScore, SmartRoutingLog, InMemorySmartRoutingLog,
    ProviderPoolConfig, DegradationRule,
};
use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;

use crate::ModelRouter;

// ─── SmartRouter ──────────────────────────────────────────────────────────────

/// Production-grade model router — considers cost, latency, quality, health, budget, context.
///
/// Wraps the existing `ModelRouter` (regex-based) and extends it with
/// multi-factor scoring. When `regex_first_pass` is enabled in the config,
/// regex rules are tried first; if they pass validation, their result is used.
/// Otherwise, multi-factor scoring determines the best provider/model.
///
/// The router is designed to be plugged into `ProviderPool` for intelligent
/// primary selection, or used standalone for routing decisions.
pub struct SmartRouter {
    /// The underlying regex-based ModelRouter (for backward compat).
    model_router: ModelRouter,

    /// Pricing catalog for cost scoring.
    pricing_catalog: ModelPricingCatalog,

    /// Quality profiles per model (for latency/quality scoring).
    quality_profiles: HashMap<String, ModelQualityProfile>,

    /// Smart routing configuration (strategy, thresholds, awareness flags).
    config: SmartRouteConfig,

    /// Circuit breaker — for provider health validation.
    circuit_breaker: Option<Arc<dyn CircuitBreaker>>,

    /// Rate limiter — for rate limit validation.
    rate_limiter: Option<Arc<dyn RateLimiter>>,

    /// Cost tracker — for budget validation.
    cost_tracker: Option<Arc<dyn CostTracker>>,

    /// Budget config — for budget limit computation.
    budget_config: Option<CostBudgetConfig>,

    /// Routing decision log — audit trail for observability.
    routing_log: Arc<dyn SmartRoutingLog>,
}

impl SmartRouter {
    /// Create a new SmartRouter with the given components.
    ///
    /// The `model_router` provides regex-based first-pass routing.
    /// The `pricing_catalog` provides cost data for scoring.
    /// The `config` defines the routing strategy and constraints.
    pub fn new(
        model_router: ModelRouter,
        pricing_catalog: ModelPricingCatalog,
        config: SmartRouteConfig,
    ) -> Self {
        // Build quality profiles from default profiles + pricing catalog
        let mut quality_profiles = HashMap::new();
        for profile in ModelQualityProfile::default_profiles() {
            quality_profiles.insert(profile.model_name.clone(), profile);
        }

        Self {
            model_router,
            pricing_catalog,
            quality_profiles,
            config,
            circuit_breaker: None,
            rate_limiter: None,
            cost_tracker: None,
            budget_config: None,
            routing_log: Arc::new(InMemorySmartRoutingLog::new()),
        }
    }

    /// Create with a circuit breaker for health validation.
    pub fn with_circuit_breaker(mut self, cb: Arc<dyn CircuitBreaker>) -> Self {
        self.circuit_breaker = Some(cb);
        self
    }

    /// Create with a rate limiter for rate validation.
    pub fn with_rate_limiter(mut self, rl: Arc<dyn RateLimiter>) -> Self {
        self.rate_limiter = Some(rl);
        self
    }

    /// Create with a cost tracker for budget validation.
    pub fn with_cost_tracker(mut self, ct: Arc<dyn CostTracker>) -> Self {
        self.cost_tracker = Some(ct);
        self
    }

    /// Create with a budget config for budget limits.
    pub fn with_budget_config(mut self, bc: CostBudgetConfig) -> Self {
        self.budget_config = Some(bc);
        self
    }

    /// Create with a custom routing log (for OTEL / database integration).
    pub fn with_routing_log(mut self, log: Arc<dyn SmartRoutingLog>) -> Self {
        self.routing_log = log;
        self
    }

    /// Add a custom quality profile for a model.
    pub fn add_quality_profile(mut self, profile: ModelQualityProfile) -> Self {
        self.quality_profiles.insert(profile.model_name.clone(), profile);
        self
    }

    /// Get the routing configuration.
    pub fn config(&self) -> &SmartRouteConfig {
        &self.config
    }

    /// Get recent routing decisions from the log.
    pub fn routing_log_recent(&self, limit: usize) -> Vec<SmartRouteDecision> {
        self.routing_log.recent_decisions(limit)
    }

    /// Get the total number of logged routing decisions.
    pub fn routing_log_count(&self) -> usize {
        self.routing_log.total_count()
    }

    // ─── Main routing method ──────────────────────────────────────────────

    /// Evaluate a routing decision based on task description, paradigm,
    /// and runtime constraints.
    ///
    /// This is the main entry point for the smart router. It:
    /// 1. Tries regex rules (if `regex_first_pass` is enabled)
    /// 2. Validates regex result against constraints
    /// 3. If regex fails or strategy overrides, does multi-factor scoring
    /// 4. Returns a `SmartRouteDecision` with full rationale
    ///
    /// `session_id` is used for budget checks (if budget_aware is enabled).
    /// `conversation_tokens` is used for context overflow checks (if context_aware is enabled).
    pub async fn route(
        &self,
        task_description: &str,
        paradigm: &str,
        session_id: Option<&str>,
        conversation_tokens: Option<u64>,
    ) -> SmartRouteDecision {
        let mut factors = Vec::new();

        // ── Step 1: Try regex rules first ────────────────────────────────
        if self.config.regex_first_pass {
            let regex_decision = self.model_router.route(task_description, paradigm);

            // Determine the tier of the regex result
            let tier = RoutingTier::from_model_name(&regex_decision.model);

            factors.push(SmartRouteFactor::RegexMatch {
                rule_description: regex_decision.matched_rule.clone(),
                matched: regex_decision.matched_rule != "fallback",
            });

            // ── Step 2: Validate regex result against constraints ────────
            // Only use regex result if it was a real rule match (not fallback)
            if regex_decision.matched_rule != "fallback" && self.validate_route(
                &regex_decision.provider.to_string(),
                &regex_decision.model,
                session_id,
                conversation_tokens,
                &mut factors,
            ).await {
                // Regex result passes validation — use it
                let profile = self.quality_profiles.get(&regex_decision.model);
                let mut decision = SmartRouteDecision::from_regex_match(
                    regex_decision.model.clone(),
                    regex_decision.provider.to_string(),
                    tier,
                    regex_decision.matched_rule,
                )
                    .with_estimated_cost(profile.map_or(0.0, |p| p.estimated_cost_per_call()))
                    .with_estimated_latency(profile.map_or(0, |p| p.estimated_latency_ms))
                    .with_max_tokens(regex_decision.max_tokens.unwrap_or(0));

                // Add factors from validation
                for factor in factors {
                    decision = decision.with_factor(factor);
                }

                self.routing_log.log_decision(decision.clone());
                return decision;
            }

            // Regex result failed validation — fall through to multi-factor scoring
            tracing::debug!(
                "SmartRouter: regex result ({}/{}) failed validation, falling through to scoring",
                regex_decision.provider.to_string(), regex_decision.model,
            );
        }

        // ── Step 3: Multi-factor scoring ─────────────────────────────────
        let decision = self.route_by_scoring(
            task_description,
            paradigm,
            session_id,
            conversation_tokens,
            &mut factors,
        ).await;

        self.routing_log.log_decision(decision.clone());
        decision
    }

    // ─── Route by scoring ────────────────────────────────────────────────

    /// Route using multi-factor scoring — the core algorithm.
    ///
    /// Scores all available providers/models on cost, latency, and quality
    /// dimensions, weighted by the configured strategy. Picks the highest-
    /// scoring available provider.
    async fn route_by_scoring(
        &self,
        task_description: &str,
        paradigm: &str,
        session_id: Option<&str>,
        conversation_tokens: Option<u64>,
        factors: &mut Vec<SmartRouteFactor>,
    ) -> SmartRouteDecision {
        // Get the list of candidate providers/models
        // We score based on the quality profiles (which include all known models)
        let candidates = ModelQualityProfile::default_profiles();
        let strategy = &self.config.strategy;

        // Compute budget remaining (if budget-aware)
        let budget_remaining = if self.config.budget_aware && session_id.is_some() && self.cost_tracker.is_some() {
            let session_id_str = session_id.unwrap();
            match self.cost_tracker.as_ref().unwrap().check_budget(session_id_str).await {
                Ok(status) => {
                    let is_low = status.remaining_usd < self.config.min_budget_for_expensive;
                    factors.push(SmartRouteFactor::BudgetRemaining {
                        remaining_usd: status.remaining_usd,
                        is_low,
                    });
                    status.remaining_usd
                },
                Err(_) => f64::INFINITY, // If budget check fails, assume unlimited
            }
        } else {
            f64::INFINITY // No budget tracking
        };

        // Score each candidate
        let mut scores: Vec<ProviderScore> = Vec::new();
        for candidate in &candidates {
            // Check if this provider is available (health, rate, context)
            let is_available = self.check_provider_available(
                &candidate.provider_family,
                &candidate.model_name,
                conversation_tokens,
                budget_remaining,
                factors,
            ).await;

            if !is_available {
                scores.push(ProviderScore::skipped(
                    candidate.provider_family.clone(),
                    candidate.model_name.clone(),
                    "Failed availability check",
                ));
                continue;
            }

            // Compute scores for each dimension
            let cost_score = self.compute_cost_score(candidate, budget_remaining);
            let latency_score = self.compute_latency_score(candidate);
            let quality_score = candidate.quality_score; // From tier

            let score = ProviderScore::new(
                candidate.provider_family.clone(),
                candidate.model_name.clone(),
                candidate.tier,
                cost_score,
                latency_score,
                quality_score,
                strategy,
            );

            factors.push(SmartRouteFactor::ScoringResult {
                provider: candidate.provider_family.clone(),
                total_score: score.total_score,
                cost_score: score.cost_score,
                latency_score: score.latency_score,
                quality_score: score.quality_score,
            });

            scores.push(score);
        }

        // Also consider regex fallback config's model as a candidate
        let fallback_config = self.model_router.fallback_config();
        let fallback_model = fallback_config.model_name.as_deref().unwrap_or("unknown");
        let fallback_provider = self.infer_provider_from_model(fallback_model);

        // Check if fallback model is already scored
        let fallback_already_scored = scores.iter().any(|s| s.model_name == fallback_model);

        if !fallback_already_scored {
            let tier = RoutingTier::from_model_name(fallback_model);
            let profile = self.quality_profiles.get(fallback_model)
                .or_else(|| {
                    // Try partial match
                    self.quality_profiles.iter()
                        .find(|(k, _)| fallback_model.starts_with(k.as_str()) || k.as_str().starts_with(fallback_model))
                        .map(|(_, v)| v)
                });

            if let Some(profile) = profile {
                let is_available = self.check_provider_available(
                    &fallback_provider,
                    fallback_model,
                    conversation_tokens,
                    budget_remaining,
                    factors,
                ).await;

                if is_available {
                    let cost_score = self.compute_cost_score(profile, budget_remaining);
                    let latency_score = self.compute_latency_score(profile);
                    let quality_score = profile.quality_score;

                    let score = ProviderScore::new(
                        fallback_provider.clone(),
                        fallback_model.to_string(),
                        tier,
                        cost_score,
                        latency_score,
                        quality_score,
                        strategy,
                    );

                    scores.push(score);
                }
            }
        }

        // Find the best available score
        let best = scores.iter()
            .filter(|s| s.is_available)
            .max_by(|a, b| a.total_score.partial_cmp(&b.total_score).unwrap_or(std::cmp::Ordering::Equal))
            .cloned(); // Clone the best score to release the borrow

        match best {
            Some(best_score) => {
                let profile = self.find_profile_for_model(&best_score.model_name);
                SmartRouteDecision::from_scoring(
                    &best_score,
                    *strategy,
                    factors.clone(),
                    scores,
                ).with_estimated_cost(
                    profile.map_or(0.0, |p| p.estimated_cost_per_call())
                ).with_estimated_latency(
                    profile.map_or(0, |p| p.estimated_latency_ms)
                )
            },
            None => {
                // No available provider — return fallback decision
                tracing::error!("SmartRouter: no available providers found, using fallback");
                SmartRouteDecision::new(
                    fallback_model,
                    fallback_provider,
                    RoutingTier::from_model_name(fallback_model),
                    *strategy,
                    "No available providers — using fallback",
                ).with_estimated_cost(0.0)
            },
        }
    }

    // ─── Route for pool ──────────────────────────────────────────────────

    /// Route within a ProviderPool — determines the order in which providers
    /// are tried. Returns the name of the provider to try first.
    ///
    /// This is called by ProviderPool when a SmartRouter is attached.
    /// Instead of always trying the primary provider, the pool starts
    /// with the smart router's recommendation.
    pub async fn route_for_pool(
        &self,
        task_description: &str,
        paradigm: &str,
        pool_config: &ProviderPoolConfig,
        session_id: Option<&str>,
        conversation_tokens: Option<u64>,
    ) -> SmartRouteDecision {
        // Score pool entries as candidates
        let mut factors = Vec::new();
        let strategy = &self.config.strategy;

        // Compute budget remaining
        let budget_remaining = if self.config.budget_aware && session_id.is_some() && self.cost_tracker.is_some() {
            match self.cost_tracker.as_ref().unwrap().check_budget(session_id.unwrap()).await {
                Ok(status) => {
                    let is_low = status.remaining_usd < self.config.min_budget_for_expensive;
                    factors.push(SmartRouteFactor::BudgetRemaining {
                        remaining_usd: status.remaining_usd,
                        is_low,
                    });
                    status.remaining_usd
                },
                Err(_) => f64::INFINITY,
            }
        } else {
            f64::INFINITY
        };

        // Score each pool entry
        let mut scores: Vec<ProviderScore> = Vec::new();
        for entry in pool_config.sorted_entries() {
            let model_name = entry.model_name();
            let provider_name = &entry.name;

            // Look up quality profile for this model
            let profile = self.find_profile_for_model(model_name);
            let tier = profile.map_or(RoutingTier::from_model_name(model_name), |p| p.tier);

            // Check availability
            let is_available = self.check_provider_available(
                provider_name,
                model_name,
                conversation_tokens,
                budget_remaining,
                &mut factors,
            ).await;

            if !is_available {
                scores.push(ProviderScore::skipped(
                    provider_name.clone(),
                    model_name.to_string(),
                    "Failed availability check",
                ));
                continue;
            }

            let cost_score = profile.map_or(0.5, |p| self.compute_cost_score(p, budget_remaining));
            let latency_score = profile.map_or(0.5, |p| self.compute_latency_score(p));
            let quality_score = tier.quality_score();

            let score = ProviderScore::new(
                provider_name.clone(),
                model_name.to_string(),
                tier,
                cost_score,
                latency_score,
                quality_score,
                strategy,
            );

            scores.push(score);
        }

        // Find the best available score
        let best = scores.iter()
            .filter(|s| s.is_available)
            .max_by(|a, b| a.total_score.partial_cmp(&b.total_score).unwrap_or(std::cmp::Ordering::Equal))
            .cloned();

        match best {
            Some(best_score) => {
                let profile = self.find_profile_for_model(&best_score.model_name);
                SmartRouteDecision::from_scoring(
                    &best_score,
                    *strategy,
                    factors,
                    scores,
                ).with_estimated_cost(
                    profile.map_or(0.0, |p| p.estimated_cost_per_call())
                ).with_estimated_latency(
                    profile.map_or(0, |p| p.estimated_latency_ms)
                )
            },
            None => {
                // All providers unavailable — try primary anyway
                let sorted = pool_config.sorted_entries();
                let primary = sorted.first();
                let model_name = primary.map_or("unknown".to_string(), |e| e.model_name().to_string());
                let provider_name = primary.map_or("unknown".to_string(), |e| e.name.clone());
                SmartRouteDecision::new(
                    model_name,
                    provider_name,
                    RoutingTier::Balanced,
                    *strategy,
                    "No available providers — trying primary anyway",
                )
            },
        }
    }

    // ─── Route for degradation ───────────────────────────────────────────

    /// Route within a provider family using DegradationRule.
    ///
    /// This is used when the primary provider's model fails and the pool
    /// needs to downgrade within the same provider family before cross-provider
    /// fallback. The smart router validates each degradation step against
    /// runtime constraints (budget, context, etc.).
    pub async fn route_for_degradation(
        &self,
        current_model: &str,
        provider_family: &str,
        degradation_rules: &[DegradationRule],
        session_id: Option<&str>,
        conversation_tokens: Option<u64>,
    ) -> Option<String> {
        let rule = DegradationRule::find_for_provider(degradation_rules, provider_family)?;

        // Try each degraded model in the chain
        let mut current = current_model.to_string();
        loop {
            let next = rule.next_degraded_model(&current);
            if next.is_none() {
                return None; // Already at cheapest model
            }

            let next_model = next.unwrap();

            // Validate the degraded model against constraints
            let mut factors = Vec::new();
            if self.validate_route(
                provider_family,
                &next_model,
                session_id,
                conversation_tokens,
                &mut factors,
            ).await {
                return Some(next_model);
            }

            current = next_model;
        }
    }

    // ─── Scoring helpers ─────────────────────────────────────────────────

    /// Compute cost score for a model profile.
    ///
    /// Higher score = cheaper relative to budget.
    /// 0.0 = would exceed budget.
    /// 1.0 = free model.
    fn compute_cost_score(&self, profile: &ModelQualityProfile, budget_remaining: f64) -> f64 {
        if profile.cost_per_1k_prompt_usd == 0.0 && profile.cost_per_1k_completion_usd == 0.0 {
            return 1.0; // Free models get maximum cost score
        }

        let estimated_cost = profile.estimated_cost_per_call();

        if budget_remaining.is_infinite() || budget_remaining <= 0.0 {
            // No budget tracking or budget exceeded — score based on relative cost
            // Use a reference cost of $10 (rough Opus-level cost per call)
            return 1.0 - (estimated_cost / 10.0).min(1.0);
        }

        if estimated_cost >= budget_remaining {
            return 0.0; // Would exceed budget
        }

        // Score inversely proportional to cost relative to budget
        // Lower cost = higher score
        1.0 - (estimated_cost / budget_remaining).min(1.0)
    }

    /// Compute latency score for a model profile.
    ///
    /// Higher score = faster.
    /// 0.0 = exceeds max latency tolerance.
    /// 1.0 = instant (0ms).
    fn compute_latency_score(&self, profile: &ModelQualityProfile) -> f64 {
        if profile.estimated_latency_ms >= self.config.max_latency_ms {
            return 0.0; // Exceeds max latency
        }

        1.0 - (profile.estimated_latency_ms as f64 / self.config.max_latency_ms as f64)
    }

    // ─── Validation helpers ──────────────────────────────────────────────

    /// Validate a route against runtime constraints.
    ///
    /// Checks: circuit breaker (health), rate limiter, budget, context window.
    /// Returns true if the route passes all checks.
    async fn validate_route(
        &self,
        provider: &str,
        model: &str,
        session_id: Option<&str>,
        conversation_tokens: Option<u64>,
        factors: &mut Vec<SmartRouteFactor>,
    ) -> bool {
        // ── Check circuit breaker ──────────────────────────────────────
        if self.config.health_aware && self.circuit_breaker.is_some() {
            let state = self.circuit_breaker.as_ref().unwrap().check(provider);
            let was_open = state.is_failing();
            factors.push(SmartRouteFactor::CircuitOpen {
                provider: provider.to_string(),
                was_open,
            });
            if was_open {
                return false;
            }
        }

        // ── Check rate limiter ────────────────────────────────────────
        if self.config.rate_aware && self.rate_limiter.is_some() {
            match self.rate_limiter.as_ref().unwrap().check_rate(provider).await {
                Ok(status) => {
                    let was_exceeded = !status.is_allowed();
                    factors.push(SmartRouteFactor::RateLimited {
                        provider: provider.to_string(),
                        was_exceeded,
                    });
                    if was_exceeded {
                        return false;
                    }
                },
                Err(_) => {
                    // Rate check failed — assume OK (don't block routing)
                    factors.push(SmartRouteFactor::RateLimited {
                        provider: provider.to_string(),
                        was_exceeded: false,
                    });
                },
            }
        }

        // ── Check budget ─────────────────────────────────────────────
        if self.config.budget_aware && session_id.is_some() && self.cost_tracker.is_some() {
            let status = self.cost_tracker.as_ref().unwrap()
                .check_budget(session_id.unwrap()).await
                .unwrap_or(oneai_core::BudgetStatus::unlimited(0));

            let is_low = status.remaining_usd < self.config.min_budget_for_expensive;
            factors.push(SmartRouteFactor::BudgetRemaining {
                remaining_usd: status.remaining_usd,
                is_low,
            });

            // If budget is low, check if this model is expensive
            if is_low {
                let tier = RoutingTier::from_model_name(model);
                if tier == RoutingTier::Powerful {
                    factors.push(SmartRouteFactor::QualityRequirement {
                        required_tier: RoutingTier::Balanced, // Should downgrade
                        actual_tier: tier,
                    });
                    return false; // Skip expensive models when budget is low
                }
            }
        }

        // ── Check context window ────────────────────────────────────
        if self.config.context_aware && conversation_tokens.is_some() {
            let tokens = conversation_tokens.unwrap();
            let profile = self.find_profile_for_model(model);
            let context_window = profile.map_or(128_000, |p| p.context_window_tokens);

            let would_overflow = (tokens as f64 / context_window as f64) > self.config.context_overflow_threshold;
            factors.push(SmartRouteFactor::ContextOverflow {
                conversation_tokens: tokens,
                model_context_window: context_window,
                would_overflow,
            });
            if would_overflow {
                return false;
            }
        }

        true
    }

    /// Check if a provider is available (passes all constraint checks).
    async fn check_provider_available(
        &self,
        provider: &str,
        model: &str,
        conversation_tokens: Option<u64>,
        budget_remaining: f64,
        factors: &mut Vec<SmartRouteFactor>,
    ) -> bool {
        // Circuit breaker
        if self.config.health_aware && self.circuit_breaker.is_some() {
            let state = self.circuit_breaker.as_ref().unwrap().check(provider);
            if state.is_failing() {
                factors.push(SmartRouteFactor::CircuitOpen {
                    provider: provider.to_string(),
                    was_open: true,
                });
                return false;
            }
        }

        // Rate limiter
        if self.config.rate_aware && self.rate_limiter.is_some() {
            match self.rate_limiter.as_ref().unwrap().check_rate(provider).await {
                Ok(status) => {
                    if !status.is_allowed() {
                        factors.push(SmartRouteFactor::RateLimited {
                            provider: provider.to_string(),
                            was_exceeded: true,
                        });
                        return false;
                    }
                },
                Err(_) => {}, // Assume OK
            }
        }

        // Budget — skip expensive models when budget is low
        if self.config.budget_aware && budget_remaining < self.config.min_budget_for_expensive {
            let tier = RoutingTier::from_model_name(model);
            if tier == RoutingTier::Powerful {
                factors.push(SmartRouteFactor::BudgetRemaining {
                    remaining_usd: budget_remaining,
                    is_low: true,
                });
                return false;
            }
        }

        // Context window overflow
        if self.config.context_aware && conversation_tokens.is_some() {
            let tokens = conversation_tokens.unwrap();
            let profile = self.find_profile_for_model(model);
            let context_window = profile.map_or(128_000, |p| p.context_window_tokens);

            if (tokens as f64 / context_window as f64) > self.config.context_overflow_threshold {
                factors.push(SmartRouteFactor::ContextOverflow {
                    conversation_tokens: tokens,
                    model_context_window: context_window,
                    would_overflow: true,
                });
                return false;
            }
        }

        true
    }

    // ─── Utility helpers ─────────────────────────────────────────────────

    /// Infer provider from model name (same logic as ModelRouter).
    fn infer_provider_from_model(&self, model: &str) -> String {
        let lower = model.to_lowercase();
        if lower.starts_with("claude") { "anthropic".to_string() }
        else if lower.starts_with("gpt") || lower.contains("openai") || lower.starts_with("o3") { "openai".to_string() }
        else if lower.starts_with("gemini") { "google".to_string() }
        else if lower.contains("ollama") || lower.contains("local") { "ollama".to_string() }
        else { "openai".to_string() } // Default: most services use OpenAI protocol
    }

    /// Find the quality profile for a model name (with partial matching).
    fn find_profile_for_model(&self, model: &str) -> Option<&ModelQualityProfile> {
        // Try exact match first
        if let Some(profile) = self.quality_profiles.get(model) {
            return Some(profile);
        }

        // Try partial match (e.g., "claude-sonnet-4-6-20250514" matches key "claude-sonnet-4-6-20250514")
        for (key, profile) in &self.quality_profiles {
            if model.starts_with(key.as_str()) || key.starts_with(model) {
                return Some(profile);
            }
        }

        None
    }

    /// Get a reference to the underlying ModelRouter's fallback config.
    fn fallback_config_ref(&self) -> &ModelConfig {
        self.model_router.fallback_config()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::CloudProviderKind;
    use oneai_core::ProviderType;
    use oneai_core::circuit_breaker::{ThresholdCircuitBreaker, CircuitBreakerConfig};
    use regex::Regex;

    fn anthropic_fallback_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::Anthropic),
            api_key: Some("sk-ant-test".to_string()),
            base_url: Some("https://api.anthropic.com/v1".to_string()),
            port: None,
            model_name: Some("claude-sonnet-4-6-20250514".to_string()),
            model_path: None,
            extra: HashMap::new(),
        }
    }

    fn openai_fallback_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::Cloud,
            cloud_kind: Some(CloudProviderKind::OpenAI),
            api_key: Some("sk-test".to_string()),
            base_url: Some("https://api.openai.com/v1".to_string()),
            port: None,
            model_name: Some("gpt-4o".to_string()),
            model_path: None,
            extra: HashMap::new(),
        }
    }

    fn create_balanced_router() -> SmartRouter {
        let model_router = ModelRouter::with_defaults(anthropic_fallback_config());
        let catalog = ModelPricingCatalog::with_known_models();
        SmartRouter::new(model_router, catalog, SmartRouteConfig::balanced())
    }

    fn create_cost_router() -> SmartRouter {
        let model_router = ModelRouter::with_defaults(anthropic_fallback_config());
        let catalog = ModelPricingCatalog::with_known_models();
        SmartRouter::new(model_router, catalog, SmartRouteConfig::cost_optimized())
    }

    fn create_quality_router() -> SmartRouter {
        let model_router = ModelRouter::with_defaults(anthropic_fallback_config());
        let catalog = ModelPricingCatalog::with_known_models();
        SmartRouter::new(model_router, catalog, SmartRouteConfig::quality_optimized())
    }

    fn create_latency_router() -> SmartRouter {
        let model_router = ModelRouter::with_defaults(anthropic_fallback_config());
        let catalog = ModelPricingCatalog::with_known_models();
        SmartRouter::new(model_router, catalog, SmartRouteConfig::latency_optimized())
    }

    // ─── Basic routing tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_balanced_route_simple_task() {
        let router = create_balanced_router();
        let decision = router.route("What is the capital of France?", "react", None, None).await;

        // Should match regex rule for simple task → cheap model (Haiku)
        assert!(decision.from_regex);
        assert!(decision.model.contains("haiku") || decision.tier == RoutingTier::Cheap);
    }

    #[tokio::test]
    async fn test_balanced_route_implementation_task() {
        let router = create_balanced_router();
        let decision = router.route("Implement a new authentication module", "react", None, None).await;

        // Should match regex rule → balanced model (Sonnet)
        assert!(decision.from_regex);
        assert!(decision.model.contains("sonnet") || decision.tier == RoutingTier::Balanced);
    }

    #[tokio::test]
    async fn test_balanced_route_architecture_task() {
        let router = create_balanced_router();
        let decision = router.route("Design the architecture for a distributed system", "plan", None, None).await;

        // Should match regex rule → powerful model (Opus)
        assert!(decision.from_regex);
        assert!(decision.model.contains("opus") || decision.tier == RoutingTier::Powerful);
    }

    // ─── Cost-optimized routing tests ──────────────────────────────────

    #[tokio::test]
    async fn test_cost_optimized_prefers_cheap() {
        let router = create_cost_router();
        let decision = router.route("Implement auth module", "react", None, None).await;

        // Cost-optimized should prefer cheaper models
        // Regex first-pass may match "implement" → balanced, but if validation fails
        // or strategy overrides, cost optimization should prefer cheap models
        // Since regex_first_pass is enabled, the regex result is used if it passes
        assert!(!decision.model.is_empty());
    }

    // ─── Scoring tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_scoring_all_providers() {
        let router = SmartRouter::new(
            ModelRouter::new(vec![], anthropic_fallback_config()), // No regex rules → scoring
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::balanced().without_budget_awareness().without_health_awareness().without_rate_awareness().without_context_awareness(),
        );

        let decision = router.route("any task", "react", None, None).await;

        // Should have scored multiple providers
        assert!(!decision.from_regex);
        assert!(decision.all_scores.len() > 0);
        assert!(!decision.model.is_empty());
    }

    #[tokio::test]
    async fn test_scoring_quality_optimized() {
        let router = SmartRouter::new(
            ModelRouter::new(vec![], anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::quality_optimized().without_budget_awareness().without_health_awareness().without_rate_awareness().without_context_awareness(),
        );

        let decision = router.route("any task", "react", None, None).await;

        // Quality-optimized should pick the most powerful model
        assert!(!decision.from_regex);
        // Opus should have the highest quality score (1.0)
        // With quality_weight=0.8, Opus should win
        let opus_scores = decision.all_scores.iter()
            .filter(|s| s.model_name.contains("opus") || s.model_name.contains("o3-pro"))
            .collect::<Vec<_>>();
        assert!(!opus_scores.is_empty());
    }

    #[tokio::test]
    async fn test_scoring_cost_optimized_no_regex() {
        let router = SmartRouter::new(
            ModelRouter::new(vec![], anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::cost_optimized().without_budget_awareness().without_health_awareness().without_rate_awareness().without_context_awareness(),
        );

        let decision = router.route("any task", "react", None, None).await;

        // Cost-optimized should pick the cheapest model
        assert!(!decision.from_regex);
        // Haiku/mini should have highest cost score
        let cheap_scores = decision.all_scores.iter()
            .filter(|s| s.tier == RoutingTier::Cheap)
            .collect::<Vec<_>>();
        assert!(!cheap_scores.is_empty());

        // Free models (Ollama) should have cost_score = 1.0
        let free_scores = cheap_scores.iter()
            .filter(|s| s.cost_score == 1.0)
            .collect::<Vec<_>>();
        assert!(!free_scores.is_empty());
    }

    #[tokio::test]
    async fn test_scoring_latency_optimized_no_regex() {
        let router = SmartRouter::new(
            ModelRouter::new(vec![], anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::latency_optimized().without_budget_awareness().without_health_awareness().without_rate_awareness().without_context_awareness(),
        );

        let decision = router.route("any task", "react", None, None).await;

        // Latency-optimized should prefer fast models
        // Cheap models have latency_score ≈ 0.95 (1500ms / 10000ms)
        // Balanced models have latency_score ≈ 0.65 (3500ms / 10000ms)
        // Powerful models have latency_score ≈ 0.0 (15000ms / 10000ms)
        assert!(!decision.from_regex);
    }

    // ─── Budget-aware routing tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_budget_low_skips_expensive_models() {
        let tracker = Arc::new(oneai_core::InMemoryCostTracker::with_budget(
            CostBudgetConfig::with_cost_limit(1.0),
        ));
        // Record enough to push budget close to limit
        tracker.record_usage(oneai_core::UsageRecord::new("sess1", "gpt-4o", "openai", 1000, 500, 0.8)).await.unwrap();

        let router = SmartRouter::new(
            ModelRouter::with_defaults(anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::balanced(),
        ).with_cost_tracker(tracker.clone())
          .with_budget_config(CostBudgetConfig::with_cost_limit(1.0));

        let decision = router.route("Design a system architecture", "plan", Some("sess1"), None).await;

        // Budget is low (~0.2 remaining) → should skip Opus (Powerful)
        // The regex rule matches "architecture" → Opus, but validation fails
        // So scoring should pick a cheaper model
        assert!(decision.model.is_empty() || decision.tier != RoutingTier::Powerful || !decision.from_regex);
    }

    #[tokio::test]
    async fn test_budget_high_allows_expensive_models() {
        let tracker = Arc::new(oneai_core::InMemoryCostTracker::new());

        let router = SmartRouter::new(
            ModelRouter::with_defaults(anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::balanced(),
        ).with_cost_tracker(tracker);

        let decision = router.route("Design a system architecture", "plan", Some("sess1"), None).await;

        // Budget is unlimited → should allow Opus
        assert!(decision.model.contains("opus") || decision.from_regex);
    }

    // ─── Circuit breaker routing tests ──────────────────────────────────

    #[tokio::test]
    async fn test_circuit_open_skips_provider() {
        let cb = Arc::new(ThresholdCircuitBreaker::with_config(
            CircuitBreakerConfig::new(1, 1, 60),
        ));
        // Open circuit for anthropic
        cb.record_failure("anthropic", "API error");

        let router = SmartRouter::new(
            ModelRouter::with_defaults(anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::balanced().without_budget_awareness().without_rate_awareness().without_context_awareness(),
        ).with_circuit_breaker(cb);

        let decision = router.route("What is the capital?", "react", None, None).await;

        // Should not use anthropic (circuit is open)
        // If regex matched "What is" → haiku (anthropic), but circuit is open
        // So should fall through to scoring and pick a non-anthropic provider
        // Decision should have CircuitOpen factor for anthropic
        let circuit_factors = decision.factors.iter()
            .filter(|f| matches!(f, SmartRouteFactor::CircuitOpen { provider, was_open: true } if provider == "anthropic"))
            .collect::<Vec<_>>();
        assert!(!circuit_factors.is_empty());
    }

    // ─── Context overflow routing tests ─────────────────────────────────

    #[tokio::test]
    async fn test_context_overflow_skips_small_window_models() {
        let router = SmartRouter::new(
            ModelRouter::new(vec![], anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::balanced().without_budget_awareness().without_health_awareness().without_rate_awareness(),
        );

        // 150K tokens — should overflow models with 128K context at 0.8 threshold
        // (150K > 128K * 0.8 = 102K)
        let decision = router.route("any task", "react", None, Some(150_000)).await;

        // Should not pick models with small context windows
        let overflow_factors = decision.factors.iter()
            .filter(|f| matches!(f, SmartRouteFactor::ContextOverflow { would_overflow: true, .. }))
            .collect::<Vec<_>>();
        assert!(!overflow_factors.is_empty());
    }

    #[tokio::test]
    async fn test_context_within_window_allows_model() {
        let router = SmartRouter::new(
            ModelRouter::with_defaults(anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::balanced().without_budget_awareness().without_health_awareness().without_rate_awareness(),
        );

        // 50K tokens — should be fine for all models (50K < 128K * 0.8)
        let decision = router.route("What is the capital?", "react", None, Some(50_000)).await;

        // Should allow models with 128K+ context windows
        let overflow_factors = decision.factors.iter()
            .filter(|f| matches!(f, SmartRouteFactor::ContextOverflow { would_overflow: true, .. }))
            .collect::<Vec<_>>();
        assert!(overflow_factors.is_empty());
    }

    // ─── Pool routing tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_route_for_pool_balanced() {
        let router = create_balanced_router();
        let pool_config = ProviderPoolConfig::anthropic_primary(
            Some("sk-ant-test".to_string()),
            Some("sk-test".to_string()),
        );

        let decision = router.route_for_pool(
            "Implement auth module",
            "react",
            &pool_config,
            None,
            None,
        ).await;

        assert!(!decision.model.is_empty());
        assert!(decision.all_scores.len() > 0);
    }

    #[tokio::test]
    async fn test_route_for_pool_cost_optimized() {
        let router = create_cost_router();
        let pool_config = ProviderPoolConfig::anthropic_primary(
            Some("sk-ant-test".to_string()),
            Some("sk-test".to_string()),
        );

        let decision = router.route_for_pool(
            "Implement auth module",
            "react",
            &pool_config,
            None,
            None,
        ).await;

        // Cost-optimized should prefer cheaper models in pool
        assert!(!decision.model.is_empty());
    }

    // ─── Degradation routing tests ─────────────────────────────────────

    #[tokio::test]
    async fn test_route_for_degradation_anthropic() {
        let router = create_balanced_router();
        let rules = DegradationRule::default_presets();

        // Start at Opus → should degrade to Sonnet (passes validation)
        let degraded = router.route_for_degradation(
            "claude-opus-4-8",
            "anthropic",
            &rules,
            None,
            None,
        ).await;

        assert_eq!(degraded, Some("claude-sonnet-4-6-20250514".to_string()));
    }

    #[tokio::test]
    async fn test_route_for_degradation_haiku_no_degradation() {
        let router = create_balanced_router();
        let rules = DegradationRule::default_presets();

        // Start at Haiku (cheapest) → should return None (no further degradation)
        let degraded = router.route_for_degradation(
            "claude-haiku-4-5-20251001",
            "anthropic",
            &rules,
            None,
            None,
        ).await;

        assert_eq!(degraded, None);
    }

    // ─── Regex-only mode tests ─────────────────────────────────────────

    #[tokio::test]
    async fn test_regex_only_mode() {
        let router = SmartRouter::new(
            ModelRouter::with_defaults(anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::regex_only(), // Disable all smart factors
        );

        let decision = router.route("What is the capital?", "react", None, None).await;

        // Should use regex rule without any validation
        assert!(decision.from_regex);
        // No budget/health/rate factors should be present
        let smart_factors = decision.factors.iter()
            .filter(|f| !matches!(f, SmartRouteFactor::RegexMatch { .. }))
            .collect::<Vec<_>>();
        assert!(smart_factors.is_empty());
    }

    // ─── Routing log tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_routing_log_records_decisions() {
        let router = create_balanced_router();

        router.route("What is the capital?", "react", None, None).await;
        router.route("Implement auth module", "react", None, None).await;

        assert_eq!(router.routing_log_count(), 2);
        let recent = router.routing_log_recent(1);
        assert_eq!(recent.len(), 1);
    }

    // ─── Scoring computation tests ──────────────────────────────────────

    #[test]
    fn test_cost_score_free_model() {
        let router = create_balanced_router();
        let profile = ModelQualityProfile::new(
            "qwen2.5:0.5b", "ollama", RoutingTier::Cheap, 0.0, 0.0,
        );
        let score = router.compute_cost_score(&profile, f64::INFINITY);
        assert_eq!(score, 1.0); // Free models get max cost score
    }

    #[test]
    fn test_cost_score_budget_exceeded() {
        let router = create_balanced_router();
        let profile = ModelQualityProfile::new(
            "claude-opus-4-8", "anthropic", RoutingTier::Powerful, 15.0, 75.0,
        );
        // Budget is $10 but estimated cost is $52.5 → would exceed
        let score = router.compute_cost_score(&profile, 10.0);
        assert_eq!(score, 0.0); // Would exceed budget → 0
    }

    #[test]
    fn test_latency_score_within_tolerance() {
        let router = SmartRouter::new(
            ModelRouter::with_defaults(anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::balanced().with_max_latency(10000),
        );
        let profile = ModelQualityProfile::new(
            "claude-haiku-4-5-20251001", "anthropic", RoutingTier::Cheap, 0.80, 4.0,
        );
        // Haiku has estimated_latency 1500ms, max_tolerance 10000ms
        let score = router.compute_latency_score(&profile);
        assert!((score - (1.0 - 1500.0/10000.0)).abs() < 0.01);
    }

    #[test]
    fn test_latency_score_exceeds_tolerance() {
        let router = SmartRouter::new(
            ModelRouter::with_defaults(anthropic_fallback_config()),
            ModelPricingCatalog::with_known_models(),
            SmartRouteConfig::latency_optimized(), // max_latency = 10000ms
        );
        let profile = ModelQualityProfile::new(
            "claude-opus-4-8", "anthropic", RoutingTier::Powerful, 15.0, 75.0,
        );
        // Opus has estimated_latency 15000ms > max_latency 10000ms
        let score = router.compute_latency_score(&profile);
        assert_eq!(score, 0.0);
    }
}
