//! `EvolutionReport` + `CaseRecord` — the persisted output of an evolution run.
//!
//! The report is a serializable summary of one generation: per-case
//! `CaseRecord`s (the `EvalResult` axis — scores, tokens, latency, iterations,
//! tool calls — plus a pointer to the per-case trajectory file), the aggregate
//! pass rate, and the run directory. Per-case trajectories are written as
//! separate `case-<id>.jsonl` files (one JSON object per line) under the run
//! dir so E2's diagnostician can stream them without loading the whole
//! report, and so a replay regression gate can re-emit them.
//!
//! Only the fields the self-evolution loop consumes are mirrored here — the
//! full `EvalResult` / `Trajectory` stay in their owning crates. This keeps
//! the report stable across `oneai-eval` schema evolution.

use std::path::PathBuf;

use oneai_eval::MetricScore;
use serde::{Deserialize, Serialize};

use crate::subgraph::ParamRef;

/// One case's record in an [`EvolutionReport`].
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseRecord {
    /// The case ID (matches `EvalCase.id`).
    pub case_id: String,
    /// Original user input.
    pub input: String,
    /// Agent's actual output.
    pub actual_output: String,
    /// Whether all metrics passed (no error + all `EvalScore.passed`).
    pub passed: bool,
    /// Per-metric scores (cloned from `EvalResult.scores`).
    pub scores: Vec<MetricScore>,
    /// Prompt tokens consumed by this case.
    pub prompt_tokens: u64,
    /// Completion tokens consumed by this case.
    pub completion_tokens: u64,
    /// Wall-clock duration in milliseconds.
    pub latency_ms: u64,
    /// ReAct iterations taken.
    pub iterations: usize,
    /// Number of tool calls made.
    pub tool_calls: usize,
    /// Tool names called, in order (the trajectory's `recorded_tool_calls`).
    pub tool_call_names: Vec<String>,
    /// Path to the per-case trajectory file (`case-<id>.jsonl`), relative to
    /// `EvolutionReport.run_dir`. `None` if trajectory capture was skipped.
    pub trajectory_file: Option<PathBuf>,
    /// Execution error, if any.
    pub error: Option<String>,
}

/// A diagnosis record for one failed case — the report-facing summary of an
/// E2 [`Diagnosis`](crate::subgraph::Diagnosis). The full diagnosis (with
/// `subtrace`) is persisted to `diagnosis_file`; this record keeps the
/// `suspect_params` + `critique` inline so a reader of `report.json` sees the
/// attribution without opening the per-case file.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosisRecord {
    /// The case ID (matches `EvalCase.id`).
    pub case_id: String,
    /// Suspect variation params (the full candidate universe if the
    /// diagnostician fell back to tail-N-rounds — see `Diagnosis.subtrace`).
    pub suspect_params: Vec<ParamRef>,
    /// Natural-language attribution (heuristic-templated, or judge-rewritten
    /// if an `LlmDiagnostician` was wired).
    pub critique: String,
    /// Path to the per-case diagnosis file (`diagnosis-<id>.json`), relative
    /// to `EvolutionReport.run_dir`. `None` if diagnosis was skipped (E1
    /// no-failure runs, or `--no-optimize` paths that opt out).
    pub diagnosis_file: Option<PathBuf>,
}

impl CaseRecord {
    /// Build a `CaseRecord` from a `CaseRun`'s scored result + trajectory, with
    /// the trajectory file path (relative to the run dir) filled in by the
    /// caller after persistence.
    pub fn from_run(
        result: &oneai_eval::EvalResult,
        trajectory: &oneai_eval::Trajectory,
        trajectory_file: Option<PathBuf>,
    ) -> Self {
        let iterations = trajectory.recorded_iterations;
        let tool_call_names = trajectory.recorded_tool_calls.clone();
        let tool_calls = tool_call_names.len();
        Self {
            case_id: result.case_id.clone(),
            input: result.input.clone(),
            actual_output: result.actual_output.clone(),
            passed: result.passed(),
            scores: result.scores.clone(),
            prompt_tokens: result.prompt_tokens,
            completion_tokens: result.completion_tokens,
            latency_ms: result.duration_ms,
            iterations,
            tool_calls,
            tool_call_names,
            trajectory_file,
            error: result.error.clone(),
        }
    }
}

