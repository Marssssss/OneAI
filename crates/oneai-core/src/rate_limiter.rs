//! Rate limiting for LLM provider API calls.
//!
//! Prevents exceeding provider API rate limits by tracking call frequency
//! and enforcing per-provider limits. When a rate limit is exceeded,
//! the caller must wait before making another request.
//!
//! Key concepts:
//! - `RateLimiter`: Trait for checking and recording call rates
//! - `TokenWindowRateLimiter`: Sliding window implementation (per minute + per hour)
//! - `RateLimitStatus`: Whether a call is allowed, remaining quota, reset time
//! - `RateLimitConfig`: Configuration for rate limits (global + per-provider overrides)

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ─── RateLimiter trait ───────────────────────────────────────────────────────

/// Trait for rate limiting provider API calls.
///
/// Implementations track call frequency per provider and enforce
/// rate limits. When a rate limit is exceeded, callers must wait
/// before making another request.
///
/// The default implementation is `TokenWindowRateLimiter` —
/// a sliding window rate limiter with per-minute and per-hour limits.
#[async_trait::async_trait]
pub trait RateLimiter: Send + Sync {
    /// Check whether a call to the given provider is allowed right now.
    ///
    /// Returns `RateLimitStatus` with whether the call is allowed,
    /// remaining quota, and when the limit resets.
    async fn check_rate(&self, provider: &str) -> Result<RateLimitStatus>;

    /// Record that a call was made to the given provider.
    ///
    /// This must be called after every successful API call to
    /// maintain accurate rate tracking.
    async fn record_call(&self, provider: &str) -> Result<()>;

    /// Compute how long to wait before the next call to the given provider.
    ///
    /// Returns `Duration::ZERO` if the call is allowed immediately.
    /// Otherwise, returns the minimum wait time needed.
    async fn wait_if_needed(&self, provider: &str) -> Result<Duration>;

    /// Reset rate limit tracking for a specific provider.
    async fn reset(&self, provider: &str) -> Result<()>;

    /// Reset all rate limit tracking.
    async fn reset_all(&self) -> Result<()>;
}

// ─── RateLimitStatus ─────────────────────────────────────────────────────────

/// Status of a rate limit check — whether a call is allowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RateLimitStatus {
    /// Whether the call is allowed right now.
    pub allowed: bool,

    /// Remaining calls allowed in the current window.
    pub remaining_calls: u64,

    /// When the rate limit window resets (if applicable).
    pub reset_at: Option<DateTime<Utc>>,

    /// How long to wait before the next call is allowed (in seconds).
    pub wait_seconds: u64,
}

impl RateLimitStatus {
    /// Create an allowed status with remaining quota.
    pub fn allowed(remaining_calls: u64) -> Self {
        Self {
            allowed: true,
            remaining_calls,
            reset_at: None,
            wait_seconds: 0,
        }
    }

    /// Create a denied status with wait time.
    pub fn denied(wait_seconds: u64, reset_at: Option<DateTime<Utc>>) -> Self {
        Self {
            allowed: false,
            remaining_calls: 0,
            reset_at,
            wait_seconds,
        }
    }

    /// Whether the call is allowed right now.
    pub fn is_allowed(&self) -> bool {
        self.allowed
    }
}

// ─── ProviderRateLimit ───────────────────────────────────────────────────────

/// Per-provider rate limit override.
///
/// When a provider has specific rate limits (e.g., Anthropic's 50 RPM
/// for free tier), you can override the global defaults here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderRateLimit {
    /// Maximum calls per minute for this provider.
    pub max_calls_per_minute: u64,

    /// Maximum calls per hour for this provider.
    pub max_calls_per_hour: u64,
}

impl ProviderRateLimit {
    /// Create a per-provider rate limit.
    pub fn new(max_calls_per_minute: u64, max_calls_per_hour: u64) -> Self {
        Self {
            max_calls_per_minute,
            max_calls_per_hour,
        }
    }

    /// OpenAI rate limits (Tier 1): 500 RPM, 30K RPH.
    pub fn openai_tier1() -> Self {
        Self::new(500, 30000)
    }

