//! Retry logic for transient provider API errors (429 rate limits, 503 service unavailable).
//!
//! When a provider API returns a retryable status code (429, 529, 503), the client
//! should automatically retry with exponential backoff instead of immediately failing.
//! This is standard practice for all major API clients (OpenAI Python SDK, Anthropic
//! SDK, Google API clients all retry 429 automatically).
//!
//! Key concepts:
//! - `ProviderRetryConfig`: Configuration for retry behavior (max retries, delays, backoff)
//! - `is_retryable_status`: Which HTTP status codes should trigger retry
//! - `parse_retry_after`: Extract `Retry-After` header value from response headers
//! - `compute_backoff_delay`: Exponential backoff delay computation
//! - `send_with_retry`: Execute an async HTTP request with automatic retry on transient errors

use std::future::Future;
use std::time::Duration;
use reqwest::StatusCode;

// ─── ProviderRetryConfig ─────────────────────────────────────────────────────

/// Configuration for automatic retry on transient provider API errors.
///
/// Default values are chosen to handle common burst rate limit patterns:
/// - 3 retries: enough for most burst rate limits (429 limit_burst_rate)
/// - 1 second initial delay: reasonable wait for rate limit to clear
/// - 30 second max delay: prevents excessive waiting on sustained rate limits
/// - 2.0 backoff factor: standard exponential (1s → 2s → 4s → 8s → ...)
///
/// Inspired by OpenAI Python SDK's retry behavior and the existing
/// RecoveryManager's RetryPolicy in `error_recovery.rs`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProviderRetryConfig {
    /// Maximum number of retry attempts.
    /// 0 = no retry (fail immediately on transient errors).
    /// Default: 3.
    pub max_retries: usize,

    /// Initial delay before the first retry (in milliseconds).
    /// Default: 1000 (1 second).
    pub initial_delay_ms: u64,

    /// Maximum delay between retries (in milliseconds).
    /// Prevents excessive waiting on sustained rate limits.
    /// Default: 30000 (30 seconds).
    pub max_delay_ms: u64,

    /// Exponential backoff factor.
    /// delay = initial_delay * backoff_factor^attempt
    /// Default: 2.0 (delay doubles each attempt).
    pub backoff_factor: f64,
}

impl Default for ProviderRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 1000,
            max_delay_ms: 30000,
            backoff_factor: 2.0,
        }
    }
}

impl ProviderRetryConfig {
    /// Create a retry config with no retries — fail immediately on transient errors.
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            initial_delay_ms: 0,
            max_delay_ms: 0,
            backoff_factor: 1.0,
        }
    }

    /// Create a retry config with custom parameters.
    pub fn new(max_retries: usize, initial_delay_ms: u64, max_delay_ms: u64, backoff_factor: f64) -> Self {
        Self {
            max_retries,
            initial_delay_ms,
            max_delay_ms,
            backoff_factor,
        }
    }

    /// Create an aggressive retry config for rate-limited environments.
    /// More retries and longer delays to handle sustained rate limits.
    pub fn aggressive() -> Self {
        Self {
            max_retries: 5,
            initial_delay_ms: 2000,
            max_delay_ms: 60000,
            backoff_factor: 2.0,
        }
    }

    /// Whether retry is enabled (max_retries > 0).
    pub fn is_enabled(&self) -> bool {
        self.max_retries > 0
    }
}

// ─── is_retryable_status ─────────────────────────────────────────────────────

/// Determine if an HTTP status code is retryable.
///
/// Retryable status codes indicate transient errors that may resolve after waiting:
/// - 429 Too Many Requests: rate limit exceeded, should retry after delay
/// - 503 Service Unavailable: provider temporarily unavailable
/// - 529 Site Is Overloaded: some providers (智谱GLM) use this for capacity limits
///
/// Non-retryable codes indicate permanent errors that won't resolve:
/// - 400 Bad Request: request format is wrong
/// - 401 Unauthorized: invalid API key
/// - 403 Forbidden: access denied
/// - 404 Not Found: endpoint doesn't exist
/// - 500 Internal Server Error: provider bug (may or may not be retryable,
///   but we don't retry to avoid amplifying bugs)
pub fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 503 | 529)
}

// ─── parse_retry_after ───────────────────────────────────────────────────────

