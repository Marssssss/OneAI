//! EvalRunner — the execution engine for running eval suites.
//!
//! The EvalRunner takes an EvalSuite and an App (or AppBuilder),
//! runs each case through the agent loop, collects trace metrics,
//! applies scoring metrics, and produces an EvalReport.
//!
//! ## Architecture
//!
//! For each EvalCase:
//! 1. Create a new AppSession
//! 2. Send the input to the agent via `run_agent_silent()`
//! 3. Collect the agent's output + TraceMetrics
//! 4. Apply each EvalMetric to score the output
//! 5. Record the result (EvalResult)
//!
//! ## Modes
//!
//! - **Full mode**: Requires an LLM provider — runs the actual agent loop
//! - **Metric-only mode**: No provider needed — only applies metrics to
//!   pre-collected outputs (useful for re-scoring or batch evaluation)
//!
//! ## Concurrency
//!
//! Cases run sequentially by default (for deterministic evaluation).
//! Use `EvalRunnerConfig::concurrent` for parallel execution.

use std::sync::Arc;
use std::time::Instant;

use oneai_core::error::Result;
use oneai_trace::{TraceMetrics, TraceTree};

use crate::eval_case::{EvalCase, ExpectedOutput};
use crate::efficiency::EfficiencyProfile;
use crate::eval_metric::EvalMetric;
use crate::eval_result::{EvalResult, EvalReport};
use crate::eval_suite::EvalSuite;
// ─── EvalRunnerConfig ────────────────────────────────────────────────────

/// Configuration for the EvalRunner.
#[derive(Debug, Clone)]
pub struct EvalRunnerConfig {
    /// Maximum concurrent case executions (default: 1 = sequential).
    pub max_concurrent: usize,

    /// Whether to collect trace metrics for each case (default: true).
    pub collect_traces: bool,

    /// Number of retries per case on failure (default: 0).
    pub retry_count: usize,

    /// Timeout per case in milliseconds (default: 30000 = 30s).
    pub timeout_ms: u64,
}

impl Default for EvalRunnerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            collect_traces: true,
            retry_count: 0,
            timeout_ms: 30000,
        }
    }
}

impl EvalRunnerConfig {
    /// Create config with concurrent execution.
    pub fn concurrent(max: usize) -> Self {
        Self {
            max_concurrent: max,
            ..Self::default()
        }
    }

    /// Create config with retries.
    pub fn with_retries(count: usize) -> Self {
        Self {
            retry_count: count,
            ..Self::default()
        }
    }
}

// ─── EvalRunner ──────────────────────────────────────────────────────────

/// The execution engine for running eval suites.
///
/// Takes a fully configured App and runs each case through the
/// agent loop, scoring the results and producing an EvalReport.
///
/// Usage:
/// ```ignore
/// let app = AppBuilder::new()
///     .provider(provider)
///     .noop_interaction_gate()
///     .trace_in_memory()
///     .build().await?;
///
/// let runner = EvalRunner::from_app(app);
/// let report = runner.run(&suite).await?;
/// println!("{}", report.to_markdown());
/// ```
pub struct EvalRunner {
    /// The app to run evaluations against.
    app: Arc<oneai_app::App>,
    /// Runner configuration.
    config: EvalRunnerConfig,
}

impl EvalRunner {
    /// Create an EvalRunner from a fully configured App.
    pub fn from_app(app: oneai_app::App) -> Self {
        Self {
            app: Arc::new(app),
            config: EvalRunnerConfig::default(),
        }
    }

    /// Create an EvalRunner with custom configuration.
    pub fn with_config(app: oneai_app::App, config: EvalRunnerConfig) -> Self {
        Self {
            app: Arc::new(app),
            config,
        }
    }

    /// Run the evaluation suite and produce a report.
    ///
    /// Each case is run through the agent loop (if a provider is configured),
    /// then scored by the suite's metrics. The results are aggregated into
    /// an EvalReport.
    ///
    /// For cases with `ExpectedOutput::Trajectory`, trace collection is
    /// always enabled (needed to check tool calls).
    pub async fn run(&self, suite: &EvalSuite) -> Result<EvalReport> {
        let mut results = Vec::new();

        for case in &suite.cases {
            let result = self.run_case(case, &suite.metrics).await;
            results.push(result);
        }

        Ok(EvalReport::new(&suite.name, results))
    }

