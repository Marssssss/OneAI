//! Built-in evaluation metrics — common scoring strategies.
//!
//! Provides the following metrics:
//! - **ExactMatchMetric**: Exact string equality comparison
//! - **ContainsMatchMetric**: Substring presence check
//! - **RegexMatchMetric**: Regex pattern matching
//! - **LlmJudgeMetric**: LLM-as-judge scoring (requires provider)
//! - **TrajectoryMetric**: Tool call sequence validation
//! - **CompositeMetric**: Weighted combination of multiple metrics

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use oneai_trace::{SpanKind, TraceMetrics, TraceTree};

use crate::eval_case::ExpectedOutput;
use crate::eval_metric::{EvalMetric, EvalScore};

// ─── ExactMatchMetric ────────────────────────────────────────────────────

/// Metric that checks for exact string equality.
///
/// Only applies to `ExpectedOutput::Exact` cases. For other expected
/// output types, it returns a zero score with a mismatch reason.
///
/// Comparison ignores leading/trailing whitespace.
pub struct ExactMatchMetric;

#[async_trait]
impl EvalMetric for ExactMatchMetric {
    fn name(&self) -> &str { "exact_match" }

    fn description(&self) -> &str {
        "Checks whether the output exactly matches the expected answer"
    }

    async fn score(&self, _input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore {
        match expected {
            ExpectedOutput::Exact { answer } => {
                let actual_trimmed = actual.trim();
                let expected_trimmed = answer.trim();
                if actual_trimmed == expected_trimmed {
                    EvalScore::perfect("Exact match")
                } else {
                    EvalScore::new(
                        0.0,
                        1.0,
                        format!("Expected '{}' but got '{}'", expected_trimmed, actual_trimmed),
                        false,
                    )
                }
            }
            ExpectedOutput::Contains { .. } => {
                // Not applicable — return skip score
                EvalScore::new(0.0, 1.0, "ExactMatch not applicable for Contains expected output", false)
            }
            ExpectedOutput::Regex { .. } => {
                EvalScore::new(0.0, 1.0, "ExactMatch not applicable for Regex expected output", false)
            }
            ExpectedOutput::LlmJudge { .. } => {
                EvalScore::new(0.0, 1.0, "ExactMatch not applicable for LlmJudge expected output", false)
            }
            ExpectedOutput::Trajectory { .. } => {
                EvalScore::new(0.0, 1.0, "ExactMatch not applicable for Trajectory expected output", false)
            }
            ExpectedOutput::Custom { .. } => {
                EvalScore::new(0.0, 1.0, "ExactMatch not applicable for Custom expected output", false)
            }
        }
    }
}

// ─── ContainsMatchMetric ─────────────────────────────────────────────────

/// Metric that checks whether all specified substrings appear in the output.
///
/// Applies to `ExpectedOutput::Contains` cases. Each substring must be
/// present in the output. The score is the fraction of substrings found.
pub struct ContainsMatchMetric;

#[async_trait]
impl EvalMetric for ContainsMatchMetric {
    fn name(&self) -> &str { "contains_match" }

    fn description(&self) -> &str {
        "Checks whether all required substrings appear in the output"
    }

    async fn score(&self, _input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore {
        match expected {
            ExpectedOutput::Exact { .. } => {
                EvalScore::new(0.0, 1.0, "ContainsMatch not applicable for Exact expected output", false)
            }
            ExpectedOutput::Contains { substrings, case_sensitive } => {
                let found = substrings.iter().filter(|s| {
                    if *case_sensitive {
                        actual.contains(*s)
                    } else {
                        actual.to_lowercase().contains(&s.to_lowercase())
                    }
                }).count();

                if found == substrings.len() {
                    EvalScore::perfect(format!("All {} substrings found", found))
                } else {
                    let missing: Vec<&str> = substrings.iter()
                        .filter(|s| {
                            if *case_sensitive {
                                !actual.contains(*s)
                            } else {
                                !actual.to_lowercase().contains(&s.to_lowercase())
                            }
                        })
                        .map(|s| s.as_str())
                        .collect();

                    EvalScore::new(
                        found as f64 / substrings.len() as f64,
                        1.0,
                        format!("Found {} of {} substrings. Missing: {}", found, substrings.len(), missing.join(", ")),
                        found == substrings.len(),
                    )
                }
            }
            ExpectedOutput::Regex { .. } => {
                EvalScore::new(0.0, 1.0, "ContainsMatch not applicable for Regex expected output", false)
            }
            ExpectedOutput::LlmJudge { .. } => {
                EvalScore::new(0.0, 1.0, "ContainsMatch not applicable for LlmJudge expected output", false)
            }
            ExpectedOutput::Trajectory { .. } => {
                EvalScore::new(0.0, 1.0, "ContainsMatch not applicable for Trajectory expected output", false)
            }
            ExpectedOutput::Custom { .. } => {
                EvalScore::new(0.0, 1.0, "ContainsMatch not applicable for Custom expected output", false)
            }
        }
    }
}

// ─── RegexMatchMetric ────────────────────────────────────────────────────

/// Metric that checks whether the output matches a regex pattern.
///
/// Applies to `ExpectedOutput::Regex` cases. Returns 1.0 if the
/// pattern matches anywhere in the output, 0.0 if not.
pub struct RegexMatchMetric;

#[async_trait]
impl EvalMetric for RegexMatchMetric {
    fn name(&self) -> &str { "regex_match" }