/// Parse the `Retry-After` header value from HTTP response headers.
///
/// The `Retry-After` header can be either:
/// - An integer number of seconds (e.g., "30" → 30 seconds)
/// - An HTTP date (e.g., "Fri, 20 Jun 2026 12:00:00 GMT" → computed seconds until that date)
///
/// Returns the delay in milliseconds, or None if the header is not present or invalid.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let str_value = value.to_str().ok()?;

    // Try parsing as integer seconds first (most common format)
    if let Ok(secs) = str_value.parse::<u64>() {
        return Some(secs * 1000);
    }

    // Try parsing as HTTP date
    // Common formats: "Fri, 20 Jun 2026 12:00:00 GMT" (RFC 2822)
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(str_value) {
        let now = chrono::Utc::now();
        let diff = dt.with_timezone(&chrono::Utc) - now;
        let secs = diff.num_seconds().max(0) as u64;
        return Some(secs * 1000);
    }

    None
}

// ─── compute_backoff_delay ───────────────────────────────────────────────────

/// Compute the backoff delay for a given retry attempt.
///
/// Uses exponential backoff: delay = initial_delay * backoff_factor^attempt
/// Capped at max_delay to prevent excessive waiting.
///
/// For attempt 0 (first retry): delay = initial_delay * 1 = initial_delay
/// For attempt 1 (second retry): delay = initial_delay * 2 = 2 * initial_delay
/// For attempt 2 (third retry): delay = initial_delay * 4 = 4 * initial_delay
///
/// If a `retry_after_ms` value is provided (from the Retry-After header),
/// it overrides the computed delay (the provider's recommended wait time
/// is more accurate than our estimate), but is still capped at max_delay.
pub fn compute_backoff_delay(attempt: usize, config: &ProviderRetryConfig, retry_after_ms: Option<u64>) -> u64 {
    // If the provider gave us a Retry-After value, use it (it's more accurate)
    if let Some(server_delay) = retry_after_ms {
        return server_delay.min(config.max_delay_ms);
    }

    // Exponential backoff: initial_delay * factor^attempt
    let factor_power = config.backoff_factor.powi(attempt as i32);
    let computed = (config.initial_delay_ms as f64 * factor_power) as u64;

    // Cap at max_delay
    computed.min(config.max_delay_ms)
}

// ─── send_with_retry ─────────────────────────────────────────────────────────

