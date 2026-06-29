//! EvalResult, EvalReport, and EvalSummary — evaluation output types.
//!
//! After running an EvalSuite, the EvalRunner produces an EvalReport
//! containing individual EvalResults for each case and an aggregated
//! EvalSummary. The report can be exported as JSON or Markdown.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::eval_metric::{EvalScore, MetricScore};
use oneai_trace::TraceMetrics;

// ─── EvalResult ──────────────────────────────────────────────────────────

/// The result of evaluating a single EvalCase.
///
/// Contains the agent's actual output, all metric scores, performance
/// metrics from tracing, and any error that occurred during execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalResult {
    /// The case ID that was evaluated.
    pub case_id: String,

    /// The original user input.
    pub input: String,

    /// The agent's actual output.
    pub actual_output: String,

    /// Scores from each metric.
    pub scores: Vec<MetricScore>,

    /// Performance metrics from the trace (token usage, latency, etc.).
    #[serde(default = "TraceMetrics::default")]
    pub trace_metrics: TraceMetrics,

    /// Duration of the evaluation run in milliseconds.
    pub duration_ms: u64,

    /// Number of LLM API calls made for this case (= UsageSummary.call_count).
    #[serde(default)]
    pub api_calls: u64,

    /// How many of the API calls had **estimated** (client-side) token counts
    /// rather than provider-reported usage. Non-zero means the provider returned
    /// no usage in (streaming) responses and the loop counted tokens locally —
    /// so tokens for those calls are approximations. Surfaced so reports
    /// can flag that part of the usage axis is estimated.
    #[serde(default)]
    pub estimated_calls: u64,

    /// Prompt tokens consumed by this case.
    #[serde(default)]
    pub prompt_tokens: u64,

    /// Completion tokens consumed by this case.
    #[serde(default)]
    pub completion_tokens: u64,

    /// Free-form per-result metadata (e.g. SWE-bench `patch` / `tests_status` /
    /// `base_commit`). Backward-compatible key-value store so domain-specific
    /// runners can stash side data without adding typed fields here.
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// Error that occurred during execution (if any).
    #[serde(default)]
    pub error: Option<String>,
}

impl EvalResult {
    /// Create a new eval result.
    pub fn new(case_id: impl Into<String>, input: impl Into<String>, actual_output: impl Into<String>) -> Self {
        Self {
            case_id: case_id.into(),
            input: input.into(),
            actual_output: actual_output.into(),
            scores: Vec::new(),
            trace_metrics: TraceMetrics::default(),
            duration_ms: 0,
            api_calls: 0,
            estimated_calls: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            metadata: HashMap::new(),
            error: None,
        }
    }

    /// Whether this result passed all metrics.
    pub fn passed(&self) -> bool {
        self.error.is_none() && self.scores.iter().all(|ms| ms.score.passed)
    }

    /// Whether this result had an execution error.
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// Get the average normalized score across all metrics.
    pub fn avg_score(&self) -> f64 {
        if self.scores.is_empty() {
            return 0.0;
        }
        self.scores.iter().map(|ms| ms.score.normalized()).sum::<f64>() / self.scores.len() as f64
    }

    /// Add a metric score.
    pub fn add_score(&mut self, metric_name: impl Into<String>, score: EvalScore) {
        self.scores.push(MetricScore {
            metric_name: metric_name.into(),
            score,
        });
    }

    /// Set a metadata key-value pair (e.g. `patch`, `resolved`, `tests_status`).
    pub fn set_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.metadata.insert(key.into(), value.into());
    }

    /// Whether a named metric passed (false if no such metric exists).
    pub fn metric_passed(&self, metric_name: &str) -> bool {
        self.scores.iter()
            .any(|ms| ms.metric_name == metric_name && ms.score.passed)
    }
}

// ─── MetricSummary ───────────────────────────────────────────────────────

/// Summary statistics for a single metric across all cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSummary {
    /// The metric name.
    pub name: String,

    /// Number of cases evaluated by this metric.
    pub case_count: usize,

    /// Number of cases that passed this metric.
    pub pass_count: usize,

    /// Pass rate (pass_count / case_count).
    pub pass_rate: f64,

    /// Average normalized score.
    pub avg_score: f64,

    /// Minimum normalized score.
    pub min_score: f64,

    /// Maximum normalized score.
    pub max_score: f64,
}