    fn description(&self) -> &str {
        "Checks whether the output matches the expected regex pattern"
    }

    async fn score(&self, _input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore {
        match expected {
            ExpectedOutput::Regex { pattern } => {
                let re = regex::Regex::new(pattern);
                match re {
                    Ok(re) => {
                        if re.is_match(actual) {
                            EvalScore::perfect(format!("Pattern '{}' matched", pattern))
                        } else {
                            EvalScore::zero(format!("Pattern '{}' not found in output", pattern))
                        }
                    }
                    Err(e) => {
                        EvalScore::new(0.0, 1.0, format!("Invalid regex pattern '{}': {}", pattern, e), false)
                    }
                }
            }
            ExpectedOutput::Exact { .. } => {
                EvalScore::new(0.0, 1.0, "RegexMatch not applicable for Exact expected output", false)
            }
            ExpectedOutput::Contains { .. } => {
                EvalScore::new(0.0, 1.0, "RegexMatch not applicable for Contains expected output", false)
            }
            ExpectedOutput::LlmJudge { .. } => {
                EvalScore::new(0.0, 1.0, "RegexMatch not applicable for LlmJudge expected output", false)
            }
            ExpectedOutput::Trajectory { .. } => {
                EvalScore::new(0.0, 1.0, "RegexMatch not applicable for Trajectory expected output", false)
            }
            ExpectedOutput::Custom { .. } => {
                EvalScore::new(0.0, 1.0, "RegexMatch not applicable for Custom expected output", false)
            }
        }
    }
}

// ─── TrajectoryMetric ────────────────────────────────────────────────────

/// Metric that checks whether the agent used the expected tools.
///
/// Applies to `ExpectedOutput::Trajectory` cases. Checks that:
/// 1. All expected tools were called
/// 2. The number of iterations didn't exceed max_iterations
///
/// The score is based on how many of the expected tools were called.
/// If all were called, score = 1.0. If some were missing, score = fraction.
pub struct TrajectoryMetric;

#[async_trait]
impl EvalMetric for TrajectoryMetric {
    fn name(&self) -> &str { "trajectory" }

    fn description(&self) -> &str {
        "Checks whether the agent used the expected tools and stayed within iteration bounds"
    }