    /// Anthropic rate limits (free tier): 50 RPM, 1000 RPH.
    pub fn anthropic_free() -> Self {
        Self::new(50, 1000)
    }

    /// Ollama (local): unlimited (effectively very high).
    pub fn ollama() -> Self {
        Self::new(10000, 100000)
    }

    /// Gemini rate limits (free tier): 15 RPM, 1500 RPD.
    pub fn gemini_free() -> Self {
        Self::new(15, 1500)
    }

    /// DeepSeek rate limits: 30 RPM, 500 RPH.
    pub fn deepseek() -> Self {
        Self::new(30, 500)
    }
}

// ─── RateLimitConfig ─────────────────────────────────────────────────────────

/// Configuration for rate limiting.
///
/// Sets global default limits and per-provider overrides.
/// When a provider is not in the overrides, it uses the global defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RateLimitConfig {
    /// Default maximum calls per minute (across all providers).
    pub max_calls_per_minute: u64,

    /// Default maximum calls per hour (across all providers).
    pub max_calls_per_hour: u64,

    /// Per-provider rate limit overrides.
    #[serde(default)]
    pub per_provider_limits: HashMap<String, ProviderRateLimit>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_calls_per_minute: 60,
            max_calls_per_hour: 1000,
            per_provider_limits: HashMap::new(),
        }
    }
}

impl RateLimitConfig {
    /// Create a config with default limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a config with custom global limits.
    pub fn with_limits(max_per_minute: u64, max_per_hour: u64) -> Self {
        Self {
            max_calls_per_minute: max_per_minute,
            max_calls_per_hour: max_per_hour,
            per_provider_limits: HashMap::new(),
        }
    }

    /// Add a per-provider rate limit override.
    pub fn with_provider_limit(mut self, provider: impl Into<String>, limit: ProviderRateLimit) -> Self {
        self.per_provider_limits.insert(provider.into(), limit);
        self
    }

    /// Create a config with common provider-specific limits pre-configured.
    pub fn with_common_provider_limits() -> Self {
        Self {
            max_calls_per_minute: 60,
            max_calls_per_hour: 1000,
            per_provider_limits: HashMap::from([
                ("openai".to_string(), ProviderRateLimit::openai_tier1()),
                ("anthropic".to_string(), ProviderRateLimit::anthropic_free()),
                ("ollama".to_string(), ProviderRateLimit::ollama()),
                ("gemini".to_string(), ProviderRateLimit::gemini_free()),
                ("deepseek".to_string(), ProviderRateLimit::deepseek()),
            ]),
        }
    }

    /// Get the effective limits for a provider.
    ///
    /// Returns per-provider overrides if configured, otherwise global defaults.
    pub fn effective_limits(&self, provider: &str) -> ProviderRateLimit {
        self.per_provider_limits
            .get(provider)
            .cloned()
            .unwrap_or(ProviderRateLimit::new(self.max_calls_per_minute, self.max_calls_per_hour))
    }
}

// ─── TokenWindowRateLimiter ──────────────────────────────────────────────────

/// Sliding window rate limiter — tracks call frequency per provider.
///
/// Uses two sliding windows: per-minute and per-hour.
/// A call is allowed only if BOTH windows have remaining quota.
///
/// Windows slide forward as time passes — calls from more than
/// one minute ago are excluded from the minute window, and calls
/// from more than one hour ago are excluded from the hour window.
pub struct TokenWindowRateLimiter {
    /// Rate limit configuration (global + per-provider overrides).
    config: RateLimitConfig,

    /// Per-provider call timestamps for sliding window tracking.
    call_times: tokio::sync::RwLock<HashMap<String, Vec<DateTime<Utc>>>>,
}

