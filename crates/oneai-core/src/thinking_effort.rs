//! User-facing thinking-effort tiers and the store that persists the choice.
//!
//! glm-5.2 (served via Aliyun DashScope) defaults to reasoning effort "max" —
//! a single inference can emit 100k–170k chars of `reasoning_content`
//! (≈30–50k tokens), which either burns a sub-agent's run-cost budget in one
//! iteration ("thinking to death") or ruminates 60–108 s before a ≤300字
//! summary. The OpenAI provider already maps `thinking_budget: Option<u32>`
//! → DashScope's `enable_thinking` / `thinking_budget` (verified live: a
//! `thinking_budget` cap bounds `reasoning_content` at N tokens AND the model
//! still emits complete output). This module surfaces that lever as a small
//! user-selectable enum so the web UI can offer an "思考程度" toggle.
//!
//! Scope (confirmed with the user): the chosen tier applies to BOTH the main
//! agent and delegated sub-agents, but sub-agents are capped at a per-kind
//! engine maximum so a user picking "Max" can never make a sub-agent think
//! itself to death. The main agent (no run-cost cap by default) follows the
//! tier directly.

use async_trait::async_trait;

/// User-facing thinking-effort tier. `#[non_exhaustive]` per the v0.2.0
/// API-stability commitment — future tiers (e.g. an "auto" that adapts to
/// task complexity) may be added without a breaking change; callers should
/// match with a fallback arm.
///
/// Serialized as lowercase strings (`"off"`, `"low"`, `"medium"`, `"high"`,
/// `"max"`) on the JSON-RPC wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ThinkingEffort {
    /// Disable reasoning entirely — pure output, zero reasoning token cost.
    /// Maps to `enable_thinking: false`. Fastest; for trivial/exec tasks.
    Off,
    /// Bounded reasoning (~1k tokens).
    Low,
    /// Bounded reasoning (~4k tokens). The default — balances speed and
    /// depth, directly addressing the main agent's 57s pre-delegate
    /// rumination.
    #[default]
    Medium,
    /// Bounded reasoning (~16k tokens). For deep multi-step orchestration.
    High,
    /// Unbounded (provider default — glm "max" effort). Slowest; the main
    /// agent may ruminate freely, sub-agents are still capped per-kind.
    Max,
}

impl ThinkingEffort {
    /// The `thinking_budget: Option<u32>` to thread into `AgentLoopConfig` /
    /// `GenerationConfig`, which the OpenAI provider maps to DashScope's
    /// `enable_thinking` + `thinking_budget`.
    ///
    /// - `Off` → `Some(0)` → `enable_thinking: false`
    /// - `Low`/`Medium`/`High` → `Some(N)` → enabled, capped at N
    /// - `Max` → `None` → emit nothing (provider default = unbounded)
    pub fn as_thinking_budget(&self) -> Option<u32> {
        match self {
            ThinkingEffort::Off => Some(0),
            ThinkingEffort::Low => Some(1024),
            ThinkingEffort::Medium => Some(4096),
            ThinkingEffort::High => Some(16384),
            ThinkingEffort::Max => None,
        }
    }

    /// Inverse of [`as_thinking_budget`] — recover the tier from a stored
    /// `thinking_budget` value (for showing the current selection in the UI
    /// when only the raw budget is known). `None` → `Max`, `Some(0)` → `Off`,
    /// otherwise bucketed by the tier thresholds.
    pub fn from_thinking_budget(budget: Option<u32>) -> Self {
        match budget {
            None => ThinkingEffort::Max,
            Some(0) => ThinkingEffort::Off,
            Some(n) if n <= 1024 => ThinkingEffort::Low,
            Some(n) if n <= 4096 => ThinkingEffort::Medium,
            Some(_) => ThinkingEffort::High,
        }
    }

    /// A short human-readable label (used in UI/debug logs).
    pub fn label(&self) -> &'static str {
        match self {
            ThinkingEffort::Off => "off",
            ThinkingEffort::Low => "low",
            ThinkingEffort::Medium => "medium",
            ThinkingEffort::High => "high",
            ThinkingEffort::Max => "max",
        }
    }
}