    async fn score(&self, _input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore {
        match expected {
            ExpectedOutput::Trajectory { expected_tools, max_iterations: _ } => {
                // Text-only fallback (metric-only mode, no trace). Checks tool
                // names as substrings in the output text — unreliable, but
                // the best we can do without the span tree. score_with_trace
                // (below) does the real check when a trace is available.
                let found: Vec<&str> = expected_tools.iter()
                    .filter(|tool| actual.contains(*tool))
                    .map(|s| s.as_str())
                    .collect();

                let missing: Vec<&str> = expected_tools.iter()
                    .filter(|tool| !actual.contains(*tool))
                    .map(|s| s.as_str())
                    .collect();

                let fraction = if expected_tools.is_empty() {
                    1.0
                } else {
                    found.len() as f64 / expected_tools.len() as f64
                };

                let reason = if missing.is_empty() {
                    format!("All {} expected tools called (text heuristic)", found.len())
                } else {
                    format!("Found {} of {} expected tools (text heuristic). Missing: {}", found.len(), expected_tools.len(), missing.join(", "))
                };

                EvalScore::new(fraction, 1.0, reason, missing.is_empty())
            }
            ExpectedOutput::Exact { .. } => {
                EvalScore::new(0.0, 1.0, "Trajectory not applicable for Exact expected output", false)
            }
            ExpectedOutput::Contains { .. } => {
                EvalScore::new(0.0, 1.0, "Trajectory not applicable for Contains expected output", false)
            }
            ExpectedOutput::Regex { .. } => {
                EvalScore::new(0.0, 1.0, "Trajectory not applicable for Regex expected output", false)
            }
            ExpectedOutput::LlmJudge { .. } => {
                EvalScore::new(0.0, 1.0, "Trajectory not applicable for LlmJudge expected output", false)
            }
            ExpectedOutput::Custom { .. } => {
                EvalScore::new(0.0, 1.0, "Trajectory not applicable for Custom expected output", false)
            }
        }
    }

    /// Real trajectory check using the trace span tree: walks TOOL spans,
    /// extracts each tool's `tool.name` attribute, and verifies the expected
    /// tool set was actually invoked. Also enforces the `max_iterations` bound
    /// (previously an unused `_` — the TODO is now resolved). Falls back to
    /// the text heuristic when no trace is available (metric-only mode).
    async fn score_with_trace(
        &self,
        input: &str,
        actual: &str,
        expected: &ExpectedOutput,
        tree: Option<&TraceTree>,
    ) -> EvalScore {
        match expected {
            ExpectedOutput::Trajectory { expected_tools, max_iterations } => {
                if let Some(tree) = tree {
                    // Build the set of tools actually called from TOOL spans.
                    let called: HashSet<String> = tree.root_span
                        .spans_by_kind(SpanKind::TOOL)
                        .iter()
                        .filter_map(|s| s.attributes.get("tool.name"))
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();

                    let found: Vec<&str> = expected_tools.iter()
                        .filter(|t| called.contains(*t))
                        .map(|s| s.as_str())
                        .collect();
                    let missing: Vec<&str> = expected_tools.iter()
                        .filter(|t| !called.contains(*t))
                        .map(|s| s.as_str())
                        .collect();

                    let tm = TraceMetrics::compute_from_tree(&tree.root_span);
                    let iters = tm.avg_iterations.round() as usize;
                    let within_bounds = *max_iterations == 0 || iters <= *max_iterations;

                    let tool_frac = if expected_tools.is_empty() {
                        1.0
                    } else {
                        found.len() as f64 / expected_tools.len() as f64
                    };
                    // Going over the iteration bound halves the score even if
                    // the right tools were called.
                    let value = if within_bounds { tool_frac } else { tool_frac * 0.5 };
                    let passed = missing.is_empty() && within_bounds;

                    let reason = format!(
                        "trace: {}/{} expected tools called (of {} total calls), {} iters (max {}){}",
                        found.len(),
                        expected_tools.len(),
                        called.len(),
                        iters,
                        max_iterations,
                        if missing.is_empty() {
                            String::new()
                        } else {
                            format!(". Missing: {}", missing.join(", "))
                        }
                    );
                    EvalScore::new(value, 1.0, reason, passed)
                } else {
                    // No trace — fall back to the text heuristic.
                    self.score(input, actual, expected).await
                }
            }
            // Non-trajectory expected outputs: delegate to the text scorer.
            _ => self.score(input, actual, expected).await,
        }
    }
}

// ─── LlmJudgeMetric ──────────────────────────────────────────────────────

/// Metric that uses an LLM as a judge for subjective quality evaluation.
///
/// Applies to `ExpectedOutput::LlmJudge` cases. The judge model evaluates
/// the output on a 0-10 scale based on the rubric description.
/// Requires an LLM provider to be configured.
///
/// **Note**: This metric requires a provider at runtime. If no provider
/// is available, it returns a zero score with an error reason.
pub struct LlmJudgeMetric {
    /// The LLM provider to use for judging (optional).
    provider: Option<Arc<dyn oneai_core::traits::LlmProvider>>,
}

impl LlmJudgeMetric {
    /// Create an LlmJudgeMetric without a provider (will return error scores).
    pub fn new() -> Self {
        Self { provider: None }
    }

    /// Create an LlmJudgeMetric with a specific LLM provider.
    pub fn with_provider(provider: Arc<dyn oneai_core::traits::LlmProvider>) -> Self {
        Self { provider: Some(provider) }
    }

