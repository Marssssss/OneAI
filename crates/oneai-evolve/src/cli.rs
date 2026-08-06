//! CLI-facing entry for `oneai evolve run`. The crate is provider-agnostic
//! (like `EvalRunner`) — the caller injects the provider, the crate drives the
//! loop. The thin `cmd_evolve.rs` in `examples/cli` does the provider wiring
//! (mirroring `cmd_eval`) and calls [`run_evolve`].
//!
//! E1 ships only `run`. `step` / `report` / `diff` / `lesson` land in E3–E5.

use std::path::PathBuf;
use std::sync::Arc;

use oneai_core::error::Result;
use oneai_core::traits::LlmProvider;
use oneai_domain::DomainPackConfig;
use oneai_eval::EvalSuite;

use crate::candidate::CandidateConfig;
use crate::loop_runner::{AppBaseline, EvolutionConfig, EvolutionLoop};
use crate::report::EvolutionReport;

/// Arguments for `oneai evolve run` (provider injected separately).
#[non_exhaustive]
#[derive(Debug)]
pub struct EvolveRunArgs {
    /// Seed pack config (loaded from `--seed <file>` by the CLI; the crate
    /// validates+builds it via `DomainPackSpecFile::validate_and_build`).
    pub seed_config: DomainPackConfig,
    /// Suite to score against.
    pub suite: EvalSuite,
    /// Skip variation (E1 default — true until E3 lands).
    pub no_optimize: bool,
    /// Output root override (default `~/.oneai/evolve`-ish; see
    /// `EvolutionConfig::default`).
    pub root: Option<PathBuf>,
}

impl EvolveRunArgs {
    /// Construct with a seed config + suite; E1 defaults (`no_optimize=true`,
    /// `root=None`).
    pub fn new(seed_config: DomainPackConfig, suite: EvalSuite) -> Self {
        Self {
            seed_config,
            suite,
            no_optimize: true,
            root: None,
        }
    }

    /// Set the output root.
    #[must_use]
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root = Some(root);
        self
    }

    /// Toggle variation (E3 sets this false).
    #[must_use]
    pub fn with_no_optimize(mut self, no_optimize: bool) -> Self {
        self.no_optimize = no_optimize;
        self
    }
}

/// Run generation 0 (E1 degenerate): hot-load the seed, run the suite live,
/// capture per-case trajectories, persist a report. Returns the report.
///
/// The provider is injected by the caller. The crate wraps it in a
/// [`oneai_eval::RecordingProvider`] internally so trajectories are captured
/// without the caller wiring a recorder.
pub async fn run_evolve(
    args: EvolveRunArgs,
    provider: Arc<dyn LlmProvider>,
    project_dir: &str,
) -> Result<EvolutionReport> {
    let seed = CandidateConfig::from_pack_config(args.seed_config);
    let config = EvolutionConfig {
        root: args.root.unwrap_or_else(crate::loop_runner::default_root),
        max_generations: 1,
        no_optimize: args.no_optimize,
    };
    let baseline = AppBaseline::new(provider, project_dir);
    let loop_runner = EvolutionLoop::new(baseline).with_config(config);
    loop_runner.run(&seed, &args.suite).await
}
