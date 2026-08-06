//! `EvolutionLoop` — the outer driver. E1's degenerate form runs generation 0
//! only (no diagnosis / variation / Pareto) and persists a report + per-case
//! trajectories. This is the "plumbing" pass: prove the seed hot-loads, the
//! trajectory collector captures per-case `(Trajectory, TraceTree)`, and the
//! report round-trips to disk.
//!
//! E2 adds diagnosis between the collect and report steps; E3 wraps variation
//! around this degenerate `run`; E4 loops generations to convergence. The
//! `EvolutionConfig` fields (`max_generations`, `no_optimize`) are forward-declared
//! so the type is stable — E1 always passes `no_optimize=true` and stops at gen 0.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::LlmProvider;
use oneai_eval::{EvalSuite, RecordingProvider};

use crate::candidate::{AppHandle, CandidateConfig};
use crate::failure_extractor::{extract_failures, FailedCase};
use crate::report::{CaseRecord, DiagnosisRecord, EvolutionReport};
use crate::subgraph::{Diagnosis, HeuristicDiagnostician, SubgraphDiagnostician};
use crate::trajectory_collector::TrajectoryCollector;

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
}

impl EvolutionLoop {
    /// Construct with a baseline + default config + the heuristic
    /// diagnostician (no LLM judge — deterministic, safe for tests).
    pub fn new(baseline: AppBaseline) -> Self {
        Self {
            baseline,
            config: EvolutionConfig::default(),
            diagnostician: Arc::new(HeuristicDiagnostician),
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

    /// Run generation 0 against `suite`, persist a report + per-case
    /// trajectories + per-failed-case diagnoses, and return the report.
    ///
    /// E1 degenerate path — no variation, no Pareto. Each case is run live
    /// (semantic-variation candidates can't be replayed; design §3.3 难点 C),
    /// its trajectory + tree captured by [`TrajectoryCollector`], the
    /// trajectory written to `run-<ts>/case-<id>.jsonl`. E2 adds a diagnosis
    /// pass: failed cases are fed to [`SubgraphDiagnostician`] and the
    /// resulting [`Diagnosis`] (suspect params + subtrace + critique) is
    /// persisted to `run-<ts>/diagnosis-<id>.json` and summarized in the report.
    pub async fn run(&self, seed: &CandidateConfig, suite: &EvalSuite) -> Result<EvolutionReport> {
        // Wrap the baseline provider in a recorder so every infer() response is
        // captured for per-case trajectory slicing.
        let recorder = Arc::new(RecordingProvider::new(self.baseline.provider.clone()));
        let provider_for_app: Arc<dyn LlmProvider> = recorder.clone();

        // Hot-load the seed: validate → build DomainPack → AppBuilder.domain_pack.
        let AppHandle(app) = seed
            .build_app(provider_for_app, &self.baseline.project_dir)
            .await?;
        let app = Arc::new(app);
        let collector = TrajectoryCollector::new(app, recorder);

        // Run dir: <root>/evolve/run-<timestamp>/.
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
        let run_dir = self.config.root.join("evolve").join(format!("run-{ts}"));
        fs::create_dir_all(&run_dir).map_err(|e| {
            OneAIError::Config(format!("create run_dir {}: {}", run_dir.display(), e))
        })?;

        // Drive the loop per case, capturing (Trajectory, TraceTree) + result.
        let mut runs: Vec<crate::trajectory_collector::CaseRun> =
            Vec::with_capacity(suite.cases.len());
        for case in &suite.cases {
            let run = collector.run_case(case, &suite.metrics).await;
            runs.push(run);
        }

        // Persist per-case trajectories + build records. `suite.cases` and
        // `runs` are parallel (same order); zip borrows both.
        let mut records = Vec::with_capacity(runs.len());
        for (case, run) in suite.cases.iter().zip(&runs) {
            let traj_rel = persist_trajectory(&run_dir, &case.id, &run.trajectory)?;
            records.push(CaseRecord::from_run(
                &run.result,
                &run.trajectory,
                Some(traj_rel),
            ));
        }

        // E2: diagnose failures. FailedCase borrows the run's result +
        // trajectory + trace_tree + the seed candidate. The diagnostician
        // always returns a Diagnosis (tail-N fallback); failures only (no
        // wasted work on passing cases).
        let failed: Vec<FailedCase<'_>> = extract_failures(&suite.cases, &runs, seed);
        let mut diagnoses = Vec::with_capacity(failed.len());
        for fc in &failed {
            let d = self.diagnostician.diagnose(fc).await;
            let diag_rel = persist_diagnosis(&run_dir, &fc.case.id, &d)?;
            diagnoses.push(DiagnosisRecord {
                case_id: fc.case.id.clone(),
                suspect_params: d.suspect_params.clone(),
                critique: d.critique.clone(),
                diagnosis_file: Some(diag_rel),
            });
        }

        let report = EvolutionReport::from_records(
            &suite.name,
            0,
            self.config.no_optimize,
            records,
            diagnoses,
            run_dir.clone(),
        );
        persist_report(&run_dir, &report)?;
        Ok(report)
    }
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
