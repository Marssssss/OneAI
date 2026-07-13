//! EvalMetric trait and EvalScore — the scoring strategy abstraction.
//!
//! An EvalMetric defines how to score an agent's output against an expected
//! output. The framework provides several built-in metrics (exact match,
//! contains, regex, LLM judge, trajectory) and supports custom metrics
//! via the trait.
//!
//! EvalJudge is a simpler trait for custom evaluation logic — it only
//! receives input + actual output and returns a score. This is used by
//! ExpectedOutput::Custom.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::eval_case::ExpectedOutput;

// ─── EvalScore ───────────────────────────────────────────────────────────

/// The score result from applying an EvalMetric to a case.
///
/// Each score has:
/// - `value`: the numeric score achieved (0.0 to max_value)
/// - `max_value`: the maximum possible score for this metric
/// - `reason`: human-readable explanation of why this score was given
/// - `passed`: whether the score meets the passing threshold
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalScore {
    /// The numeric score achieved.
    pub value: f64,

    /// The maximum possible score.
    pub max_value: f64,

    /// Human-readable explanation.
    pub reason: String,

    /// Whether this score passes the threshold.
    pub passed: bool,
}

impl EvalScore {
    /// Create a passing score with full marks.
    pub fn perfect(reason: impl Into<String>) -> Self {
        Self {
            value: 1.0,
            max_value: 1.0,
            reason: reason.into(),
            passed: true,
        }
    }

    /// Create a failing score with zero marks.
    pub fn zero(reason: impl Into<String>) -> Self {
        Self {
            value: 0.0,
            max_value: 1.0,
            reason: reason.into(),
            passed: false,
        }
    }

    /// Create a score with specific values.
    pub fn new(value: f64, max_value: f64, reason: impl Into<String>, passed: bool) -> Self {
        Self {
            value,
            max_value,
            reason: reason.into(),
            passed,
        }
    }

    /// Create a score from a pass/fail boolean.
    pub fn from_bool(passed: bool, reason: impl Into<String>) -> Self {
        Self {
            value: if passed { 1.0 } else { 0.0 },
            max_value: 1.0,
            reason: reason.into(),
            passed,
        }
    }

    /// Get the normalized score (value / max_value).
    pub fn normalized(&self) -> f64 {
        if self.max_value == 0.0 {
            0.0
        } else {
            self.value / self.max_value
        }
    }
}

// ─── MetricScore ─────────────────────────────────────────────────────────

/// A score attributed to a specific metric (metric name + score).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MetricScore {
    /// The metric that produced this score.
    pub metric_name: String,

    /// The score value.
    pub score: EvalScore,
}

// ─── EvalMetric ──────────────────────────────────────────────────────────

/// Trait for evaluation scoring strategies.
///
/// Each metric receives the user input, the agent's actual output, and the
/// expected output, and returns an EvalScore. Metrics can be combined
/// (e.g., exact match + trajectory) for multi-dimensional evaluation.
///
/// Built-in metrics:
/// - `ExactMatchMetric`: exact string equality
/// - `ContainsMatchMetric`: substring presence check
/// - `RegexMatchMetric`: regex pattern match
/// - `LlmJudgeMetric`: LLM-as-judge scoring (needs provider)
/// - `TrajectoryMetric`: tool call sequence check
/// - `CompositeMetric`: weighted combination of multiple metrics
///
/// Implement this trait to create domain-specific evaluation strategies.
#[async_trait]
pub trait EvalMetric: Send + Sync {
    /// The name of this metric (for identification in reports).
    fn name(&self) -> &str;

    /// A brief description of what this metric measures.
    fn description(&self) -> &str;

    /// Score the agent's output against the expected output.
    ///
    /// Parameters:
    /// - `input`: the original user input/task
    /// - `actual`: the agent's actual output
    /// - `expected`: the expected output definition
    ///
    /// Returns an `EvalScore` indicating the quality of the output.
    async fn score(&self, input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore;

    /// Score with access to the full trace span tree (efficiency / trajectory
    /// axis). Default delegates to [`EvalMetric::score`], ignoring the tree —
    /// so metrics that only inspect output text are unaffected. Override to
    /// use real trace data: e.g. `TrajectoryMetric` walks TOOL spans for the
    /// actual tool-call sequence, `EfficiencyMetric` reads LLM/TOOL durations
    /// + token/iteration counts.
    ///
    /// This is an additive, semver-minor extension: existing implementors
    /// inherit the default and keep working.
    async fn score_with_trace(
        &self,
        input: &str,
        actual: &str,
        expected: &ExpectedOutput,
        tree: Option<&oneai_trace::TraceTree>,
    ) -> EvalScore {
        let _ = tree;
        self.score(input, actual, expected).await
    }
}

// ─── EvalJudge ───────────────────────────────────────────────────────────

/// Trait for custom evaluation judges — used by ExpectedOutput::Custom.
///
/// This is a simpler interface than EvalMetric: it only receives
/// the input and actual output, and returns a score. This is useful
/// for domain-specific evaluation logic that doesn't need to inspect
/// the ExpectedOutput structure.
///
/// Usage:
/// ```ignore
/// struct MyJudge;
/// impl EvalJudge for MyJudge {
///     async fn judge(&self, input: &str, actual: &str) -> EvalScore {
///         // Custom logic...
///         EvalScore::from_bool(actual.contains("expected"), "Contains expected keyword")
///     }
/// }
///
/// let case = EvalCase::new("test", ExpectedOutput::Custom {
///     judge: Arc::new(MyJudge),
/// });
/// ```
#[async_trait]
pub trait EvalJudge: Send + Sync {
    /// Judge the agent's output for the given input.
    ///
    /// Returns an EvalScore indicating the quality of the output.
    async fn judge(&self, input: &str, actual: &str) -> EvalScore;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eval_score_perfect() {
        let score = EvalScore::perfect("Exact match");
        assert_eq!(score.value, 1.0);
        assert_eq!(score.max_value, 1.0);
        assert!(score.passed);
        assert_eq!(score.normalized(), 1.0);
    }

    #[test]
    fn test_eval_score_zero() {
        let score = EvalScore::zero("No match");
        assert_eq!(score.value, 0.0);
        assert_eq!(score.max_value, 1.0);
        assert!(!score.passed);
        assert_eq!(score.normalized(), 0.0);
    }

    #[test]
    fn test_eval_score_from_bool() {
        let pass = EvalScore::from_bool(true, "Contains keyword");
        assert!(pass.passed);
        assert_eq!(pass.value, 1.0);

        let fail = EvalScore::from_bool(false, "Missing keyword");
        assert!(!fail.passed);
        assert_eq!(fail.value, 0.0);
    }

    #[test]
    fn test_eval_score_normalized() {
        let score = EvalScore::new(7.0, 10.0, "Partial match", true);
        assert_eq!(score.normalized(), 0.7);
    }

    #[test]
    fn test_metric_score() {
        let ms = MetricScore {
            metric_name: "exact_match".to_string(),
            score: EvalScore::perfect("Matched"),
        };
        assert_eq!(ms.metric_name, "exact_match");
        assert!(ms.score.passed);
    }
}