    /// Try to extract a numeric score from raw text.
    /// Looks for patterns like "Score: 8" or "8/10" or standalone numbers.
    fn extract_score_from_text(text: &str) -> f64 {
        // Try "Score: X" or "score: X"
        for line in text.lines() {
            let lower = line.to_lowercase();
            if lower.contains("score:") || lower.contains("rating:") {
                // Extract the number after "score:"
                let num_part = lower.split("score:").nth(1)
                    .or_else(|| lower.split("rating:").nth(1));
                if let Some(part) = num_part {
                    if let Some(num) = part.trim().split_whitespace().next() {
                        if let Ok(score) = num.parse::<f64>() {
                            return score.clamp(0.0, 10.0);
                        }
                    }
                }
            }
        }

        // Try "X/10" pattern
        if let Some(re_match) = regex::Regex::new("([\\d.]+)\\/10").unwrap().captures(text) {
            if let Some(num) = re_match.get(1) {
                if let Ok(score) = num.as_str().parse::<f64>() {
                    return score.clamp(0.0, 10.0);
                }
            }
        }

        // Default: 0.0 if no score found
        0.0
    }
}

#[async_trait]
impl EvalMetric for LlmJudgeMetric {
    fn name(&self) -> &str { "llm_judge" }

    fn description(&self) -> &str {
        "Uses an LLM to subjectively evaluate output quality against a rubric"
    }

    async fn score(&self, input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore {
        match expected {
            ExpectedOutput::LlmJudge { rubric, min_score } => {
                if let Some(provider) = &self.provider {
                    // Build the judge prompt
                    let judge_prompt = format!(
                        "Evaluate the following agent output on a 0-10 scale.\n\n\
                         Task: {}\n\
                         Agent output: {}\n\n\
                         Rubric: {}\n\n\
                         Respond with ONLY a JSON object: {{\"score\": <number>, \"reason\": <string>}}",
                        input, actual, rubric
                    );

                    let mut conversation = oneai_core::Conversation::new();
                    conversation.add_message(oneai_core::Message::user(judge_prompt));

                    let request = oneai_core::InferenceRequest {
                        conversation,
                        tools: vec![],
                        max_tokens: Some(256),
                        temperature: Some(0.0),
                        top_p: None,
                        stop_sequences: vec![],
                        constrained_output: None,
                        thinking_budget: None,
                        metadata: std::collections::HashMap::new(),
                    };

                    let response = provider.infer(request).await;
                    match response {
                        Ok(resp) => {
                            // Parse the judge's response
                            let text = resp.message.text_content();

                            // Try to extract JSON from the response
                            // The model may wrap the JSON in markdown code blocks
                            let json_text = if text.contains("```json") {
                                text.split("```json").nth(1)
                                    .and_then(|s| s.split("```").next())
                                    .unwrap_or(&text)
                                    .trim()
                            } else if text.contains("```") {
                                text.split("```").nth(1)
                                    .and_then(|s| s.split("```").next())
                                    .unwrap_or(&text)
                                    .trim()
                            } else {
                                text.trim()
                            };

                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_text) {
                                let score = parsed.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                let reason = parsed.get("reason").and_then(|v| v.as_str()).unwrap_or("No reason provided");
                                let normalized = score / 10.0;
                                let passed = score >= *min_score;

                                EvalScore::new(normalized, 1.0, reason.to_string(), passed)
                            } else {
                                // Try to extract a numeric score from raw text
                                let score = Self::extract_score_from_text(&text);
                                let passed = score >= *min_score;
                                EvalScore::new(score / 10.0, 1.0,
                                    format!("Extracted score {:.1} from judge response (not JSON)", score), passed)
                            }
                        }
                        Err(e) => {
                            EvalScore::new(0.0, 1.0, format!("Judge inference failed: {}", e), false)
                        }
                    }
                } else {
                    EvalScore::new(0.0, 1.0, "No LLM provider configured for judge", false)
                }
            }
            ExpectedOutput::Exact { .. } => {
                EvalScore::new(0.0, 1.0, "LlmJudge not applicable for Exact expected output", false)
            }
            ExpectedOutput::Contains { .. } => {
                EvalScore::new(0.0, 1.0, "LlmJudge not applicable for Contains expected output", false)
            }
            ExpectedOutput::Regex { .. } => {
                EvalScore::new(0.0, 1.0, "LlmJudge not applicable for Regex expected output", false)
            }
            ExpectedOutput::Trajectory { .. } => {
                EvalScore::new(0.0, 1.0, "LlmJudge not applicable for Trajectory expected output", false)
            }
            ExpectedOutput::Custom { .. } => {
                EvalScore::new(0.0, 1.0, "LlmJudge not applicable for Custom expected output", false)
            }
        }
    }
}

// ─── CustomJudgeMetric ────────────────────────────────────────────────────

/// Metric that delegates scoring to an `ExpectedOutput::Custom` judge.
///
/// `ExpectedOutput::Custom` carries an `EvalJudge` trait object, but without a
/// metric to dispatch to it the variant was previously dead code. This metric
/// completes that wiring: for `Custom` cases it calls `judge.judge(input,
/// actual)` and returns the judge's `EvalScore` unchanged. For all other
/// `ExpectedOutput` variants it returns a "not applicable" zero score (mirrors
/// the other built-in metrics' behavior for non-matching variants).
///
/// This is the metric SWE-bench's external judge rides on — each instance's
/// `EvalCase` carries a `SwebenchJudge` via the `Custom` variant, and this
/// metric invokes it so the `resolved` verdict becomes an `EvalScore`.
pub struct CustomJudgeMetric;

#[async_trait]
impl EvalMetric for CustomJudgeMetric {
    fn name(&self) -> &str { "custom_judge" }