impl MetricSummary {
    /// Compute a summary from a list of metric scores.
    pub fn compute(name: &str, scores: &[EvalScore]) -> Self {
        let case_count = scores.len();
        let pass_count = scores.iter().filter(|s| s.passed).count();
        let normalized = scores.iter().map(|s| s.normalized()).collect::<Vec<_>>();

        Self {
            name: name.to_string(),
            case_count,
            pass_count,
            pass_rate: if case_count > 0 { pass_count as f64 / case_count as f64 } else { 0.0 },
            avg_score: if normalized.is_empty() { 0.0 } else { normalized.iter().sum::<f64>() / normalized.len() as f64 },
            min_score: normalized.iter().copied().fold(f64::INFINITY, f64::min),
            max_score: normalized.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        }
    }
}

// ─── EvalSummary ─────────────────────────────────────────────────────────

/// Aggregated summary across all cases in an EvalReport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSummary {
    /// Total number of cases evaluated.
    pub total_cases: usize,

    /// Number of cases that passed all metrics.
    pub passed_cases: usize,

    /// Overall pass rate.
    pub pass_rate: f64,

    /// Average normalized score across all cases.
    pub avg_score: f64,

    /// Average duration per case (ms).
    pub avg_duration_ms: u64,

    /// Total token consumption across all cases.
    pub total_tokens: u64,

    /// Total LLM API calls across all cases.
    #[serde(default)]
    pub total_api_calls: u64,

    /// Summary per metric.
    pub metric_summaries: HashMap<String, MetricSummary>,
}

impl EvalSummary {
    /// Compute a summary from a list of eval results.
    pub fn compute(results: &[EvalResult]) -> Self {
        let total_cases = results.len();
        let passed_cases = results.iter().filter(|r| r.passed()).count();
        let pass_rate = if total_cases > 0 { passed_cases as f64 / total_cases as f64 } else { 0.0 };
        let avg_score = if total_cases > 0 { results.iter().map(|r| r.avg_score()).sum::<f64>() / total_cases as f64 } else { 0.0 };

        let total_duration: u64 = results.iter().map(|r| r.duration_ms).sum();
        let avg_duration_ms = if total_cases > 0 { total_duration / total_cases as u64 } else { 0 };

        let total_tokens: u64 = results.iter().map(|r| r.trace_metrics.total_tokens).sum();

        let total_api_calls: u64 = results.iter().map(|r| r.api_calls).sum();

        // Compute per-metric summaries
        let mut metric_scores: HashMap<String, Vec<EvalScore>> = HashMap::new();
        for result in results {
            for ms in &result.scores {
                metric_scores.entry(ms.metric_name.clone())
                    .or_default()
                    .push(ms.score.clone());
            }
        }

        let metric_summaries = metric_scores.into_iter()
            .map(|(name, scores)| (name.clone(), MetricSummary::compute(&name, &scores)))
            .collect();

        Self {
            total_cases,
            passed_cases,
            pass_rate,
            avg_score,
            avg_duration_ms,
            total_tokens,
            total_api_calls,
            metric_summaries,
        }
    }
}

// ─── EvalReport ──────────────────────────────────────────────────────────

