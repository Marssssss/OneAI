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

/// One generation's compact summary — E4's multi-generation loop records one
/// `GenerationSummary` per generation in [`EvolutionReport::generations`].
/// The full per-case detail lives only for the **final** generation
/// (`case_records`); earlier generations keep just the axes + the frontier
/// outcome so the report stays readable + cheap to serialize.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationSummary {
    /// 0-based generation index.
    pub generation: usize,
    /// The base config's full-suite pass rate this generation.
    pub base_pass_rate: f64,
    /// The frontier-best **subset** pass rate this generation.
    pub frontier_pass_rate: f64,
    /// Frontier-best total tokens (subset).
    pub frontier_total_tokens: u64,
    /// Frontier-best total latency ms (subset).
    pub frontier_total_latency_ms: u64,
    /// True iff the frontier-best is the base (no candidate improved on it).
    pub frontier_is_seed: bool,
    /// Per-candidate subset scores this generation (E3 shape; empty for
    /// `no_optimize` generations).
    #[serde(default)]
    pub candidate_scores: Vec<CandidateScoreRecord>,
    /// The frontier config file persisted this generation
    /// (`frontier-gen<n>.json`), relative to `run_dir`. `None` when the
    /// frontier is the base (nothing new persisted).
    #[serde(default)]
    pub frontier_config_file: Option<PathBuf>,
    /// Natural-language lessons note (the merger's output).
    #[serde(default)]
    pub lessons_text: String,
}

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

/// One variation candidate's Pareto axes + outcome — the report-facing
/// summary of an E3 [`ScoredCandidate`](crate::gepa::ScoredCandidate). Only
/// surviving (validated) candidates appear; dropped ones are logged via
/// `tracing::warn` and counted implicitly as `population - len(candidate_scores)`.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateScoreRecord {
    /// 1-based index among the K variation candidates this generation.
    pub index: usize,
    /// Fraction of subset cases that passed.
    pub pass_rate: f64,
    /// Prompt + completion tokens summed across subset cases.
    pub total_tokens: u64,
    /// Wall-clock latency summed across subset cases (ms).
    pub total_latency_ms: u64,
    /// Cases passed on the subset.
    pub passed: usize,
    /// Total subset cases.
    pub total_cases: usize,
}

/// The generation's Pareto-frontier best — folded into the report so a reader
/// of `report.json` sees the recommended next-gen config + its axes without
/// opening a separate file.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontierRecord {
    /// Frontier-best pass rate (on the subset).
    pub pass_rate: f64,
    /// Frontier-best total tokens.
    pub total_tokens: u64,
    /// Frontier-best total latency (ms).
    pub total_latency_ms: u64,
    /// Path to the persisted frontier config (`frontier-gen<n>.json`),
    /// relative to `EvolutionReport.run_dir`. `None` if the frontier best
    /// is the seed (no improvement → nothing new to persist).
    pub config_file: Option<PathBuf>,
    /// True iff the frontier best is the seed (no candidate improved on it).
    pub is_seed: bool,
}

/// Aggregate report for one evolution run. With `max_generations == 1`
/// (E1/E2/E3) `generations` has one entry and `case_records` /
/// `candidate_scores` / `frontier` are that single generation's — the shape
/// E3 asserts on. With `max_generations > 1` (E4) `generations` carries the
/// per-generation summaries and the top-level `case_records` /
/// `candidate_scores` / `frontier` mirror the **final** generation (so a
/// reader that doesn't iterate `generations` still sees the run's outcome).
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionReport {
    /// Suite name.
    pub suite_name: String,
    /// Final generation index reached (0 for E1's single-gen run).
    pub generation: usize,
    /// Whether optimization was skipped (`--no-optimize`, the E1 default).
    pub no_optimize: bool,
    /// Per-case records for the **final** generation.
    pub case_records: Vec<CaseRecord>,
    /// Per-failed-case diagnoses for the final generation (E2). Empty for E1
    /// runs with no failures.
    #[serde(default)]
    pub diagnoses: Vec<DiagnosisRecord>,
    /// E3: per-candidate scores on the subset, for the final generation.
    /// Empty for `no_optimize` runs.
    #[serde(default)]
    pub candidate_scores: Vec<CandidateScoreRecord>,
    /// E3: the Pareto-frontier best, for the final generation, if
    /// optimization ran.
    #[serde(default)]
    pub frontier: Option<FrontierRecord>,
    /// E4: per-generation summaries (one per generation that ran). Empty for
    /// reports serialized by older versions (`#[serde(default)]`).
    #[serde(default)]
    pub generations: Vec<GenerationSummary>,
    /// E4: path to the cross-generation `lessons.jsonl`, relative to
    /// `run_dir`. `None` only when no generations recorded.
    #[serde(default)]
    pub lessons_file: Option<PathBuf>,
    /// E4: why the loop stopped (converged / max-generations / budget /
    /// stagnation). `None` for E1/E2/E3 single-gen runs (no stop decision).
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Fraction of cases that passed (final generation, full suite).
    pub pass_rate: f64,
    /// Total prompt + completion tokens across all cases (final generation).
    pub total_tokens: u64,
    /// Run directory (absolute) where per-case trajectories + report live.
    pub run_dir: PathBuf,
}