/// Send an HTTP request with automatic retry on transient errors.
///
/// Returns `Result<reqwest::Response, reqwest::Error>` — same type as `.send()`,
/// so the caller can handle it exactly the same way. The only difference is that
/// transient errors (429, 503, 529) and network errors are automatically retried
/// with exponential backoff before returning.
///
/// The `request_fn` closure is called repeatedly until:
/// 1. The response has a success status code (200-299)
/// 2. The response has a non-retryable error status code (400, 401, 403, 404, 500, etc.)
/// 3. The maximum number of retries is exhausted (returns the last response/error)
///
/// Between retries, the delay is computed using exponential backoff,
/// optionally adjusted by the `Retry-After` header from the previous response.
///
/// ## Usage
/// ```ignore
/// let response = send_with_retry(
///     &self.retry_config,
///     || self.client.post(&url).header("Authorization", ...).json(&body).send(),
/// ).await
///     .map_err(|e| OneAIError::Network(e.to_string()))?;
///
/// if !response.status().is_success() {
///     let status = response.status();
///     let text = response.text().await.map_err(|e| OneAIError::Network(e.to_string()))?;
///     if is_retryable_status(status) {
///         return Err(OneAIError::RateLimit(format!(
///             "Rate limit after {} retries: {} {}", status, self.retry_config.max_retries, text
///         )));
///     }
///     return Err(OneAIError::Provider(format!("API error {}: {}", status, text)));
/// }
/// ```
pub async fn send_with_retry<F, Fut>(
    config: &ProviderRetryConfig,
    request_fn: F,
) -> std::result::Result<reqwest::Response, reqwest::Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = std::result::Result<reqwest::Response, reqwest::Error>>,
{
    if !config.is_enabled() {
        // No retry — just call the request function once
        return request_fn().await;
    }

    let mut attempt: usize = 0;

    loop {
        match request_fn().await {
            Ok(response) => {
                let status = response.status();

                // Success or non-retryable error — return immediately
                if status.is_success() || !is_retryable_status(status) {
                    return Ok(response);
                }

                // Retryable status (429, 503, 529) — check if we can retry
                if attempt >= config.max_retries {
                    tracing::error!(
                        "Provider API rate limit error {} after {} retries exhausted (max_retries={})",
                        status, attempt, config.max_retries
                    );
                    return Ok(response); // Return the 429 response — caller handles it
                }

                // Parse Retry-After header for more accurate delay
                let retry_after_ms = parse_retry_after(response.headers());

                // Drop the response before retrying (releases the connection)
                drop(response);

                let delay = compute_backoff_delay(attempt, config, retry_after_ms);
                tracing::warn!(
                    "Provider API returned {} (retry {}/{}, waiting {}ms): rate limit or service unavailable",
                    status,
                    attempt + 1,
                    config.max_retries,
                    delay
                );

                attempt += 1;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            Err(e) => {
                // Network error (connection refused, timeout, DNS failure)
                if attempt >= config.max_retries {
                    tracing::error!(
                        "Provider request network error after {} retries: {}",
                        attempt, e
                    );
                    return Err(e); // Return the network error — caller handles it
                }

                let delay = compute_backoff_delay(attempt, config, None);
                tracing::warn!(
                    "Provider request network error (retry {}/{}, waiting {}ms): {}",
                    attempt + 1,
                    config.max_retries,
                    delay,
                    e
                );

                attempt += 1;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_config_default() {
        let config = ProviderRetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.initial_delay_ms, 1000);
        assert_eq!(config.max_delay_ms, 30000);
        assert_eq!(config.backoff_factor, 2.0);
        assert!(config.is_enabled());
    }

    #[test]
    fn test_retry_config_no_retry() {
        let config = ProviderRetryConfig::no_retry();
        assert_eq!(config.max_retries, 0);
        assert!(!config.is_enabled());
    }

    #[test]
    fn test_retry_config_aggressive() {
        let config = ProviderRetryConfig::aggressive();
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.initial_delay_ms, 2000);
        assert_eq!(config.max_delay_ms, 60000);
    }

    #[test]
    fn test_is_retryable_status() {
        // Retryable: rate limit, service unavailable, overloaded
        assert!(is_retryable_status(StatusCode::from_u16(429).unwrap()));
        assert!(is_retryable_status(StatusCode::from_u16(503).unwrap()));
        assert!(is_retryable_status(StatusCode::from_u16(529).unwrap()));

        // Non-retryable: auth errors, bad request, server error, success
        assert!(!is_retryable_status(StatusCode::from_u16(400).unwrap()));
        assert!(!is_retryable_status(StatusCode::from_u16(401).unwrap()));
        assert!(!is_retryable_status(StatusCode::from_u16(403).unwrap()));
        assert!(!is_retryable_status(StatusCode::from_u16(404).unwrap()));
        assert!(!is_retryable_status(StatusCode::from_u16(500).unwrap()));
        assert!(!is_retryable_status(StatusCode::from_u16(200).unwrap()));
        assert!(!is_retryable_status(StatusCode::from_u16(201).unwrap()));
    }

    #[test]
    fn test_compute_backoff_delay_exponential() {
        let config = ProviderRetryConfig::default();

        // attempt 0: 1000 * 2^0 = 1000
        assert_eq!(compute_backoff_delay(0, &config, None), 1000);
        // attempt 1: 1000 * 2^1 = 2000
        assert_eq!(compute_backoff_delay(1, &config, None), 2000);
        // attempt 2: 1000 * 2^2 = 4000
        assert_eq!(compute_backoff_delay(2, &config, None), 4000);
        // attempt 3: 1000 * 2^3 = 8000
        assert_eq!(compute_backoff_delay(3, &config, None), 8000);
        // attempt 4: 1000 * 2^4 = 16000
        assert_eq!(compute_backoff_delay(4, &config, None), 16000);
        // attempt 5: 1000 * 2^5 = 32000, capped at 30000
        assert_eq!(compute_backoff_delay(5, &config, None), 30000);
    }

    #[test]
    fn test_compute_backoff_delay_with_retry_after() {
        let config = ProviderRetryConfig::default();

        // Retry-After overrides computed delay
        assert_eq!(compute_backoff_delay(0, &config, Some(5000)), 5000);
        assert_eq!(compute_backoff_delay(3, &config, Some(15000)), 15000);

        // Retry-After is capped at max_delay
        assert_eq!(compute_backoff_delay(0, &config, Some(60000)), 30000);
    }

    #[test]
    fn test_parse_retry_after_integer() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());

        let result = parse_retry_after(&headers);
        assert_eq!(result, Some(30000)); // 30 seconds → 30000 ms
    }

    #[test]
    fn test_parse_retry_after_missing() {
        let headers = reqwest::header::HeaderMap::new();
        let result = parse_retry_after(&headers);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_retry_after_invalid() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "not-a-number".parse().unwrap());

        let result = parse_retry_after(&headers);
        // "not-a-number" is not a valid integer or RFC 2822 date
        assert!(result.is_none());
    }
}