/// A complete evaluation report — suite results + summary.
///
/// The primary output of the EvalRunner. Contains individual results
/// for each case and an aggregated summary. Can be exported as JSON
/// or Markdown for review and CI integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    /// The suite name that was evaluated.
    pub suite_name: String,

    /// Individual case results.
    pub results: Vec<EvalResult>,

    /// Aggregated summary statistics.
    pub summary: EvalSummary,

    /// When the evaluation was run.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl EvalReport {
    /// Create a new report from results.
    pub fn new(suite_name: impl Into<String>, results: Vec<EvalResult>) -> Self {
        let summary = EvalSummary::compute(&results);
        Self {
            suite_name: suite_name.into(),
            results,
            summary,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Export as JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Export as Markdown (see report_format.rs for details).
    pub fn to_markdown(&self) -> String {
        crate::report_format::render_markdown(self)
    }

    /// Overall pass rate.
    pub fn pass_rate(&self) -> f64 {
        self.summary.pass_rate
    }

    /// Whether all cases passed.
    pub fn all_passed(&self) -> bool {
        self.summary.pass_rate == 1.0
    }

    /// Number of failed cases.
    pub fn failed_count(&self) -> usize {
        self.summary.total_cases - self.summary.passed_cases
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eval_result_new() {
        let result = EvalResult::new("case_1", "What is 2+2?", "4");
        assert_eq!(result.case_id, "case_1");
        assert_eq!(result.input, "What is 2+2?");
        assert_eq!(result.actual_output, "4");
        assert!(result.scores.is_empty());
        assert!(!result.has_error());
        // Usage fields default to zero (no usage tracker wired).
        assert_eq!(result.api_calls, 0);
        assert_eq!(result.prompt_tokens, 0);
        assert_eq!(result.completion_tokens, 0);
        assert!(result.metadata.is_empty());
    }

    #[test]
    fn test_summary_aggregates_usage_and_api_calls() {
        let mut r1 = EvalResult::new("c1", "in", "out");
        r1.api_calls = 3;
        r1.prompt_tokens = 100;
        r1.completion_tokens = 50;
        r1.add_score("m", EvalScore::perfect("ok"));

        let mut r2 = EvalResult::new("c2", "in", "out");
        r2.api_calls = 7;
        r2.prompt_tokens = 200;
        r2.completion_tokens = 100;
        r2.add_score("m", EvalScore::perfect("ok"));

        let report = EvalReport::new("usage_suite", vec![r1, r2]);
        let s = &report.summary;
        assert_eq!(s.total_cases, 2);
        assert_eq!(s.total_api_calls, 10);
    }

    #[test]
    fn test_eval_result_passed() {
        let mut result = EvalResult::new("case_1", "test", "4");
        result.add_score("exact_match", EvalScore::perfect("Matched"));
        result.add_score("contains", EvalScore::perfect("Contains"));
        assert!(result.passed());
    }

    #[test]
    fn test_eval_result_failed() {
        let mut result = EvalResult::new("case_1", "test", "wrong");
        result.add_score("exact_match", EvalScore::zero("No match"));
        assert!(!result.passed());
    }

    #[test]
    fn test_eval_result_with_error() {
        let mut result = EvalResult::new("case_1", "test", "");
        result.error = Some("Provider unavailable".to_string());
        assert!(result.has_error());
        assert!(!result.passed());
    }

    #[test]
    fn test_eval_result_avg_score() {
        let mut result = EvalResult::new("case_1", "test", "partial");
        result.add_score("metric_1", EvalScore::new(7.0, 10.0, "Partial", true));
        result.add_score("metric_2", EvalScore::new(8.0, 10.0, "Good", true));
        assert_eq!(result.avg_score(), 0.75);
    }

    #[test]
    fn test_metric_summary_compute() {
        let scores = vec![
            EvalScore::perfect("Match"),
            EvalScore::zero("No match"),
            EvalScore::new(7.0, 10.0, "Partial", true),
        ];
        let summary = MetricSummary::compute("exact_match", &scores);
        assert_eq!(summary.case_count, 3);
        assert_eq!(summary.pass_count, 2);
        assert_eq!(summary.pass_rate, 2.0 / 3.0);
        assert!(summary.avg_score > 0.0);
        assert_eq!(summary.min_score, 0.0);
        assert_eq!(summary.max_score, 1.0);
    }

    #[test]
    fn test_eval_summary_compute() {
        let mut r1 = EvalResult::new("c1", "test", "4");
        r1.add_score("exact", EvalScore::perfect("OK"));
        r1.duration_ms = 100;

        let mut r2 = EvalResult::new("c2", "test", "15");
        r2.add_score("exact", EvalScore::perfect("OK"));
        r2.duration_ms = 200;

        let summary = EvalSummary::compute(&[r1, r2]);
        assert_eq!(summary.total_cases, 2);
        assert_eq!(summary.passed_cases, 2);
        assert_eq!(summary.pass_rate, 1.0);
        assert_eq!(summary.avg_duration_ms, 150);
    }

    #[test]
    fn test_eval_report() {
        let mut r1 = EvalResult::new("c1", "2+2?", "4");
        r1.add_score("exact", EvalScore::perfect("OK"));

        let report = EvalReport::new("math_test", vec![r1]);
        assert_eq!(report.suite_name, "math_test");
        assert!(report.all_passed());
        assert_eq!(report.pass_rate(), 1.0);
    }

    #[test]
    fn test_eval_report_json() {
        let mut r1 = EvalResult::new("c1", "test", "4");
        r1.add_score("exact", EvalScore::perfect("OK"));

        let report = EvalReport::new("test", vec![r1]);
        let json = report.to_json().unwrap();
        assert!(json.contains("test"));
        assert!(json.contains("exact"));
    }
}