    /// Run a single evaluation case.
    ///
    /// This is the core loop:
    /// 1. Create session with tracing enabled
    /// 2. Run the agent (or skip if no provider)
    /// 3. Collect output + trace metrics + cost
    /// 4. Apply each metric
    /// 5. Return the result
    async fn run_case(
        &self,
        case: &EvalCase,
        metrics: &[Arc<dyn EvalMetric>],
    ) -> EvalResult {
        let start = Instant::now();
        let mut result = EvalResult::new(&case.id, &case.input, "");

        // Run the agent if a provider is configured. run_agent_for_case
        // returns the trace tree (when tracing is on) so trace-aware metrics
        // (TrajectoryMetric, EfficiencyMetric) can walk the span tree.
        let tree = if self.app.has_provider() {
            self.run_agent_for_case(case, &mut result).await
        } else {
            // No provider — can't run the agent, mark as error
            result.error = Some("No LLM provider configured".to_string());
            None
        };

        result.duration_ms = start.elapsed().as_millis() as u64;

        // Apply each metric to score the output. Trace-aware metrics receive
        // the span tree; text-only metrics ignore it via the default impl.
        for metric in metrics {
            let score = metric
                .score_with_trace(&case.input, &result.actual_output, &case.expected, tree.as_ref())
                .await;
            result.add_score(metric.name(), score);
        }

        result
    }

    /// Run the agent loop for a single case and collect output + telemetry.
    ///
    /// Creates a fresh session with in-memory tracing, runs the agent, then
    /// writes the final answer, trace metrics, efficiency profile, and
    /// per-case usage into `result`. Returns the trace tree (so trace-aware
    /// metrics can walk the span tree) when tracing is enabled.
    ///
    /// Usage isolation: the session id is new per case, and we clear any prior
    /// records for it before running so concurrent/sequential cases don't bleed
    /// usage into each other. A single `session_usage` call yields api_calls
    /// + token breakdown (the UsageSummary aggregates the UsageRecords the
    /// AgentLoop already records after each inference).
    async fn run_agent_for_case(&self, case: &EvalCase, result: &mut EvalResult) -> Option<TraceTree> {
        let mut session = self.app.create_session();
        let session_id = session.session_id().to_string();
        let mut tree_out: Option<TraceTree> = None;

        // Isolate this case's usage accounting.
        if let Some(ct) = &self.app.usage_tracker {
            let _ = ct.clear_session(&session_id).await;
        }

        // Measure the agent wall-clock (excluding metric scoring) so the
        // efficiency breakdown attributes overhead correctly.
        let agent_start = Instant::now();
        let agent_result = session.run_agent_silent(&case.input).await;
        let agent_dur_ms = agent_start.elapsed().as_millis() as u64;

        match agent_result {
            Ok(loop_result) => {
                result.actual_output = loop_result.final_answer;

                // Collect trace metrics (previously computed then discarded —
                // the TODO flagged during the eval audit). Now wired into the
                // result, feeding the efficiency axis (tokens, tool_calls,
                // iterations, retries, latency).
                if self.config.collect_traces {
                    if let Some(ctx) = session.trace_context() {
                        let tree = ctx.build_tree();
                        result.trace_metrics = TraceMetrics::compute_from_tree(&tree.root_span);
                        // Efficiency axis: decompose the agent wall-clock into
                        // inference vs tool vs overhead straight from the span
                        // tree (generalized from the SWE-bench runner). Token +
                        // cache fields filled from usage below; cache stays 0
                        // until A4 wires cache_read_input_tokens.
                        result.efficiency = Some(EfficiencyProfile::from_tree(
                            &tree.root_span,
                            agent_dur_ms,
                            0, // total_tokens set below after usage fetch
                            0,
                            0,
                            result.trace_metrics.avg_iterations.round() as usize,
                        ));
                        tree_out = Some(tree);
                    }
                }
            }
            Err(e) => {
                // Preserve historical behavior: embed the error in the output
                // so metrics can still score it. (We do not set result.error
                // here — that path is reserved for "no provider".)
                result.actual_output = format!("ERROR: {}", e);
            }
        }

        // Collect the usage axis: api_calls + token breakdown.
        if let Some(ct) = &self.app.usage_tracker {
            if let Ok(summary) = ct.session_usage(&session_id).await {
                result.api_calls = summary.call_count;
                result.estimated_calls = summary.estimated_call_count;
                result.prompt_tokens = summary.prompt_tokens;
                result.completion_tokens = summary.completion_tokens;
                // Backfill token totals into the efficiency profile now that
                // usage is known.
                if let Some(p) = result.efficiency.as_mut() {
                    p.total_tokens = summary.prompt_tokens + summary.completion_tokens;
                }
            }
        }

        tree_out
    }

