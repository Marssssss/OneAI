//! Per-tenant quotas + create-rate limiting (MVS4-B).
//!
//! Three enforcement layers, all keyed on the tenant bucket
//! ([`crate::runner::tenant_bucket`] — untagged sessions share the literal
//! `"default"` bucket):
//!
//! 1. **Concurrent-session cap** — exact and cross-replica: the count +
//!    insert run inside ONE store operation
//!    ([`SessionStore::insert_if_under_quota`]); on Pg that is a single
//!    transaction under `pg_advisory_xact_lock(hashtext(tenant))`, so two
//!    replicas racing the last slot produce exactly one winner. Never
//!    check-then-insert as two calls.
//! 2. **Token budget** — the engine containers stamp every usage row with
//!    `metadata.tenant_id` (orchestrator injects `ONEAI_TENANT_ID` at spawn;
//!    the CLI-layer `TenantTaggingUsageTracker` decorator does the rest), so
//!    the budget is a server-side `SUM` over `usage_records_pg`. Lifetime
//!    (`max_total_tokens`) or rolling-24h (`daily_token_budget`). Requires a
//!    [`TenantUsageSum`] source (Pg mode); without one the check degrades
//!    OPEN with a loud warning (a usage-DB outage must not brick session
//!    creation — availability over exactness, same posture as the registry
//!    cache serving stale reads).
//! 3. **Create rate** — per-replica in-memory token bucket (burst = one
//!    minute of the rate). Cross-replica exactness would need Redis/Pg row
//!    throttling; the design doc (§6 MVS4) explicitly accepts the per-replica
//!    approximation, and the shared routing table already bounds the total
//!    via layer 1.
//!
//! Quotas are OPT-IN per tenant: [`crate::config::OrchestratorConfig::
//! effective_quota`] resolves `quotas_tenants.<bucket>` → `quotas_default` →
//! `None` (unlimited). With no `[quotas*]` config every check short-circuits
//! — pre-MVS4-B deployments and acceptance scripts see identical behaviour.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use serde::Serialize;

use crate::config::QuotaConfig;
use crate::error::{OrchestratorError, Result};
use crate::runner::tenant_bucket;

/// Which quota rejected the request (machine-readable 429 discriminator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum QuotaReason {
    /// Concurrent-session cap for the tenant bucket.
    ConcurrentSessions,
    /// Token budget (lifetime or daily) exhausted.
    TokenBudget,
    /// Per-replica create-rate token bucket empty.
    CreateRate,
}

impl QuotaReason {
    /// Wire value for the 429 body's `reason` field.
    pub fn as_str(self) -> &'static str {
        match self {
            QuotaReason::ConcurrentSessions => "concurrent_sessions",
            QuotaReason::TokenBudget => "token_budget",
            QuotaReason::CreateRate => "create_rate",
        }
    }
}

impl std::fmt::Display for QuotaReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Shared token-usage read side for budget checks. Implemented by the
/// `PgUsageTracker` adapter (feature `postgres`); the orchestrator core stays
/// backend-agnostic. Errors are treated as "budget unknown" → fail open.
#[async_trait]
pub trait TenantUsageSum: Send + Sync {
    /// `SUM(prompt_tokens + completion_tokens)` over every usage row tagged
    /// with this tenant. `daily` narrows to the rolling 24h window.
    async fn tenant_token_sum(&self, tenant: &str, daily: bool)
        -> std::result::Result<u64, String>;
}

/// Adapter over the shared-Pg usage ledger (MVS3-B table, MVS4-B tenant
/// tagging). One pool with the engine containers' tracker — same DSN.
#[cfg(feature = "postgres")]
pub struct PgTenantUsage {
    tracker: std::sync::Arc<oneai_persistence::PgUsageTracker>,
}

#[cfg(feature = "postgres")]
impl PgTenantUsage {
    /// Wrap a connected tracker.
    pub fn new(tracker: std::sync::Arc<oneai_persistence::PgUsageTracker>) -> Self {
        Self { tracker }
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl TenantUsageSum for PgTenantUsage {
    async fn tenant_token_sum(
        &self,
        tenant: &str,
        daily: bool,
    ) -> std::result::Result<u64, String> {
        self.tracker
            .tenant_token_sum(tenant, daily)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Lazy-refill token bucket (create-rate limiting). Capacity = one minute
/// of the rate (burst allowance); refill is continuous.
#[derive(Debug, Clone)]
struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate_per_min: u32) -> Self {
        let rate = rate_per_min.max(1) as f64;
        Self {
            capacity: rate,
            tokens: rate,
            refill_per_sec: rate / 60.0,
            last: Instant::now(),
        }
    }

    /// Consume one token; `Err(secs_to_wait)` when empty (≥1s, for the
    /// `Retry-After` header).
    fn try_take(&mut self) -> std::result::Result<(), u64> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - self.tokens) / self.refill_per_sec;
            Err(wait.ceil().max(1.0) as u64)
        }
    }
}