/// Aggregate report for one evolution generation.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionReport {
    /// Suite name.
    pub suite_name: String,
    /// Generation index (0 for E1's degenerate single-gen run).
    pub generation: usize,
    /// Whether optimization was skipped (`--no-optimize`, the E1 default).
    pub no_optimize: bool,
    /// Per-case records.
    pub case_records: Vec<CaseRecord>,
    /// Per-failed-case diagnoses (E2). Empty for E1 runs with no failures.
    #[serde(default)]
    pub diagnoses: Vec<DiagnosisRecord>,
    /// Fraction of cases that passed.
    pub pass_rate: f64,
    /// Total prompt + completion tokens across all cases.
    pub total_tokens: u64,
    /// Run directory (absolute) where per-case trajectories + report live.
    pub run_dir: PathBuf,
}

impl EvolutionReport {
    /// Compute the report from per-case records (called after persistence).
    pub fn from_records(
        suite_name: &str,
        generation: usize,
        no_optimize: bool,
        case_records: Vec<CaseRecord>,
        diagnoses: Vec<DiagnosisRecord>,
        run_dir: PathBuf,
    ) -> Self {
        let total_cases = case_records.len();
        let passed = case_records.iter().filter(|c| c.passed).count();
        let pass_rate = if total_cases == 0 {
            0.0
        } else {
            passed as f64 / total_cases as f64
        };
        let total_tokens = case_records
            .iter()
            .map(|c| c.prompt_tokens + c.completion_tokens)
            .sum();
        Self {
            suite_name: suite_name.to_string(),
            generation,
            no_optimize,
            case_records,
            diagnoses,
            pass_rate,
            total_tokens,
            run_dir,
        }
    }

    /// Render a compact human-readable summary (for CLI output).
    pub fn to_summary(&self) -> String {
        let passed = self.case_records.iter().filter(|c| c.passed).count();
        let mut s = format!(
            "Evolution gen {} ({}): suite={} | pass {}/{} ({:.0}%) | tokens {} | run_dir={}\n",
            self.generation,
            if self.no_optimize {
                "no-optimize"
            } else {
                "optimized"
            },
            self.suite_name,
            passed,
            self.case_records.len(),
            self.pass_rate * 100.0,
            self.total_tokens,
            self.run_dir.display()
        );
        for c in &self.case_records {
            let mark = if c.passed { "✓" } else { "✗" };
            s.push_str(&format!(
                "  {} {} | iter {} | tools {} | tokens {} | latency {}ms\n",
                mark,
                c.case_id,
                c.iterations,
                c.tool_call_names.join(","),
                c.prompt_tokens + c.completion_tokens,
                c.latency_ms
            ));
        }
        if !self.diagnoses.is_empty() {
            s.push_str(&format!(
                "  diagnoses ({} failed case{}):\n",
                self.diagnoses.len(),
                if self.diagnoses.len() == 1 { "" } else { "s" }
            ));
            for d in &self.diagnoses {
                let params: Vec<String> = d.suspect_params.iter().map(|p| p.path()).collect();
                s.push_str(&format!(
                    "    ✗ {} | suspect: {}\n",
                    d.case_id,
                    if params.is_empty() {
                        "(none)".to_string()
                    } else {
                        params.join(", ")
                    }
                ));
                // Indent the critique under the case.
                for line in d.critique.lines().take(1) {
                    s.push_str(&format!("      {}\n", line));
                }
            }
        }
        s
    }
}