    fn description(&self) -> &str {
        "Delegates scoring to a user-supplied EvalJudge via ExpectedOutput::Custom"
    }

    async fn score(&self, input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore {
        match expected {
            ExpectedOutput::Custom { judge } => judge.judge(input, actual).await,
            ExpectedOutput::Exact { .. } => {
                EvalScore::new(0.0, 1.0, "CustomJudge not applicable for Exact expected output", false)
            }
            ExpectedOutput::Contains { .. } => {
                EvalScore::new(0.0, 1.0, "CustomJudge not applicable for Contains expected output", false)
            }
            ExpectedOutput::Regex { .. } => {
                EvalScore::new(0.0, 1.0, "CustomJudge not applicable for Regex expected output", false)
            }
            ExpectedOutput::LlmJudge { .. } => {
                EvalScore::new(0.0, 1.0, "CustomJudge not applicable for LlmJudge expected output", false)
            }
            ExpectedOutput::Trajectory { .. } => {
                EvalScore::new(0.0, 1.0, "CustomJudge not applicable for Trajectory expected output", false)
            }
        }
    }
}

// ─── CompositeMetric ─────────────────────────────────────────────────────

/// A metric that combines multiple sub-metrics with weighted averaging.
///
/// Each sub-metric is applied, and the final score is the weighted
/// average of their normalized scores. This allows multi-dimensional
/// evaluation (e.g., 50% exact match + 30% trajectory + 20% LLM judge).
///
/// Weights should sum to 1.0. If they don't, the final score is
/// normalized to the total weight sum.
pub struct CompositeMetric {
    /// Sub-metrics with their weights.
    sub_metrics: Vec<(Arc<dyn EvalMetric>, f64)>,
    /// The composite metric name.
    name: String,
}

impl CompositeMetric {
    /// Create a new composite metric.
    ///
    /// Weights should ideally sum to 1.0.
    pub fn new(name: impl Into<String>, sub_metrics: Vec<(Arc<dyn EvalMetric>, f64)>) -> Self {
        Self {
            name: name.into(),
            sub_metrics,
        }
    }

    /// Create an equal-weight composite.
    pub fn equal_weight(name: impl Into<String>, metrics: Vec<Arc<dyn EvalMetric>>) -> Self {
        let weight = if metrics.is_empty() { 0.0 } else { 1.0 / metrics.len() as f64 };
        let sub_metrics = metrics.into_iter().map(|m| (m, weight)).collect();
        Self {
            name: name.into(),
            sub_metrics,
        }
    }
}

#[async_trait]
impl EvalMetric for CompositeMetric {
    fn name(&self) -> &str { &self.name }

    fn description(&self) -> &str {
        "Weighted combination of multiple sub-metrics"
    }