/// The enforcement point held by `OrchestratorState`. Cheap to clone
/// (Arc inside); the rate buckets are process-local by design (layer 3).
pub struct TenantQuotaEnforcer {
    buckets: Mutex<HashMap<String, TokenBucket>>,
    usage: Option<std::sync::Arc<dyn TenantUsageSum>>,
}

impl TenantQuotaEnforcer {
    /// Enforcer with an optional token-usage source (`None` = budget checks
    /// degrade open with a warning; file-mode single-replica deployments).
    pub fn new(usage: Option<std::sync::Arc<dyn TenantUsageSum>>) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            usage,
        }
    }

    /// Layer 3: per-replica create-rate check. Consumes one token on
    /// success (so a request rejected by a LATER layer still spent its rate
    /// token — intentional: the burst protection covers the whole create
    /// path, and double-spend on failure would let a client hammer retries).
    pub fn check_create_rate(&self, tenant_id: &str, quota: &QuotaConfig) -> Result<()> {
        let Some(rate) = quota.create_rate_per_min.filter(|r| *r > 0) else {
            return Ok(());
        };
        let bucket_key = tenant_bucket(tenant_id).to_string();
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = buckets
            .entry(bucket_key.clone())
            .or_insert_with(|| TokenBucket::new(rate));
        // Rate reconfigured since the bucket was minted → re-mint (burst
        // resets; a config change is an admin action, not a client one).
        if (bucket.capacity - rate as f64).abs() > f64::EPSILON {
            *bucket = TokenBucket::new(rate);
        }
        match bucket.try_take() {
            Ok(()) => Ok(()),
            Err(retry_after) => Err(self.rejection(
                tenant_id,
                QuotaReason::CreateRate,
                rate as u64,
                rate as u64,
                Some(retry_after),
                format!(
                    "tenant '{bucket_key}' exceeded the session-create rate \
                     ({rate}/min on this replica; retry after {retry_after}s)"
                ),
            )),
        }
    }

    /// Layer 2: token-budget check (lifetime or rolling-24h). Degrades OPEN
    /// on a missing usage source or a failed SUM (loud warning) — see the
    /// module docs for the availability rationale.
    pub async fn check_token_budget(&self, tenant_id: &str, quota: &QuotaConfig) -> Result<()> {
        // Lifetime wins when both are configured (strictest single number;
        // daily is the softer alternative, not an additional gate — the
        // config picks one semantic).
        let (limit, daily) = match (quota.max_total_tokens, quota.daily_token_budget) {
            (Some(total), _) => (total, false),
            (None, Some(d)) => (d, true),
            (None, None) => return Ok(()),
        };
        let Some(usage) = &self.usage else {
            tracing::warn!(
                tenant = %tenant_id,
                "token budget configured but no shared usage source is wired \
                 (file mode / no Pg) — budget check SKIPPED (fail open)"
            );
            return Ok(());
        };
        let bucket = tenant_bucket(tenant_id);
        let sum = match usage.tenant_token_sum(bucket, daily).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    tenant = %bucket, error = %e,
                    "token-budget SUM failed — check SKIPPED (fail open)"
                );
                return Ok(());
            }
        };
        if sum >= limit {
            let window = if daily { "24h " } else { "" };
            return Err(self.rejection(
                tenant_id,
                QuotaReason::TokenBudget,
                limit,
                sum,
                None,
                format!(
                    "tenant '{bucket}' exhausted its {window}token budget \
                     ({sum}/{limit} tokens used)"
                ),
            ));
        }
        Ok(())
    }

    /// Layer 1 rejection constructor (the count+insert itself lives in
    /// `SessionStore::insert_if_under_quota`; the caller turns the observed
    /// count into this error).
    pub fn concurrent_rejection(
        &self,
        tenant_id: &str,
        limit: u64,
        current: u64,
    ) -> OrchestratorError {
        let bucket = tenant_bucket(tenant_id);
        self.rejection(
            tenant_id,
            QuotaReason::ConcurrentSessions,
            limit,
            current,
            None,
            format!(
                "tenant '{bucket}' has reached its concurrent-session limit \
                 ({current}/{limit} active)"
            ),
        )
    }

    fn rejection(
        &self,
        tenant_id: &str,
        reason: QuotaReason,
        limit: u64,
        current: u64,
        retry_after_secs: Option<u64>,
        message: String,
    ) -> OrchestratorError {
        OrchestratorError::QuotaExceeded {
            tenant: tenant_bucket(tenant_id).to_string(),
            reason,
            limit,
            current,
            retry_after_secs,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota() -> QuotaConfig {
        QuotaConfig {
            create_rate_per_min: Some(3),
            ..Default::default()
        }
    }

    #[test]
    fn token_bucket_burst_then_refill() {
        let mut b = TokenBucket::new(60); // 1 token/sec
        for _ in 0..60 {
            assert!(b.try_take().is_ok());
        }
        let err = b.try_take().unwrap_err();
        assert!((1..=2).contains(&err), "retry_after ~1s, got {err}");
    }

    #[test]
    fn rate_limit_rejects_after_burst() {
        let e = TenantQuotaEnforcer::new(None);
        let q = quota(); // 3/min
        for _ in 0..3 {
            e.check_create_rate("acme", &q).unwrap();
        }
        let err = e.check_create_rate("acme", &q).unwrap_err();
        match err {
            OrchestratorError::QuotaExceeded {
                reason,
                tenant,
                retry_after_secs,
                ..
            } => {
                assert_eq!(reason, QuotaReason::CreateRate);
                assert_eq!(tenant, "acme");
                assert!(retry_after_secs.is_some_and(|s| s >= 1));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // A different tenant has its own bucket.
        e.check_create_rate("other", &q).unwrap();
        // Untagged normalizes to the "default" bucket.
        e.check_create_rate("", &q).unwrap();
    }

    #[test]
    fn rate_reconfig_remints_bucket() {
        let e = TenantQuotaEnforcer::new(None);
        let q1 = QuotaConfig {
            create_rate_per_min: Some(1),
            ..Default::default()
        };
        e.check_create_rate("t", &q1).unwrap();
        assert!(e.check_create_rate("t", &q1).is_err());
        // Raise the rate → bucket re-mints with the new capacity.
        let q2 = QuotaConfig {
            create_rate_per_min: Some(10),
            ..Default::default()
        };
        e.check_create_rate("t", &q2).unwrap();
    }

    #[tokio::test]
    async fn budget_skipped_without_usage_source() {
        let e = TenantQuotaEnforcer::new(None);
        let q = QuotaConfig {
            max_total_tokens: Some(100),
            ..Default::default()
        };
        // Fail-open: no source → Ok (warn logged).
        e.check_token_budget("acme", &q).await.unwrap();
    }

    /// Stub usage source returning a fixed sum.
    struct FixedSum(u64);
    #[async_trait]
    impl TenantUsageSum for FixedSum {
        async fn tenant_token_sum(
            &self,
            _tenant: &str,
            _daily: bool,
        ) -> std::result::Result<u64, String> {
            Ok(self.0)
        }
    }

    /// Stub whose SUM always fails — proves the fail-open posture.
    struct FailingSum;
    #[async_trait]
    impl TenantUsageSum for FailingSum {
        async fn tenant_token_sum(
            &self,
            _tenant: &str,
            _daily: bool,
        ) -> std::result::Result<u64, String> {
            Err("pg down".into())
        }
    }

    #[tokio::test]
    async fn budget_rejects_at_limit_and_daily_window() {
        let e = TenantQuotaEnforcer::new(Some(std::sync::Arc::new(FixedSum(900))));
        // Lifetime: under → ok; at/over → reject.
        let q = QuotaConfig {
            max_total_tokens: Some(1000),
            ..Default::default()
        };
        e.check_token_budget("acme", &q).await.unwrap();
        let q = QuotaConfig {
            max_total_tokens: Some(900),
            ..Default::default()
        };
        let err = e.check_token_budget("acme", &q).await.unwrap_err();
        match err {
            OrchestratorError::QuotaExceeded {
                reason,
                current,
                limit,
                retry_after_secs,
                message,
                ..
            } => {
                assert_eq!(reason, QuotaReason::TokenBudget);
                assert_eq!((current, limit), (900, 900));
                assert!(retry_after_secs.is_none());
                assert!(message.contains("token budget"), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Daily window works the same way.
        let q = QuotaConfig {
            daily_token_budget: Some(500),
            ..Default::default()
        };
        assert!(e.check_token_budget("acme", &q).await.is_err());
        // No budget configured → ok.
        e.check_token_budget("acme", &QuotaConfig::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn budget_fails_open_on_sum_error() {
        let e = TenantQuotaEnforcer::new(Some(std::sync::Arc::new(FailingSum)));
        let q = QuotaConfig {
            max_total_tokens: Some(10),
            ..Default::default()
        };
        e.check_token_budget("acme", &q).await.unwrap();
    }

    #[test]
    fn reason_wire_values() {
        assert_eq!(
            QuotaReason::ConcurrentSessions.as_str(),
            "concurrent_sessions"
        );
        assert_eq!(QuotaReason::TokenBudget.as_str(), "token_budget");
        assert_eq!(QuotaReason::CreateRate.as_str(), "create_rate");
    }
}
