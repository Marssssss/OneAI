//! `EvolutionLoop` — the outer driver. E1's degenerate form runs generation 0
//! only (no diagnosis / variation / Pareto) and persists a report + per-case
//! trajectories. E2 adds diagnosis between the collect and report steps; E3
//! wraps variation around a single generation; **E4 loops generations to
//! convergence**, carrying the frontier forward as the next-gen base via a
//! `LessonMerger` and persisting a cross-generation `lessons.jsonl`.
//!
//! With `max_generations == 1` (the E1/E2/E3 default) the loop runs once and
//! the report is identical to E3's — backward compatible. `max_generations >
//! 1` activates the multi-gen path: per generation it collects the base's
//! full-suite runs → diagnoses failures → varies K candidates on a subset →
//! Pareto-selects the frontier → merges the frontier into the next-gen base
//! → records a lesson. It stops on convergence (`frontier_pass ≥ target`),
//! the generation cap, the cumulative-token cap, or stagnation
//! (`early_stop_patience` consecutive generations with no improvement).
//!
//! `EvolutionConfig` fields (`max_generations`, `no_optimize`) are stable
//! across phases; E4 finally consumes `max_generations`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::LlmProvider;
use oneai_eval::{EvalSuite, RecordingProvider};

use crate::candidate::{AppHandle, CandidateConfig};
use crate::failure_extractor::{extract_failures, FailedCase};
use crate::gepa::{select_case_subset, GepaOptimizer, ScoredCandidate};
use crate::lessons::{LessonEntry, LessonsLog};
use crate::report::{
    CandidateScoreRecord, CaseRecord, DiagnosisRecord, EvolutionReport, FrontierRecord,
    GenerationSummary,
};
use crate::subgraph::{Diagnosis, HeuristicDiagnostician, SubgraphDiagnostician};
use crate::trajectory_collector::{CaseRun, TrajectoryCollector};

/// The baseline an evolution run is rooted in: the provider to wrap in a
/// recorder + the project dir the seed pack resolves against.
#[non_exhaustive]
#[derive(Clone)]
pub struct AppBaseline {
    /// Provider injected by the caller (the loop wraps it in a
    /// [`RecordingProvider`]). Tests pass a `MockProvider`; the CLI passes a
    /// real provider from `ProviderFactory`.
    pub provider: Arc<dyn LlmProvider>,
    /// Project directory for `DomainPackSpecFile::validate_and_build` (where
    /// context sources like `.oneai/instructions` resolve).
    pub project_dir: String,
}

impl AppBaseline {
    /// Construct a baseline from the injected provider + project dir.
    pub fn new(provider: Arc<dyn LlmProvider>, project_dir: impl Into<String>) -> Self {
        Self {
            provider,
            project_dir: project_dir.into(),
        }
    }
}

/// Evolution-loop configuration.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct EvolutionConfig {
    /// Root directory for evolution output (`<root>/evolve/run-<ts>/...`).
    pub root: PathBuf,
    /// Generation cap (E1 stops at gen 0 regardless; E4 loops to this).
    pub max_generations: usize,
    /// Skip variation (E1 default — always true until E3 lands).
    pub no_optimize: bool,
}

impl EvolutionConfig {
    /// Construct with a custom output root + E1 defaults (max_generations=1,
    /// no_optimize=true).
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            max_generations: 1,
            no_optimize: true,
        }
    }

    /// Set the generation cap (forward for E4).
    #[must_use]
    pub fn with_max_generations(mut self, n: usize) -> Self {
        self.max_generations = n;
        self
    }

    /// Toggle variation (E3 sets this false to run the optimizer).
    #[must_use]
    pub fn with_no_optimize(mut self, no_optimize: bool) -> Self {
        self.no_optimize = no_optimize;
        self
    }
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            root: default_root(),
            max_generations: 1,
            no_optimize: true,
        }
    }
}