    async fn score(&self, input: &str, actual: &str, expected: &ExpectedOutput) -> EvalScore {
        if self.sub_metrics.is_empty() {
            return EvalScore::new(0.0, 1.0, "No sub-metrics configured", false);
        }

        let mut weighted_sum = 0.0;
        let mut total_weight = 0.0;
        let mut all_passed = true;
        let mut reasons = Vec::new();

        for (metric, weight) in &self.sub_metrics {
            let score = metric.score(input, actual, expected).await;
            weighted_sum += score.normalized() * *weight;
            total_weight += *weight;
            if !score.passed {
                all_passed = false;
            }
            reasons.push(format!("{}: {}", metric.name(), score.reason));
        }

        let final_score = if total_weight == 0.0 { 0.0 } else { weighted_sum / total_weight };

        EvalScore::new(
            final_score,
            1.0,
            reasons.join("; "),
            all_passed,
        )
    }
}

// ─── EfficiencyMetric ──────────────────────────────────────────────────────

/// Efficiency-axis metric — scores token+latency cost (quality-agnostic).
///
/// Computes `1 / (1 + 0.1·log10(1+tokens) + 0.1·log10(1+latency_ms))` from
/// the trace span tree, so a cheap, fast run scores near 1.0 and an
/// expensive, slow run scores lower. This is the "efficiency" component of
/// the three-axis evaluation (能力×成本×效率); the quality axis comes from a
/// correctness metric and is folded in by the CLI `--profile` report via
/// [`crate::EfficiencyProfile::three_axis_score`].
///
/// Pass/fail thresholds are configurable (`max_tokens`, `max_latency_ms`);
/// `0` means "no limit on this dimension".
pub struct EfficiencyMetric {
    /// Max tokens for a pass (0 = no limit).
    pub max_tokens: u64,
    /// Max agent latency (ms) for a pass (0 = no limit).
    pub max_latency_ms: u64,
}

impl Default for EfficiencyMetric {
    fn default() -> Self {
        Self { max_tokens: 0, max_latency_ms: 0 }
    }
}

impl EfficiencyMetric {
    /// Create with no pass/fail thresholds (score only).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with explicit pass thresholds.
    pub fn with_limits(max_tokens: u64, max_latency_ms: u64) -> Self {
        Self { max_tokens, max_latency_ms }
    }
}

#[async_trait]
impl EvalMetric for EfficiencyMetric {
    fn name(&self) -> &str { "efficiency" }

    fn description(&self) -> &str {
        "Efficiency axis: token+latency cost from the trace (quality-agnostic); \
         1/(1+0.1·log(1+tokens)+0.1·log(1+latency))"
    }

    async fn score(&self, _input: &str, _actual: &str, _expected: &ExpectedOutput) -> EvalScore {
        // No trace available in metric-only mode — efficiency is unmeasurable.
        EvalScore::new(
            0.0,
            1.0,
            "efficiency requires trace data — run with a provider + tracing".to_string(),
            false,
        )
    }

