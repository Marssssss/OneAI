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
use crate::gepa::GepaConfig;
use crate::loop_runner::{AppBaseline, EvolutionConfig, EvolutionLoop};
use crate::report::EvolutionReport;

/// Arguments for `oneai evolve run` (provider injected separately).
#[non_exhaustive]
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
    /// E3: dedicated variation provider (the "optimizer model"). Required when
    /// `no_optimize == false` (kept separate from the candidate provider per
    /// design §6.3 to avoid self-eval bias). `None` → optimization is skipped
    /// even if `no_optimize == false`.
    /// Not part of the `Debug` impl (a `dyn LlmProvider` has no Debug).
    pub variation_provider: Option<Arc<dyn LlmProvider>>,
    /// E3: GEPA config (population K, case_subset_ratio, target). Defaults
    /// applied when `None`.
    pub gepa_config: Option<GepaConfig>,
}

impl std::fmt::Debug for EvolveRunArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvolveRunArgs")
            .field("seed_config", &self.seed_config)
            .field("suite", &self.suite.name)
            .field("no_optimize", &self.no_optimize)
            .field("root", &self.root)
            .field(
                "variation_provider",
                &self.variation_provider.as_ref().map(|_| "<set>"),
            )
            .field("gepa_config", &self.gepa_config)
            .finish()
    }
}

impl EvolveRunArgs {
    /// Construct with a seed config + suite; E1 defaults (`no_optimize=true`,
    /// `root=None`, no variation provider).
    pub fn new(seed_config: DomainPackConfig, suite: EvalSuite) -> Self {
        Self {
            seed_config,
            suite,
            no_optimize: true,
            root: None,
            variation_provider: None,
            gepa_config: None,
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

    /// Wire a dedicated variation provider (the "optimizer model"). Only
    /// consulted when `no_optimize == false`.
    #[must_use]
    pub fn with_variation_provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.variation_provider = Some(provider);
        self
    }

    /// Override the GEPA config (population K, case-subset ratio, target).
    #[must_use]
    pub fn with_gepa_config(mut self, cfg: GepaConfig) -> Self {
        self.gepa_config = Some(cfg);
        self
    }
}

/// Run generation 0 (E1 degenerate, or E3 single-gen optimized if a variation
/// provider is wired and `no_optimize == false`): hot-load the seed, run the
/// suite live, capture per-case trajectories, persist a report. Returns the
/// report.
///
/// The candidate `provider` is injected by the caller. The crate wraps it in a
/// [`oneai_eval::RecordingProvider`] internally so trajectories are captured
/// without the caller wiring a recorder. The variation provider (if any) is a
/// *separate* provider — the caller passes it via [`EvolveRunArgs::with_variation_provider`].
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
    let mut loop_runner = EvolutionLoop::new(baseline).with_config(config);
    // E3: wire the optimizer when a variation provider is present and
    // optimization is on. Without a variation provider, the loop silently
    // degrades to the E1/E2 no-optimize path (the caller forgot the seam).
    if !args.no_optimize {
        if let Some(vp) = args.variation_provider {
            let gepa_cfg = args.gepa_config.unwrap_or_default();
            let optimizer = crate::gepa::GepaOptimizer::with_llm_operator(vp, gepa_cfg);
            loop_runner = loop_runner.with_optimizer(Arc::new(optimizer));
        }
    }
    loop_runner.run(&seed, &args.suite).await
}