pub(crate) fn default_root() -> PathBuf {
    // Mirror the working-state / supervisor convention: ~/.oneai/<subsystem>.
    dirs_or_cwd().join(".oneai").join("evolve")
}

fn dirs_or_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The outer driver. Construct per run (the loop is stateless across runs;
/// cross-generation state lands in `LessonsLog` in E4).
pub struct EvolutionLoop {
    /// Baseline provider + project dir.
    pub baseline: AppBaseline,
    /// Run configuration.
    pub config: EvolutionConfig,
    /// Diagnoses failed cases into suspect `ParamRef`s (E2). Defaults to the
    /// deterministic [`HeuristicDiagnostician`]; E5 wires an
    /// [`LlmDiagnostician`](crate::subgraph::LlmDiagnostician) with a
    /// stronger/different-family judge.
    pub diagnostician: Arc<dyn SubgraphDiagnostician>,
    /// E3 GEPA optimizer (variation + Pareto). `None` (or `no_optimize=true`)
    /// → the E1/E2 degenerate path. Set via [`with_optimizer`](Self::with_optimizer).
    pub optimizer: Option<Arc<GepaOptimizer>>,
}

impl EvolutionLoop {
    /// Construct with a baseline + default config + the heuristic
    /// diagnostician (no LLM judge — deterministic, safe for tests).
    pub fn new(baseline: AppBaseline) -> Self {
        Self {
            baseline,
            config: EvolutionConfig::default(),
            diagnostician: Arc::new(HeuristicDiagnostician),
            optimizer: None,
        }
    }

    /// Set the config (builder-style).
    #[must_use]
    pub fn with_config(mut self, config: EvolutionConfig) -> Self {
        self.config = config;
        self
    }

    /// Override the diagnostician (E5 injects an `LlmDiagnostician`).
    #[must_use]
    pub fn with_diagnostician(mut self, diagnostician: Arc<dyn SubgraphDiagnostician>) -> Self {
        self.diagnostician = diagnostician;
        self
    }

    /// Wire the E3 GEPA optimizer (variation operator + Pareto selector +
    /// config). Only consulted when `config.no_optimize == false`; otherwise
    /// the loop runs the E1/E2 degenerate path regardless.
    #[must_use]
    pub fn with_optimizer(mut self, optimizer: Arc<GepaOptimizer>) -> Self {
        self.optimizer = Some(optimizer);
        self
    }

    /// Run the evolution loop against `suite`, persisting a report + the
    /// final generation's per-case trajectories + diagnoses + a
    /// cross-generation `lessons.jsonl`. Returns the report.
    ///
    /// With `max_generations == 1` (E1/E2/E3 default) this is a single
    /// generation — identical to E3's behavior. With `max_generations > 1`
    /// (E4) the loop carries the frontier forward as the next-gen base via
    /// the optimizer's `LessonMerger` and stops on convergence / cap /
    /// budget / stagnation. The `no_optimize` path skips variation + Pareto
    /// (E1/E2); a non-`no_optimize` run without a wired optimizer also
    /// degrades to the no-optimize path.
    ///
    /// Each generation's base is run live on the FULL suite (semantic-
    /// variation candidates can't be replayed; design §3.3 难点 C). Only the
    /// **final** generation's trajectories + diagnoses are persisted to disk
    /// (intermediate gens keep their lessons row only — saves disk + the
    /// in-memory diagnoses still feed the next gen's variation).
    pub async fn run(&self, seed: &CandidateConfig, suite: &EvalSuite) -> Result<EvolutionReport> {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
        let run_dir = self.config.root.join("evolve").join(format!("run-{ts}"));
        fs::create_dir_all(&run_dir).map_err(|e| {
            OneAIError::Config(format!("create run_dir {}: {}", run_dir.display(), e))
        })?;

        let optimizer = if !self.config.no_optimize {
            self.optimizer.clone()
        } else {
            None
        };
        let target = optimizer
            .as_ref()
            .map(|o| o.config.target_pass_rate)
            .unwrap_or(1.0);
        let patience = optimizer
            .as_ref()
            .map(|o| o.config.early_stop_patience)
            .unwrap_or(usize::MAX);
        let token_cap = optimizer.as_ref().and_then(|o| o.config.max_total_tokens);

        let mut lessons = LessonsLog::new(run_dir.join("lessons.jsonl"));
        let mut base = seed.clone();
        let mut total_tokens: u64 = 0;
        let mut generations: Vec<GenerationSummary> = Vec::new();
        // Final-gen accumulators (lifted to the report top level).
        let mut final_gen = 0usize;
        let mut final_case_records = Vec::new();
        let mut final_diagnoses = Vec::new();
        let mut final_candidate_scores = Vec::new();
        let mut final_frontier: Option<FrontierRecord> = None;
        let mut stop_reason: Option<String> = None;

        for gen in 0..self.config.max_generations {
            final_gen = gen;
            // ① full-suite runs of this generation's base.
            let runs = self.collect_runs(&base, suite).await?;
            let gen_tokens: u64 = runs
                .iter()
                .map(|r| r.result.prompt_tokens + r.result.completion_tokens)
                .sum();
            total_tokens += gen_tokens;

            let mut case_records = Vec::with_capacity(runs.len());
            for (_case, run) in suite.cases.iter().zip(&runs) {
                case_records.push(CaseRecord::from_run(&run.result, &run.trajectory, None));
            }
            let total_cases = case_records.len();
            let passed = case_records.iter().filter(|c| c.passed).count();
            let base_pass_rate = if total_cases == 0 {
                0.0
            } else {
                passed as f64 / total_cases as f64
            };

            // ③ diagnose failures (in-memory diagnoses feed the next step's
            // variation; only the final gen's diagnoses are persisted).
            let failed: Vec<FailedCase<'_>> = extract_failures(&suite.cases, &runs, &base);
            let failed_ids: Vec<String> = failed.iter().map(|f| f.case.id.clone()).collect();
            let mut diagnoses: Vec<Diagnosis> = Vec::with_capacity(failed.len());
            let mut diag_records = Vec::with_capacity(failed.len());
            for fc in &failed {
                let d = self.diagnostician.diagnose(fc).await;
                diag_records.push(DiagnosisRecord {
                    case_id: fc.case.id.clone(),
                    suspect_params: d.suspect_params.clone(),
                    critique: d.critique.clone(),
                    diagnosis_file: None,
                });
                diagnoses.push(d);
            }

            // ④ optimization: vary K → score subset → Pareto. The base is
            // always candidate 0 so the frontier can be the base itself.
            let (candidate_scores, frontier_rec, frontier_scored) = match &optimizer {
                Some(opt) => {
                    let ratio = opt.config.case_subset_ratio;
                    let subset = select_case_subset(suite, ratio, &failed_ids);
                    let subset_ids: HashSet<String> =
                        subset.cases.iter().map(|c| c.id.clone()).collect();
                    let base_subset_runs: Vec<CaseRun> = suite
                        .cases
                        .iter()
                        .zip(&runs)
                        .filter(|(c, _)| subset_ids.contains(&c.id))
                        .map(|(_, r)| r.clone())
                        .collect();
                    let base_scored = ScoredCandidate::from_runs(base.clone(), &base_subset_runs);
                    let step = self
                        .run_optimization(&base, base_scored, &diagnoses, &subset, &run_dir, gen)
                        .await?;
                    (
                        step.candidate_scores,
                        step.frontier_rec,
                        step.frontier_scored,
                    )
                }
                None => (Vec::new(), None, Vec::new()),
            };

            // Frontier-best axes (subset) for the lesson + convergence.
            let (frontier_pass, frontier_tok, frontier_lat, frontier_is_seed) = match &frontier_rec
            {
                Some(f) => (f.pass_rate, f.total_tokens, f.total_latency_ms, f.is_seed),
                None => (base_pass_rate, gen_tokens, 0, true),
            };
            let frontier_cfg_file = frontier_rec.as_ref().and_then(|f| f.config_file.clone());

            // Merge the frontier into the next-gen base + lessons text. With
            // no optimizer, the base carries forward unchanged.
            let (next_base, lessons_text) = match &optimizer {
                Some(opt) => opt.merge(&frontier_scored, &base).await,
                None => (
                    base.clone(),
                    "no optimization — base carried forward unchanged".into(),
                ),
            };

            // Record the lesson + generation summary.
            lessons.record(LessonEntry {
                generation: gen,
                base_pass_rate,
                frontier_pass_rate: frontier_pass,
                frontier_total_tokens: frontier_tok,
                frontier_total_latency_ms: frontier_lat,
                frontier_is_seed,
                lessons_text: lessons_text.clone(),
            });
            generations.push(GenerationSummary {
                generation: gen,
                base_pass_rate,
                frontier_pass_rate: frontier_pass,
                frontier_total_tokens: frontier_tok,
                frontier_total_latency_ms: frontier_lat,
                frontier_is_seed,
                candidate_scores: candidate_scores.clone(),
                frontier_config_file: frontier_cfg_file.clone(),
                lessons_text,
            });

            // Lift this generation's final-gen accumulators (overwritten each
            // gen so the last one wins).
            final_candidate_scores = candidate_scores;
            final_frontier = frontier_rec.clone();

            // Convergence / stop checks (design §4 E4).
            if frontier_pass >= target {
                stop_reason = Some(format!(
                    "converged: frontier pass_rate {frontier_pass:.2} ≥ target {target:.2} (gen {gen})"
                ));
            }
            if let Some(cap) = token_cap {
                if total_tokens >= cap {
                    stop_reason = Some(format!(
                        "budget cap: cumulative tokens {total_tokens} ≥ {cap} (gen {gen})"
                    ));
                }
            }
            if gen > 0 && lessons.gens_without_improvement() >= patience {
                stop_reason = Some(format!(
                    "stagnation: {patience} consecutive generation(s) with no improvement (gen {gen})"
                ));
            }
            let mut stopping = stop_reason.is_some();
            if gen + 1 == self.config.max_generations {
                // Reached the cap — record it if no other reason fired first.
                if stop_reason.is_none() {
                    stop_reason = Some(format!("max_generations {gen} reached"));
                }
                stopping = true;
            }

            if stopping {
                // Persist the final generation's trajectories + diagnoses
                // (intermediate gens only record their lesson row). `case_records`
                // + `diag_records` are owned here; `runs` is borrowed immutably
                // alongside `failed`'s borrow (both read-only — fine).
                final_case_records = persist_case_records(&run_dir, case_records, &runs)?;
                final_diagnoses = persist_diagnoses(&run_dir, diag_records, &diagnoses)?;
                break;
            }

            // Next-gen base + carry the final-gen accumulators forward. Only
            // the final generation persists trajectories; intermediate gens
            // just record their lesson row.
            base = next_base;
        }

        let lessons_file = if lessons.is_empty() {
            None
        } else {
            let rel = PathBuf::from("lessons.jsonl");
            lessons.persist()?;
            Some(rel)
        };

        let report = EvolutionReport::for_run(
            &suite.name,
            self.config.no_optimize,
            run_dir.clone(),
            generations,
            final_gen,
            final_case_records,
            final_diagnoses,
            final_candidate_scores,
            final_frontier,
            lessons_file,
            stop_reason,
        );
        persist_report(&run_dir, &report)?;
        Ok(report)
    }

    /// Build a fresh `App` per `candidate` (separate recorder + provider
    /// wrap) and drive the loop per case, returning the captured `CaseRun`s.
    /// Shared by the seed's gen-0 run + each variation candidate's scoring.
    /// Live (not replayed) — semantic-variation candidates can't be replayed
    /// (design §3.3 难点 C).
    async fn collect_runs(
        &self,
        candidate: &CandidateConfig,
        suite: &EvalSuite,
    ) -> Result<Vec<CaseRun>> {
        let recorder = Arc::new(RecordingProvider::new(self.baseline.provider.clone()));
        let provider_for_app: Arc<dyn LlmProvider> = recorder.clone();
        let AppHandle(app) = candidate
            .build_app(provider_for_app, &self.baseline.project_dir)
            .await?;
        let app = Arc::new(app);
        let collector = TrajectoryCollector::new(app, recorder);
        let mut runs = Vec::with_capacity(suite.cases.len());
        for case in &suite.cases {
            let run = collector.run_case(case, &suite.metrics).await;
            runs.push(run);
        }
        Ok(runs)
    }

    /// The single-generation optimization: vary K candidates → score on the
    /// subset → Pareto select → persist the frontier config. Returns the
    /// candidate-score records + the frontier record + the scored frontier
    /// (the merger carries the frontier forward as the next-gen base in E4).
    /// `gen` parameterizes the persisted config filename (`frontier-gen<n>.json`).
    async fn run_optimization(
        &self,
        base: &CandidateConfig,
        base_scored: ScoredCandidate,
        diagnoses: &[Diagnosis],
        subset: &EvalSuite,
        run_dir: &Path,
        gen: usize,
    ) -> Result<OptStep> {
        let optimizer = self
            .optimizer
            .as_ref()
            .expect("optimizer set (caller checks no_optimize)");

        // Vary K candidates (operator drops invalid/cheat ones with a warn).
        let candidates = optimizer.vary(diagnoses, base).await;

        let mut candidate_scores: Vec<CandidateScoreRecord> = Vec::with_capacity(candidates.len());
        let mut scored: Vec<ScoredCandidate> = Vec::with_capacity(candidates.len() + 1);
        // Base as candidate 0 so Pareto can compare "do nothing" against the
        // variations — the frontier may be the base itself (no improvement).
        scored.push(base_scored.clone());

        for (i, cand) in candidates.iter().enumerate() {
            let runs = self.collect_runs(cand, subset).await?;
            let passed = runs.iter().filter(|r| r.result.passed()).count();
            let total = runs.len();
            let sc = ScoredCandidate::from_runs(cand.clone(), &runs);
            candidate_scores.push(CandidateScoreRecord {
                index: i + 1,
                pass_rate: sc.pass_rate,
                total_tokens: sc.total_tokens,
                total_latency_ms: sc.total_latency_ms,
                passed,
                total_cases: total,
            });
            scored.push(sc);
        }

        let frontier = optimizer.select(&scored);
        let frontier_rec = match frontier.first() {
            Some(b) => {
                // Is the frontier-best the base? Compare the three axes —
                // an exact match means no candidate improved on the base, so
                // nothing new to persist. (Collision only when a candidate
                // matches the base on all axes — then not persisting is
                // harmless: it's the same quality.)
                let is_seed = b.pass_rate == base_scored.pass_rate
                    && b.total_tokens == base_scored.total_tokens
                    && b.total_latency_ms == base_scored.total_latency_ms;
                let config_file = if is_seed {
                    None
                } else {
                    let name = PathBuf::from(format!("frontier-gen{gen}.json"));
                    let path = run_dir.join(&name);
                    let json =
                        serde_json::to_string_pretty(&b.candidate.pack_config).map_err(|e| {
                            OneAIError::Config(format!("serialize frontier config: {e}"))
                        })?;
                    fs::write(&path, json).map_err(|e| {
                        OneAIError::Config(format!("write {}: {}", path.display(), e))
                    })?;
                    Some(name)
                };
                Some(FrontierRecord {
                    pass_rate: b.pass_rate,
                    total_tokens: b.total_tokens,
                    total_latency_ms: b.total_latency_ms,
                    config_file,
                    is_seed,
                })
            }
            None => None,
        };
        Ok(OptStep {
            candidate_scores,
            frontier_rec,
            frontier_scored: frontier,
        })
    }
}

/// One generation's optimization output — folded into the loop's lesson +
/// report by the caller.
struct OptStep {
    candidate_scores: Vec<CandidateScoreRecord>,
    frontier_rec: Option<FrontierRecord>,
    frontier_scored: Vec<ScoredCandidate>,
}

/// Write the per-case trajectory to `run_dir/case-<id>.jsonl` (one JSON line).
/// Returns the relative filename (stored in `CaseRecord.trajectory_file`).
fn persist_trajectory(
    run_dir: &Path,
    case_id: &str,
    trajectory: &oneai_eval::Trajectory,
) -> Result<PathBuf> {
    let safe = sanitize_filename(case_id);
    let name = PathBuf::from(format!("case-{safe}.jsonl"));
    let path = run_dir.join(&name);
    let line = serde_json::to_string(trajectory)
        .map_err(|e| OneAIError::Config(format!("serialize trajectory {case_id}: {e}")))?;
    fs::write(&path, format!("{line}\n"))
        .map_err(|e| OneAIError::Config(format!("write {}: {}", path.display(), e)))?;
    Ok(name)
}

/// Write the per-case diagnosis to `run_dir/diagnosis-<id>.json`. Returns the
/// relative filename (stored in `DiagnosisRecord.diagnosis_file`).
fn persist_diagnosis(run_dir: &Path, case_id: &str, diagnosis: &Diagnosis) -> Result<PathBuf> {
    let safe = sanitize_filename(case_id);
    let name = PathBuf::from(format!("diagnosis-{safe}.json"));
    let path = run_dir.join(&name);
    let json = serde_json::to_string_pretty(diagnosis)
        .map_err(|e| OneAIError::Config(format!("serialize diagnosis {case_id}: {e}")))?;
    fs::write(&path, json)
        .map_err(|e| OneAIError::Config(format!("write {}: {}", path.display(), e)))?;
    Ok(name)
}

/// Persist the final generation's per-case trajectories + fill the
/// `trajectory_file` pointer on each [`CaseRecord`] (parallel to `runs`).
/// Returns the records with paths filled. Called once per run, on the final
/// generation (intermediate gens skip disk — their data stays in-memory).
fn persist_case_records(
    run_dir: &Path,
    mut case_records: Vec<CaseRecord>,
    runs: &[CaseRun],
) -> Result<Vec<CaseRecord>> {
    for (rec, run) in case_records.iter_mut().zip(runs) {
        let traj_rel = persist_trajectory(run_dir, &rec.case_id, &run.trajectory)?;
        rec.trajectory_file = Some(traj_rel);
    }
    Ok(case_records)
}

/// Persist the final generation's per-failed-case diagnoses + fill the
/// `diagnosis_file` pointer on each [`DiagnosisRecord`] (parallel to
/// `diagnoses`). Called once per run, on the final generation.
fn persist_diagnoses(
    run_dir: &Path,
    mut diag_records: Vec<DiagnosisRecord>,
    diagnoses: &[Diagnosis],
) -> Result<Vec<DiagnosisRecord>> {
    for (rec, d) in diag_records.iter_mut().zip(diagnoses) {
        let rel = persist_diagnosis(run_dir, &rec.case_id, d)?;
        rec.diagnosis_file = Some(rel);
    }
    Ok(diag_records)
}

/// Write the aggregate report to `run_dir/report.json`.
fn persist_report(run_dir: &Path, report: &EvolutionReport) -> Result<()> {
    let path = run_dir.join("report.json");
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| OneAIError::Config(format!("serialize report: {e}")))?;
    fs::write(&path, json)
        .map_err(|e| OneAIError::Config(format!("write {}: {}", path.display(), e)))?;
    Ok(())
}

/// Make a case ID safe as a single path component (replace separators).
fn sanitize_filename(id: &str) -> String {
    id.replace(['/', '\\'], "_")
}