    async fn score_with_trace(
        &self,
        _input: &str,
        _actual: &str,
        _expected: &ExpectedOutput,
        tree: Option<&TraceTree>,
    ) -> EvalScore {
        let tree = match tree {
            Some(t) => t,
            None => {
                return EvalScore::new(
                    0.0, 1.0,
                    "no trace — efficiency not measurable".to_string(),
                    false,
                )
            }
        };

        let tm = TraceMetrics::compute_from_tree(&tree.root_span);
        let tokens = tm.total_tokens;
        let latency_ms = tm.total_session_duration_ms;
        let iters = tm.avg_iterations.round() as usize;
        let tool_calls = tm.tool_call_count;

        let value = 1.0
            / (1.0
                + 0.1 * log10_1p(tokens as f64)
                + 0.1 * log10_1p(latency_ms as f64));

        let within_tokens = self.max_tokens == 0 || tokens <= self.max_tokens;
        let within_latency = self.max_latency_ms == 0 || latency_ms <= self.max_latency_ms;
        let passed = within_tokens && within_latency;

        let reason = format!(
            "{} tokens, {}ms latency, {} iters, {} tool calls (efficiency={:.3})",
            tokens, latency_ms, iters, tool_calls, value
        );
        EvalScore::new(value, 1.0, reason, passed)
    }
}

/// log10(1+x), floored at 0 so non-positive inputs don't NaN.
fn log10_1p(x: f64) -> f64 {
    if x <= 0.0 { 0.0 } else { (1.0 + x).log10() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval_metric::EvalJudge;

    #[tokio::test]
    async fn test_exact_match_pass() {
        let metric = ExactMatchMetric;
        let score = metric.score("2+2?", "4", &ExpectedOutput::exact("4")).await;
        assert!(score.passed);
        assert_eq!(score.value, 1.0);
    }

    #[tokio::test]
    async fn test_exact_match_fail() {
        let metric = ExactMatchMetric;
        let score = metric.score("2+2?", "5", &ExpectedOutput::exact("4")).await;
        assert!(!score.passed);
        assert_eq!(score.value, 0.0);
    }

    #[tokio::test]
    async fn test_exact_match_whitespace() {
        let metric = ExactMatchMetric;
        let score = metric.score("2+2?", "  4  ", &ExpectedOutput::exact("4")).await;
        assert!(score.passed); // trim comparison
    }

    #[tokio::test]
    async fn test_exact_match_not_applicable() {
        let metric = ExactMatchMetric;
        let score = metric.score("test", "output", &ExpectedOutput::contains(["x"])).await;
        assert!(!score.passed);
        assert!(score.reason.contains("not applicable"));
    }

    #[tokio::test]
    async fn test_contains_match_all_found() {
        let metric = ContainsMatchMetric;
        let score = metric.score(
            "Explain Rust",
            "Rust is a memory-safe language",
            &ExpectedOutput::contains(["memory", "safe"]),
        ).await;
        assert!(score.passed);
        assert_eq!(score.value, 1.0);
    }

    #[tokio::test]
    async fn test_contains_match_partial() {
        let metric = ContainsMatchMetric;
        let score = metric.score(
            "Explain Rust",
            "Rust is a programming language",
            &ExpectedOutput::contains(["memory", "safe", "programming"]),
        ).await;
        assert!(!score.passed);
        assert_eq!(score.value, 1.0 / 3.0); // 1 of 3 found
        assert!(score.reason.contains("memory") || score.reason.contains("safe"));
    }

    #[tokio::test]
    async fn test_contains_match_case_insensitive() {
        let metric = ContainsMatchMetric;
        let score = metric.score(
            "test",
            "RUST IS MEMORY SAFE",
            &ExpectedOutput::contains(["rust", "memory"]),
        ).await;
        assert!(score.passed);
    }

    #[tokio::test]
    async fn test_contains_match_case_sensitive() {
        let metric = ContainsMatchMetric;
        let score = metric.score(
            "test",
            "RUST IS MEMORY SAFE",
            &ExpectedOutput::contains_case_sensitive(["rust", "memory"]),
        ).await;
        assert!(!score.passed); // "rust" != "RUST"
    }

    #[tokio::test]
    async fn test_regex_match_pass() {
        let metric = RegexMatchMetric;
        let score = metric.score(
            "What date?",
            "Today is 2026-06-18",
            &ExpectedOutput::regex("\\d{4}-\\d{2}-\\d{2}"),
        ).await;
        assert!(score.passed);
    }

    #[tokio::test]
    async fn test_regex_match_fail() {
        let metric = RegexMatchMetric;
        let score = metric.score(
            "What date?",
            "Today is June 18th",
            &ExpectedOutput::regex("\\d{4}-\\d{2}-\\d{2}"),
        ).await;
        assert!(!score.passed);
    }

    #[tokio::test]
    async fn test_regex_match_invalid_pattern() {
        let metric = RegexMatchMetric;
        let score = metric.score(
            "test",
            "output",
            &ExpectedOutput::regex("[invalid"),
        ).await;
        assert!(!score.passed);
        assert!(score.reason.contains("Invalid regex"));
    }

    #[tokio::test]
    async fn test_trajectory_metric_all_found() {
        let metric = TrajectoryMetric;
        let score = metric.score(
            "Calculate 2+2",
            "Used calculator tool: result is 4",
            &ExpectedOutput::trajectory(["calculator"], 5),
        ).await;
        assert!(score.passed);
    }

    #[tokio::test]
    async fn test_trajectory_metric_partial() {
        let metric = TrajectoryMetric;
        let score = metric.score(
            "Research and analyze",
            "Used search tool for research",
            &ExpectedOutput::trajectory(["search", "calculator", "read_file"], 10),
        ).await;
        assert!(!score.passed);
        assert_eq!(score.value, 1.0 / 3.0); // 1 of 3 found
    }

    #[tokio::test]
    async fn test_trajectory_metric_uses_trace_tool_names() {
        // score_with_trace walks TOOL spans for `tool.name` instead of
        // substring-matching the output text. Build a synthetic tree with
        // two tool calls (calculator + grep), neither mentioned in `actual`.
        use std::collections::HashMap;
        use oneai_trace::{Span, SpanKind, SpanStatus, TraceTree};

        let mut root = Span::new(SpanKind::SESSION, "session", None);
        root.end(SpanStatus::Ok);
        let mut calc = Span::new(SpanKind::TOOL, "tool.calc", Some(&root.span_id));
        calc.set_attribute("tool.name", serde_json::json!("calculator"));
        calc.end(SpanStatus::Ok);
        let mut grep = Span::new(SpanKind::TOOL, "tool.grep", Some(&root.span_id));
        grep.set_attribute("tool.name", serde_json::json!("grep"));
        grep.end(SpanStatus::Ok);

        let mut spans = HashMap::new();
        spans.insert(root.span_id.clone(), root.clone());
        spans.insert(calc.span_id.clone(), calc);
        spans.insert(grep.span_id.clone(), grep);
        let tree = TraceTree::from_spans(spans, None);

        let metric = TrajectoryMetric;
        // expected {calculator, shell}: only calculator was called → 1/2, fail.
        let score = metric.score_with_trace(
            "in", "no tool names mentioned here",
            &ExpectedOutput::trajectory(["calculator", "shell"], 10),
            Some(&tree),
        ).await;
        assert!(!score.passed);
        assert!(score.reason.contains("1/2"), "reason: {}", score.reason);

        // expected {calculator, grep}: both called → pass.
        let score2 = metric.score_with_trace(
            "in", "no tool names mentioned here",
            &ExpectedOutput::trajectory(["calculator", "grep"], 0),
            Some(&tree),
        ).await;
        assert!(score2.passed, "reason: {}", score2.reason);
    }

    #[tokio::test]
    async fn test_efficiency_metric_requires_trace() {
        let metric = EfficiencyMetric::new();
        // No trace → unmeasurable.
        let s = metric.score_with_trace("in", "out", &ExpectedOutput::exact("x"), None).await;
        assert!(!s.passed);
        assert_eq!(s.value, 0.0);

        // score() (text path) is also unmeasurable.
        let s2 = metric.score("in", "out", &ExpectedOutput::exact("x")).await;
        assert!(!s2.passed);
        assert_eq!(s2.value, 0.0);
    }

    #[tokio::test]
    async fn test_efficiency_metric_with_trace_scores() {
        use std::collections::HashMap;
        use oneai_trace::{Span, SpanKind, SpanStatus, TraceTree};

        let mut root = Span::new(SpanKind::SESSION, "session", None);
        // Give the session a measurable duration so latency > 0.
        root.end(SpanStatus::Ok);
        let mut llm = Span::new(SpanKind::LLM, "llm.call", Some(&root.span_id));
        llm.end(SpanStatus::Ok);
        let mut spans = HashMap::new();
        spans.insert(root.span_id.clone(), root.clone());
        spans.insert(llm.span_id.clone(), llm);
        let tree = TraceTree::from_spans(spans, None);

        let metric = EfficiencyMetric::new();
        let s = metric.score_with_trace("in", "out", &ExpectedOutput::exact("x"), Some(&tree)).await;
        // With (near-)zero tokens/latency the efficiency score is near 1.0.
        assert!(s.value > 0.0, "value should be positive, got {}", s.value);
        assert!(s.reason.contains("tokens"));
    }

    #[tokio::test]
    async fn test_composite_equal_weight() {
        let composite = CompositeMetric::equal_weight("combined", vec![
            Arc::new(ExactMatchMetric),
            Arc::new(ContainsMatchMetric),
        ]);

        let score = composite.score(
            "What is Rust?",
            "Rust is a memory-safe programming language",
            &ExpectedOutput::contains(["memory", "safe"]),
        ).await;

        // ContainsMatch passes (1.0), ExactMatch is not applicable (0.0)
        // Equal weight: (1.0 * 0.5 + 0.0 * 0.5) = 0.5
        assert_eq!(score.value, 0.5);
    }

    #[tokio::test]
    async fn test_llm_judge_no_provider() {
        let metric = LlmJudgeMetric::new();
        let score = metric.score(
            "test",
            "output",
            &ExpectedOutput::llm_judge("Test rubric", 7.0),
        ).await;
        assert!(!score.passed);
        assert!(score.reason.contains("No LLM provider"));
    }

    #[tokio::test]
    async fn test_custom_judge_dispatches() {
        // A tiny judge: passes if the actual output contains "ok".
        struct OkJudge;
        #[async_trait]
        impl EvalJudge for OkJudge {
            async fn judge(&self, _input: &str, actual: &str) -> EvalScore {
                EvalScore::from_bool(actual.contains("ok"), "checked 'ok'")
            }
        }

        let metric = CustomJudgeMetric;
        let expected = ExpectedOutput::Custom { judge: Arc::new(OkJudge) };

        let pass = metric.score("in", "looks ok to me", &expected).await;
        assert!(pass.passed);
        assert_eq!(pass.value, 1.0);

        let fail = metric.score("in", "nope", &expected).await;
        assert!(!fail.passed);
    }

    #[tokio::test]
    async fn test_custom_judge_not_applicable() {
        let metric = CustomJudgeMetric;
        let score = metric.score("in", "out", &ExpectedOutput::exact("x")).await;
        assert!(!score.passed);
        assert!(score.reason.contains("not applicable"));
    }
}
