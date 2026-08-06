//! `oneai evolve` subcommand — self-evolution loop driver.
//!
//! Phase E1 ships only `run`:
//!   oneai evolve run --seed <pack.yaml> --suite <name> [--no-optimize] [--root <dir>]
//!
//! It hot-loads a seed DomainPack from `--seed`, scores it against a builtin
//! eval suite, and persists a generation-0 report + per-case trajectories under
//! `<root>/evolve/run-<ts>/`. No diagnosis / variation / Pareto (E2–E3). The
//! crate is provider-agnostic; this command wires the real provider (mirroring
//! `cmd_eval_run`) from `OneaiConfig` / `ONEAI_API_KEY`.

use std::sync::Arc;

use oneai_domain::DomainPackSpecFile;
use oneai_eval::builtin_suites;
use oneai_evolve::{run_evolve, EvolveRunArgs};

use crate::config::OneaiConfig;

/// `oneai evolve run` — generation-0 (degenerate, no optimization) run.
pub async fn cmd_evolve_run(
    seed: &str,
    suite: &str,
    no_optimize: bool,
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
        "Evolve run (E1, no-optimize): seed={seed} suite={suite_name}",
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

    let args = EvolveRunArgs::new(seed_config, suite).with_no_optimize(no_optimize);
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