    /// Run metrics only (no agent execution) — for re-scoring pre-collected outputs.
    ///
    /// This is useful when you have pre-collected agent outputs and want to
    /// score them with different metrics, or when doing batch evaluation
    /// where agent execution was done separately.
    pub async fn score_only(
        &self,
        cases: &[(EvalCase, String)], // (case, actual_output) pairs
        metrics: &[Arc<dyn EvalMetric>],
        suite_name: &str,
    ) -> EvalReport {
        let mut results = Vec::new();

        for (case, actual_output) in cases {
            let mut result = EvalResult::new(&case.id, &case.input, actual_output);

            for metric in metrics {
                let score = metric.score(&case.input, actual_output, &case.expected).await;
                result.add_score(metric.name(), score);
            }

            results.push(result);
        }

        EvalReport::new(suite_name, results)
    }
}

// ─── ScoreOnlyRunner ─────────────────────────────────────────────────────

/// A simplified runner that only applies metrics without running the agent.
///
/// Useful for unit-testing metrics, re-scoring outputs, or CI integration
/// where agent execution is done in a separate step.
pub struct ScoreOnlyRunner;

impl ScoreOnlyRunner {
    /// Score a set of (input, actual_output, expected) pairs.
    pub async fn score(
        cases: &[(String, String, ExpectedOutput)], // (input, actual, expected)
        metrics: &[Arc<dyn EvalMetric>],
        suite_name: &str,
    ) -> EvalReport {
        let mut results = Vec::new();

        for (idx, (input, actual, expected)) in cases.iter().enumerate() {
            let mut result = EvalResult::new(
                format!("case_{}", idx),
                input,
                actual,
            );

            for metric in metrics {
                let score = metric.score(input, actual, expected).await;
                result.add_score(metric.name(), score);
            }

            results.push(result);
        }

        EvalReport::new(suite_name, results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval_case::ExpectedOutput;
    use crate::builtin_metrics::{ExactMatchMetric, ContainsMatchMetric};

    #[tokio::test]
    async fn test_score_only_runner() {
        // Use metrics that match their ExpectedOutput types
        let metrics: Vec<Arc<dyn EvalMetric>> = vec![
            Arc::new(ExactMatchMetric),
        ];

        let cases = vec![
            ("What is 2+2?".to_string(), "4".to_string(), ExpectedOutput::exact("4")),
            ("What is 3*5?".to_string(), "15".to_string(), ExpectedOutput::exact("15")),
            ("Hello world".to_string(), "wrong".to_string(), ExpectedOutput::exact("correct")),
        ];

        let report = ScoreOnlyRunner::score(&cases, &metrics, "test_suite").await;

        assert_eq!(report.summary.total_cases, 3);
        assert_eq!(report.summary.passed_cases, 2); // case 0 + case 1 pass exact match, case 2 fails
        assert!(report.summary.pass_rate > 0.5);
    }

    #[tokio::test]
    async fn test_score_only_all_pass() {
        let metrics: Vec<Arc<dyn EvalMetric>> = vec![Arc::new(ExactMatchMetric)];
        let cases = vec![
            ("2+2?".to_string(), "4".to_string(), ExpectedOutput::exact("4")),
            ("3+3?".to_string(), "6".to_string(), ExpectedOutput::exact("6")),
        ];

        let report = ScoreOnlyRunner::score(&cases, &metrics, "exact_test").await;
        assert!(report.all_passed());
        assert_eq!(report.pass_rate(), 1.0);
    }

    #[tokio::test]
    async fn test_score_only_with_contains() {
        let metrics: Vec<Arc<dyn EvalMetric>> = vec![Arc::new(ContainsMatchMetric)];
        let cases = vec![
            ("Explain Rust".to_string(), "Rust is a memory-safe language".to_string(),
             ExpectedOutput::contains(["memory", "safe"])),
        ];

        let report = ScoreOnlyRunner::score(&cases, &metrics, "contains_test").await;
        assert!(report.all_passed());
    }
}