impl TokenWindowRateLimiter {
    /// Create a rate limiter with default configuration.
    pub fn new() -> Self {
        Self {
            config: RateLimitConfig::default(),
            call_times: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Create a rate limiter with custom configuration.
    pub fn with_config(config: RateLimitConfig) -> Self {
        Self {
            config,
            call_times: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Create a rate limiter with common provider-specific limits.
    pub fn with_common_limits() -> Self {
        Self::with_config(RateLimitConfig::with_common_provider_limits())
    }

    /// Count calls in the sliding minute window for a provider.
    fn count_minute_calls(times: &[DateTime<Utc>], now: &DateTime<Utc>) -> usize {
        let one_min_ago = *now - chrono::Duration::seconds(60);
        times.iter().filter(|t| **t > one_min_ago).count()
    }

    /// Count calls in the sliding hour window for a provider.
    fn count_hour_calls(times: &[DateTime<Utc>], now: &DateTime<Utc>) -> usize {
        let one_hour_ago = *now - chrono::Duration::hours(1);
        times.iter().filter(|t| **t > one_hour_ago).count()
    }
}

impl Default for TokenWindowRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl RateLimiter for TokenWindowRateLimiter {
    async fn check_rate(&self, provider: &str) -> Result<RateLimitStatus> {
        let limits = self.config.effective_limits(provider);
        let call_times = self.call_times.read().await;
        let now = Utc::now();

        let times = call_times.get(provider).cloned().unwrap_or_default();

        let minute_calls = Self::count_minute_calls(&times, &now);
        let hour_calls = Self::count_hour_calls(&times, &now);

        let minute_remaining = limits.max_calls_per_minute.saturating_sub(minute_calls as u64);
        let hour_remaining = limits.max_calls_per_hour.saturating_sub(hour_calls as u64);

        if minute_remaining > 0 && hour_remaining > 0 {
            Ok(RateLimitStatus::allowed(minute_remaining.min(hour_remaining)))
        } else {
            // Compute wait time: either wait for minute window reset or hour window reset
            let wait_seconds = if minute_remaining == 0 {
                60 // Wait ~1 minute for minute window to slide
            } else {
                3600 // Wait ~1 hour for hour window to slide
            };
            Ok(RateLimitStatus::denied(wait_seconds, Some(now + chrono::Duration::seconds(wait_seconds as i64))))
        }
    }

    async fn record_call(&self, provider: &str) -> Result<()> {
        let mut call_times = self.call_times.write().await;
        let times = call_times.entry(provider.to_string()).or_insert_with(Vec::new);
        times.push(Utc::now());

        // Prune old entries (keep only last 2 hours to avoid unbounded growth)
        let two_hours_ago = Utc::now() - chrono::Duration::hours(2);
        times.retain(|t| *t > two_hours_ago);

        Ok(())
    }

    async fn wait_if_needed(&self, provider: &str) -> Result<Duration> {
        let status = self.check_rate(provider).await?;
        if status.is_allowed() {
            Ok(Duration::ZERO)
        } else {
            Ok(Duration::from_secs(status.wait_seconds))
        }
    }

    async fn reset(&self, provider: &str) -> Result<()> {
        let mut call_times = self.call_times.write().await;
        call_times.remove(provider);
        Ok(())
    }

    async fn reset_all(&self) -> Result<()> {
        let mut call_times = self.call_times.write().await;
        call_times.clear();
        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_status_allowed() {
        let status = RateLimitStatus::allowed(10);
        assert!(status.is_allowed());
        assert_eq!(status.remaining_calls, 10);
        assert_eq!(status.wait_seconds, 0);
    }

    #[test]
    fn test_rate_limit_status_denied() {
        let status = RateLimitStatus::denied(30, None);
        assert!(!status.is_allowed());
        assert_eq!(status.remaining_calls, 0);
        assert_eq!(status.wait_seconds, 30);
    }

    #[test]
    fn test_provider_rate_limit_creation() {
        let limit = ProviderRateLimit::new(100, 5000);
        assert_eq!(limit.max_calls_per_minute, 100);
        assert_eq!(limit.max_calls_per_hour, 5000);
    }

    #[test]
    fn test_provider_rate_limit_common_presets() {
        let openai = ProviderRateLimit::openai_tier1();
        assert_eq!(openai.max_calls_per_minute, 500);

        let anthropic = ProviderRateLimit::anthropic_free();
        assert_eq!(anthropic.max_calls_per_minute, 50);

        let ollama = ProviderRateLimit::ollama();
        assert_eq!(ollama.max_calls_per_minute, 10000);
    }

    #[test]
    fn test_rate_limit_config_default() {
        let config = RateLimitConfig::default();
        assert_eq!(config.max_calls_per_minute, 60);
        assert_eq!(config.max_calls_per_hour, 1000);
        assert!(config.per_provider_limits.is_empty());
    }

    #[test]
    fn test_rate_limit_config_with_limits() {
        let config = RateLimitConfig::with_limits(100, 5000);
        assert_eq!(config.max_calls_per_minute, 100);
        assert_eq!(config.max_calls_per_hour, 5000);
    }

    #[test]
    fn test_rate_limit_config_with_provider_override() {
        let config = RateLimitConfig::new()
            .with_provider_limit("openai", ProviderRateLimit::openai_tier1());

        let effective = config.effective_limits("openai");
        assert_eq!(effective.max_calls_per_minute, 500);

        let default = config.effective_limits("unknown");
        assert_eq!(default.max_calls_per_minute, 60); // global default
    }

    #[test]
    fn test_rate_limit_config_common_provider_limits() {
        let config = RateLimitConfig::with_common_provider_limits();
        assert_eq!(config.per_provider_limits.len(), 5);

        let openai = config.effective_limits("openai");
        assert_eq!(openai.max_calls_per_minute, 500);
    }

    #[tokio::test]
    async fn test_token_window_rate_limiter_allow_calls() {
        let limiter = TokenWindowRateLimiter::new();

        // First call should be allowed
        let status = limiter.check_rate("openai").await.unwrap();
        assert!(status.is_allowed());
        assert_eq!(status.remaining_calls, 60); // default global limit

        // Record a call
        limiter.record_call("openai").await.unwrap();

        // Check again — should still be allowed with one fewer remaining
        let status = limiter.check_rate("openai").await.unwrap();
        assert!(status.is_allowed());
    }

    #[tokio::test]
    async fn test_token_window_rate_limiter_rate_exceeded() {
        let config = RateLimitConfig::with_limits(5, 100);
        let limiter = TokenWindowRateLimiter::with_config(config);

        // Make 5 calls to exhaust the minute window
        for _ in 0..5 {
            limiter.record_call("test").await.unwrap();
        }

        // Next call should be denied
        let status = limiter.check_rate("test").await.unwrap();
        assert!(!status.is_allowed());
        assert_eq!(status.wait_seconds, 60); // ~1 minute wait
    }

    #[tokio::test]
    async fn test_token_window_rate_limiter_wait_time() {
        let limiter = TokenWindowRateLimiter::new();

        // No calls yet — wait time should be zero
        let wait = limiter.wait_if_needed("openai").await.unwrap();
        assert_eq!(wait, Duration::ZERO);
    }

    #[tokio::test]
    async fn test_token_window_rate_limiter_reset() {
        let limiter = TokenWindowRateLimiter::new();

        limiter.record_call("openai").await.unwrap();
        limiter.record_call("anthropic").await.unwrap();

        // Reset specific provider
        limiter.reset("openai").await.unwrap();

        let status = limiter.check_rate("openai").await.unwrap();
        assert!(status.is_allowed());
        assert_eq!(status.remaining_calls, 60); // back to full quota

        // Reset all
        limiter.reset_all().await.unwrap();

        let status2 = limiter.check_rate("anthropic").await.unwrap();
        assert!(status2.is_allowed());
    }

    #[tokio::test]
    async fn test_token_window_rate_limiter_per_provider_override() {
        let config = RateLimitConfig::new()
            .with_provider_limit("anthropic", ProviderRateLimit::anthropic_free());
        let limiter = TokenWindowRateLimiter::with_config(config);

        // Anthropic should use per-provider limit (50 RPM)
        let status = limiter.check_rate("anthropic").await.unwrap();
        assert!(status.is_allowed());
        assert_eq!(status.remaining_calls, 50);

        // Other providers should use global default (60 RPM)
        let status2 = limiter.check_rate("openai").await.unwrap();
        assert!(status2.is_allowed());
        assert_eq!(status2.remaining_calls, 60);
    }
}