impl std::fmt::Display for ThinkingEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Compute the effective thinking budget for a delegated sub-agent: the
/// user's chosen tier, capped at the sub-agent kind's engine maximum so a
/// "Max" user can never make a sub-agent think itself to death.
///
/// `Option<u32>` min semantics:
/// - `(Some(user), Some(cap))` → `Some(user.min(cap))` — both bound; lower wins.
/// - `(Some(user), None)` → `Some(user)` — kind has no cap; user binds.
/// - `(None, Some(cap))` → `Some(cap)` — user picked Max (unbounded); the
///   kind cap still binds (death prevention).
/// - `(None, None)` → `None` — both unbounded (a reasoning kind with no cap
///   under a Max user).
pub fn min_effort_cap(user: Option<u32>, cap: Option<u32>) -> Option<u32> {
    match (user, cap) {
        (Some(u), Some(c)) => Some(u.min(c)),
        (Some(u), None) => Some(u),
        (None, Some(c)) => Some(c),
        (None, None) => None,
    }
}

/// Persisted, user-configurable thinking-effort selection. Object-safe so the
/// same `Arc<dyn ThinkingEffortStore>` is shared between the engine (reads the
/// tier each turn) and the app-server (the `thinking/get`·`thinking/set`
/// JSON-RPC methods), in one process. `get` returns [`ThinkingEffort::default`]
/// (`Medium`) when no value is persisted yet.
#[async_trait]
pub trait ThinkingEffortStore: Send + Sync {
    /// The currently-selected tier (default `Medium` if never set).
    async fn get(&self) -> ThinkingEffort;

    /// Persist `effort` as the new selection (idempotent).
    async fn set(&self, effort: ThinkingEffort);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_thinking_budget_round_trips_through_tiers() {
        for tier in [
            ThinkingEffort::Off,
            ThinkingEffort::Low,
            ThinkingEffort::Medium,
            ThinkingEffort::High,
            ThinkingEffort::Max,
        ] {
            // Max → None (no budget); the others survive the round trip via
            // the bucketing inverse.
            let back = ThinkingEffort::from_thinking_budget(tier.as_thinking_budget());
            assert_eq!(tier, back, "{tier:?} round-trips");
        }
    }

    #[test]
    fn min_effort_cap_user_max_still_capped() {
        // The whole point: a Max user (None = unbounded) cannot make a capped
        // sub-agent (e.g. Code, cap 2048) think itself to death.
        assert_eq!(min_effort_cap(None, Some(2048)), Some(2048));
    }

    #[test]
    fn min_effort_cap_lower_user_wins_over_cap() {
        assert_eq!(min_effort_cap(Some(1024), Some(2048)), Some(1024));
        assert_eq!(min_effort_cap(Some(4096), Some(2048)), Some(2048));
    }

    #[test]
    fn min_effort_cap_uncapped_kind_follows_user() {
        // Reasoning kinds (cap None): user binds directly.
        assert_eq!(min_effort_cap(Some(4096), None), Some(4096));
        assert_eq!(min_effort_cap(None, None), None);
    }

    #[test]
    fn default_is_medium() {
        assert_eq!(ThinkingEffort::default(), ThinkingEffort::Medium);
        assert_eq!(ThinkingEffort::default().as_thinking_budget(), Some(4096));
    }

    #[test]
    fn serde_round_trips_lowercase() {
        for tier in [
            ThinkingEffort::Off,
            ThinkingEffort::Low,
            ThinkingEffort::Medium,
            ThinkingEffort::High,
            ThinkingEffort::Max,
        ] {
            let s = serde_json::to_string(&tier).unwrap();
            assert_eq!(s, format!("\"{}\"", tier.label()));
            let back: ThinkingEffort = serde_json::from_str(&s).unwrap();
            assert_eq!(tier, back);
        }
    }
}
