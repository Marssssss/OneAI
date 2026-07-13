//! Efficiency profile — wall-clock + token decomposition of an agent run.
//!
//! This generalizes the timing breakdown that previously lived only inside
//! the SWE-bench runner (`swebench/runner.rs::trace_timing_breakdown`).
//! Any `EvalRunner` case can now produce an `EfficiencyProfile` straight
//! from its trace span tree, giving every suite an efficiency axis
//! (inference vs tool vs overhead wall-clock, call counts, token cost, and
//! when A4 lands, prompt-cache hit ratio).
//!
//! The three-axis evaluation score (能力×成本×效率 / quality×tokens×latency)
//! is encoded as [`EfficiencyProfile::three_axis_score`]:
//!
//! ```text
//! efficiency = quality / (1 + log10(1 + tokens) * 0.1 + log10(1 + latency_ms) * 0.1)
//! ```
//!
//! (A log-scaled denominator keeps the score in a sane 0..1 range and avoids
//! a single long-running case collapsing to zero. Quality is supplied by the
//! caller — typically the normalized score from a correctness metric.)

use serde::{Deserialize, Serialize};
use oneai_trace::{Span, SpanKind};

/// Per-case efficiency breakdown derived from the trace span tree + measured
/// wall-clock. This is the "efficiency axis" of the three-axis evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EfficiencyProfile {
    /// LLM inference wall-clock (sum of all LLM spans), in ms.
    pub inference_ms: u64,
    /// Number of LLM inference calls.
    pub inference_calls: usize,
    /// Tool execution wall-clock (sum of all TOOL spans), in ms.
    pub tool_ms: u64,
    /// Number of tool calls.
    pub tool_calls: usize,
    /// Agent wall-clock not attributable to inference or tools (context
    /// assembly, parsing, compression, scheduling, etc.).
    pub overhead_ms: u64,
    /// Total measured agent wall-clock, in ms.
    pub dur_ms: u64,

    /// ReAct iterations taken (from trace).
    pub iterations: usize,
    /// Total tokens consumed (prompt + completion).
    pub total_tokens: u64,
    /// Prompt tokens cached & read on the provider side (Anthropic
    /// `cache_read_input_tokens`). Populated by A4; 0 until then.
    pub cache_read_tokens: u64,
    /// Prompt tokens written into the cache (`cache_creation_input_tokens`).
    pub cache_creation_tokens: u64,
}

impl EfficiencyProfile {
    /// Build a profile from the trace span tree root + measured wall-clock.
    ///
    /// Sums LLM/TOOL span durations via [`Span::spans_by_kind`] (the same
    /// logic the SWE-bench runner used, now reusable). Token + cache fields
    /// are passed in by the caller from the usage tracker; pass 0 when
    /// unavailable.
    pub fn from_tree(
        root: &Span,
        dur_ms: u64,
        total_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        iterations: usize,
    ) -> Self {
        let sum_kind = |kind: SpanKind| -> (u64, usize) {
            let spans = root.spans_by_kind(kind);
            let total: u64 = spans.iter().filter_map(|s| s.duration_ms).sum();
            (total, spans.len())
        };
        let (inference_ms, inference_calls) = sum_kind(SpanKind::LLM);
        let (tool_ms, tool_calls) = sum_kind(SpanKind::TOOL);
        let attributed = inference_ms + tool_ms;
        let overhead_ms = dur_ms.saturating_sub(attributed);

        // Sum prompt-cache usage from LLM span events (the AgentLoop stamps
        // llm.cache_read_tokens / llm.cache_creation_tokens on the InferenceEnd
        // event of each LLM span). Prefer tree-derived values over the caller's
        // fallback (which is 0 when no per-call cache data was available).
        let (mut tree_cache_read, mut tree_cache_creation) = (0u64, 0u64);
        for llm in root.spans_by_kind(SpanKind::LLM) {
            for ev in &llm.events {
                if let Some(v) = ev.attributes.get("llm.cache_read_tokens").and_then(|v| v.as_u64()) {
                    tree_cache_read += v;
                }
                if let Some(v) = ev.attributes.get("llm.cache_creation_tokens").and_then(|v| v.as_u64()) {
                    tree_cache_creation += v;
                }
            }
        }
        let cache_read_tokens = if tree_cache_read > 0 { tree_cache_read } else { cache_read_tokens };
        let cache_creation_tokens = if tree_cache_creation > 0 { tree_cache_creation } else { cache_creation_tokens };

        Self {
            inference_ms,
            inference_calls,
            tool_ms,
            tool_calls,
            overhead_ms,
            dur_ms,
            iterations,
            total_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        }
    }

    /// Fraction of input tokens served from the prompt cache.
    ///
    /// `cache_read / (cache_read + cache_creation + total_tokens)`. Returns
    /// 0.0 when no cache data is present (A4 not wired).
    pub fn cache_hit_ratio(&self) -> f64 {
        let cached = self.cache_read_tokens + self.cache_creation_tokens;
        if cached == 0 {
            return 0.0;
        }
        let denom = (cached + self.total_tokens).max(1) as f64;
        self.cache_read_tokens as f64 / denom
    }

    /// Average tokens per iteration — a compact "verbosity per step" signal.
    pub fn tokens_per_iter(&self) -> f64 {
        if self.iterations == 0 {
            0.0
        } else {
            self.total_tokens as f64 / self.iterations as f64
        }
    }

    /// Share of agent wall-clock spent in the LLM (vs tools/overhead).
    pub fn inference_ratio(&self) -> f64 {
        if self.dur_ms == 0 {
            0.0
        } else {
            self.inference_ms as f64 / self.dur_ms as f64
        }
    }

    /// Three-axis efficiency score: quality normalized by a log-scaled
    /// token+latency cost. Returns a value in [0, 1] when quality ∈ [0,1].
    ///
    /// `quality / (1 + 0.1*log10(1+tokens) + 0.1*log10(1+latency_ms))`
    pub fn three_axis_score(&self, quality: f64) -> f64 {
        let token_cost = 0.1 * log10_1p(self.total_tokens as f64);
        let latency_cost = 0.1 * log10_1p(self.dur_ms as f64);
        let denom = 1.0 + token_cost + latency_cost;
        (quality / denom).clamp(0.0, 1.0)
    }
}

fn log10_1p(x: f64) -> f64 {
    if x <= 0.0 {
        0.0
    } else {
        (1.0 + x).log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_axis_decreases_with_cost() {
        let cheap = EfficiencyProfile {
            dur_ms: 100,
            total_tokens: 100,
            ..Default::default()
        };
        let pricey = EfficiencyProfile {
            dur_ms: 50_000,
            total_tokens: 200_000,
            ..Default::default()
        };
        assert!(cheap.three_axis_score(1.0) > pricey.three_axis_score(1.0));
        assert!(cheap.three_axis_score(1.0) <= 1.0);
        assert!(pricey.three_axis_score(1.0) > 0.0);
    }

    #[test]
    fn cache_hit_ratio_zero_without_cache() {
        let p = EfficiencyProfile {
            total_tokens: 1000,
            ..Default::default()
        };
        assert_eq!(p.cache_hit_ratio(), 0.0);
    }

    #[test]
    fn tokens_per_iter_handles_zero() {
        assert_eq!(EfficiencyProfile::default().tokens_per_iter(), 0.0);
        let p = EfficiencyProfile {
            total_tokens: 500,
            iterations: 5,
            ..Default::default()
        };
        assert_eq!(p.tokens_per_iter(), 100.0);
    }
}
