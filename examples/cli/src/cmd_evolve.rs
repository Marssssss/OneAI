//! `oneai evolve` subcommand — self-evolution loop driver.
//!
//! `oneai evolve run --seed <pack.yaml> --suite <name> [--no-optimize]
//! [--max-generations N] [--target 0.85] [--patience 2] [--max-tokens N]
//! [--root <dir>]`
//!
//! It hot-loads a seed DomainPack from `--seed`, scores it against a builtin
//! eval suite, and persists a report + per-case trajectories + (E4)
//! `lessons.jsonl` under `<root>/evolve/run-<ts>/`. `--no-optimize` runs the
//! E1/E2 degenerate path; otherwise an E3 single-gen or E4 multi-gen run
//! (when `--max-generations > 1`). The crate is provider-agnostic; this
//! command wires the real provider (mirroring `cmd_eval_run`) from
//! `OneaiConfig` / `ONEAI_API_KEY`.

use std::sync::Arc;

use oneai_domain::DomainPackSpecFile;
use oneai_eval::builtin_suites;
use oneai_evolve::{run_evolve, EvolveRunArgs, GepaConfig};

use crate::config::OneaiConfig;

/// `oneai evolve run` — E1 degenerate, E3 single-gen, or E4 multi-gen run.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_evolve_run(
    seed: &str,
    suite: &str,
    no_optimize: bool,
    max_generations: usize,
    target: f64,
    patience: usize,
    max_tokens: Option<u64>,
    root: Option<&str>,
    format: &str,
) {
    // Load the seed pack config (YAML/TOML) → validate (build_app re-runs the
    // validator, but surface a friendly error here if the file won't parse).
    let spec_file = DomainPackSpecFile::load(std::path::Path::new(seed)).unwrap_or_else(|e| {
        eprintln!("Error loading seed pack '{seed}': {e}");
        std::process::exit(1);
    });
    let seed_config = spec_file.config;

    // Load the builtin suite (coding_basics / tool_use / general / efficiency).
    let suite = builtin_suites::get_builtin_suite(suite).unwrap_or_else(|| {
        eprintln!(
            "Suite '{suite}' not found. Available: {:?}",
            builtin_suites::builtin_suite_names()
        );
        std::process::exit(1);
    });

    println!(
        "Evolve run: seed={seed} suite={suite_name} | no_optimize={no_optimize} | \
         max_generations={max_generations} | target={target} | patience={patience}",
        suite_name = suite.name
    );
    println!(
        "Cases: {} | Metrics: {}",
        suite.case_count(),
        suite.metric_count()
    );

    // Build the provider from config (ONEAI_API_KEY or ~/.oneai/config.toml).
    let config = OneaiConfig::load_or_default();
    let provider_config = config.to_model_config_with_overrides(None);
    if provider_config.is_none() {
        eprintln!("Error: No LLM provider configured for evolve.");
        eprintln!("Set ONEAI_API_KEY or configure ~/.oneai/config.toml");
        std::process::exit(1);
    }
    let model_config = provider_config.unwrap();
    let real_provider = oneai_provider::ProviderFactory::create(model_config);
    let provider: Arc<dyn oneai_core::traits::LlmProvider> = Arc::from(real_provider);

    let mut args = EvolveRunArgs::new(seed_config, suite)
        .with_no_optimize(no_optimize)
        .with_max_generations(max_generations);
    // When optimizing, the variation provider == the candidate provider here
    // (the CLI is a single-model smoke harness; design §6.3 wants a separate
    // stronger judge — wire that via the library API for real runs). Pass the
    // GEPA config through so the CLI flags take effect.
    if !no_optimize {
        let mut gepa = GepaConfig::new()
            .with_target_pass_rate(target)
            .with_early_stop_patience(patience);
        if let Some(t) = max_tokens {
            gepa = gepa.with_max_total_tokens(t);
        }
        args = args
            .with_variation_provider(provider.clone())
            .with_gepa_config(gepa);
    }
    let args = if let Some(r) = root {
        args.with_root(std::path::PathBuf::from(r))
    } else {
        args
    };
    let project_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let report = match run_evolve(args, provider, &project_dir).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Evolve run failed: {e:?}");
            std::process::exit(1);
        }
    };

    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
        ),
        _ => print!("{}", report.to_summary()),
    }
}