impl EvolutionReport {
    /// Compute the report from per-case records (called after persistence).
    /// E3's `candidate_scores` + `frontier` are empty/None here; the optimized
    /// path mutates them in before `persist_report`. E4's multi-gen fields
    /// (`generations` / `lessons_file` / `stop_reason`) are also empty here —
    /// the multi-gen loop builds via [`Self::for_run`].
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
            candidate_scores: Vec::new(),
            frontier: None,
            generations: Vec::new(),
            lessons_file: None,
            stop_reason: None,
            pass_rate,
            total_tokens,
            run_dir,
        }
    }

    /// Build the report for a multi-generation run (E4). The final
    /// generation's `case_records` / `diagnoses` / `candidate_scores` /
    /// `frontier` are lifted to the top level (so non-iterating readers see
    /// the outcome); `generations` carries the per-gen summaries.
    #[allow(clippy::too_many_arguments)]
    pub fn for_run(
        suite_name: &str,
        no_optimize: bool,
        run_dir: PathBuf,
        generations: Vec<GenerationSummary>,
        final_generation: usize,
        final_case_records: Vec<CaseRecord>,
        final_diagnoses: Vec<DiagnosisRecord>,
        final_candidate_scores: Vec<CandidateScoreRecord>,
        final_frontier: Option<FrontierRecord>,
        lessons_file: Option<PathBuf>,
        stop_reason: Option<String>,
    ) -> Self {
        let total_cases = final_case_records.len();
        let passed = final_case_records.iter().filter(|c| c.passed).count();
        let pass_rate = if total_cases == 0 {
            0.0
        } else {
            passed as f64 / total_cases as f64
        };
        let total_tokens = final_case_records
            .iter()
            .map(|c| c.prompt_tokens + c.completion_tokens)
            .sum();
        Self {
            suite_name: suite_name.to_string(),
            generation: final_generation,
            no_optimize,
            case_records: final_case_records,
            diagnoses: final_diagnoses,
            candidate_scores: final_candidate_scores,
            frontier: final_frontier,
            generations,
            lessons_file,
            stop_reason,
            pass_rate,
            total_tokens,
            run_dir,
        }
    }

    /// Attach E3 optimization results (candidate scores + frontier) in place.
    /// Called by the optimized `run()` path before persisting.
    pub fn with_optimization(
        &mut self,
        candidate_scores: Vec<CandidateScoreRecord>,
        frontier: Option<FrontierRecord>,
    ) {
        self.candidate_scores = candidate_scores;
        self.frontier = frontier;
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
        if let Some(reason) = &self.stop_reason {
            s.push_str(&format!("  stop: {reason}\n"));
        }
        if let Some(lf) = &self.lessons_file {
            s.push_str(&format!("  lessons: {}\n", lf.display()));
        }
        // Multi-generation table (E4). Omitted when only one generation ran
        // (avoids duplicating the per-case + frontier blocks below).
        if self.generations.len() > 1 {
            s.push_str(&format!("  generations ({}):\n", self.generations.len()));
            for g in &self.generations {
                s.push_str(&format!(
                    "    gen {} | base {:.0}% | frontier {:.0}% (tok {} lat {}ms) | {}\n",
                    g.generation,
                    g.base_pass_rate * 100.0,
                    g.frontier_pass_rate * 100.0,
                    g.frontier_total_tokens,
                    g.frontier_total_latency_ms,
                    if g.frontier_is_seed {
                        "seed".to_string()
                    } else if let Some(p) = &g.frontier_config_file {
                        p.display().to_string()
                    } else {
                        "frontier".to_string()
                    },
                ));
            }
        }
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
        if !self.candidate_scores.is_empty() {
            s.push_str(&format!(
                "  candidates ({} scored):\n",
                self.candidate_scores.len()
            ));
            for c in &self.candidate_scores {
                s.push_str(&format!(
                    "    #{} | pass {}/{} ({:.0}%) | tokens {} | latency {}ms\n",
                    c.index,
                    c.passed,
                    c.total_cases,
                    c.pass_rate * 100.0,
                    c.total_tokens,
                    c.total_latency_ms
                ));
            }
        }
        if let Some(f) = &self.frontier {
            s.push_str(&format!(
                "  frontier: pass {:.0}% | tokens {} | latency {}ms | {}\n",
                f.pass_rate * 100.0,
                f.total_tokens,
                f.total_latency_ms,
                if f.is_seed {
                    "seed (no improvement)".to_string()
                } else if let Some(p) = &f.config_file {
                    p.display().to_string()
                } else {
                    "(no config)".to_string()
                }
            ));
        }
        s
    }
}
