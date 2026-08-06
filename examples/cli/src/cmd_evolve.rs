//! `oneai evolve` subcommand — self-evolution loop driver.
//!
//! Subcommands (E5 full set):
//! - `run` — hot-load a seed pack, score against a builtin suite, persist a
//!   report + per-case trajectories + (E4) `lessons.jsonl`. `--no-optimize`
//!   runs the E1/E2 degenerate path; otherwise an E3 single-gen or E4
//!   multi-gen run (when `--max-generations > 1`).
//! - `step <run-dir>` — resume an existing run for one more generation
//!   (E5). Reads `report.json` for the gen index + `no_optimize` flag, loads
//!   the latest frontier config (or `seed.json`) as the new base, appends a
//!   lesson row, rewrites `report.json`.
//! - `report <run-dir>` — pretty-print a persisted `report.json`.
//! - `diff <run-dir>` — structured config diff: seed vs the latest frontier.
//! - `lesson <run-dir>` — print the cross-generation `lessons.jsonl`.
//!
//! The crate is provider-agnostic; this command wires the real provider
//! (mirroring `cmd_eval_run`) from `OneaiConfig` / `ONEAI_API_KEY`. The
//! `--judge-model` flag wires a *separate* variation (optimizer) provider —
//! design §6.3's judge/candidate separation; absent it, the candidate
//! provider doubles as the variation provider (smoke-harness mode).

use std::sync::Arc;

use oneai_core::traits::LlmProvider;
use oneai_domain::{DomainPackConfig, DomainPackSpecFile};
use oneai_eval::builtin_suites;
use oneai_evolve::{
    config_diff, AppBaseline, EvolutionConfig, EvolutionLoop, EvolutionReport, EvolveRunArgs,
    GepaConfig, LessonsLog,
};

use crate::config::OneaiConfig;

/// Build a provider from `OneaiConfig`, optionally overriding the model name
/// (used for the separate judge/variation provider).
fn build_provider(
    config: &OneaiConfig,
    model_override: Option<&str>,
) -> Option<Arc<dyn LlmProvider>> {
    let model_config = config.to_model_config_with_overrides(model_override)?;
    let real_provider = oneai_provider::ProviderFactory::create(model_config);
    Some(Arc::from(real_provider))
}

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
    judge_model: Option<&str>,
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
        "Evolve run: seed={seed_path} suite={suite_name} | no_optimize={no_optimize} | \
         max_generations={max_generations} | target={target} | patience={patience}",
        seed_path = seed,
        suite_name = suite.name
    );
    println!(
        "Cases: {} | Metrics: {}",
        suite.case_count(),
        suite.metric_count()
    );

    // Build the candidate provider from config (ONEAI_API_KEY or ~/.oneai/config.toml).
    let config = OneaiConfig::load_or_default();
    let provider = match build_provider(&config, None) {
        Some(p) => p,
        None => {
            eprintln!("Error: No LLM provider configured for evolve.");
            eprintln!("Set ONEAI_API_KEY or configure ~/.oneai/config.toml");
            std::process::exit(1);
        }
    };

    let mut args = EvolveRunArgs::new(seed_config, suite)
        .with_no_optimize(no_optimize)
        .with_max_generations(max_generations);
    // When optimizing, wire the variation provider. A separate judge model
    // (design §6.3) takes priority; absent it, the candidate provider doubles
    // as the variation provider (single-model smoke harness — real
    // cross-family separation uses the library API).
    if !no_optimize {
        let mut gepa = GepaConfig::new()
            .with_target_pass_rate(target)
            .with_early_stop_patience(patience);
        if let Some(t) = max_tokens {
            gepa = gepa.with_max_total_tokens(t);
        }
        let variation_provider = build_provider(&config, judge_model).unwrap_or_else(|| {
            eprintln!("Error: No LLM provider configured for the variation/judge model.");
            eprintln!("Set ONEAI_API_KEY or pass --judge-model with a configured provider.");
            std::process::exit(1);
        });
        if judge_model.is_some() {
            println!("judge/variation model: {judge_model:?} (separate from candidate)");
        }
        args = args
            .with_variation_provider(variation_provider)
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

    let report = match oneai_evolve::run_evolve(args, provider, &project_dir).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Evolve run failed: {e:?}");
            std::process::exit(1);
        }
    };

    print_report(&report, format);
}

/// `oneai evolve step <run-dir>` — resume one more generation (E5).
pub async fn cmd_evolve_step(run_dir: &str, suite: &str, judge_model: Option<&str>, format: &str) {
    let run_dir = std::path::PathBuf::from(run_dir);
    let report_path = run_dir.join("report.json");
    let prev: EvolutionReport = load_report(&report_path);

    let suite = builtin_suites::get_builtin_suite(suite).unwrap_or_else(|| {
        eprintln!(
            "Suite '{suite}' not found. Available: {:?}",
            builtin_suites::builtin_suite_names()
        );
        std::process::exit(1);
    });

    let config = OneaiConfig::load_or_default();
    let provider = match build_provider(&config, None) {
        Some(p) => p,
        None => {
            eprintln!("Error: No LLM provider configured for evolve step.");
            std::process::exit(1);
        }
    };
    let project_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let mut ev = EvolutionLoop::new(AppBaseline::new(provider, project_dir)).with_config(
        EvolutionConfig::new(
            run_dir
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf(),
        )
        .with_no_optimize(prev.no_optimize)
        .with_max_generations(1),
    );
    // Reconstruct the optimizer if the original run was optimized.
    if !prev.no_optimize {
        let variation_provider = build_provider(&config, judge_model).unwrap_or_else(|| {
            eprintln!("Error: No LLM provider configured for the variation/judge model.");
            std::process::exit(1);
        });
        let gepa = GepaConfig::new();
        let optimizer = Arc::new(oneai_evolve::GepaOptimizer::with_llm_operator(
            variation_provider,
            gepa,
        ));
        ev = ev.with_optimizer(optimizer);
    }

    let report = match ev.run_one_more(&run_dir, &suite).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Evolve step failed: {e:?}");
            std::process::exit(1);
        }
    };
    print_report(&report, format);
}

/// `oneai evolve report <run-dir>` — pretty-print a persisted report.
pub fn cmd_evolve_report(run_dir: &str, format: &str) {
    let report_path = std::path::PathBuf::from(run_dir).join("report.json");
    let report = load_report(&report_path);
    print_report(&report, format);
}

/// `oneai evolve lesson <run-dir>` — print the cross-generation lessons log.
pub fn cmd_evolve_lesson(run_dir: &str, format: &str) {
    let lessons_path = std::path::PathBuf::from(run_dir).join("lessons.jsonl");
    let log = LessonsLog::load(lessons_path.clone()).unwrap_or_else(|e| {
        eprintln!("Error loading lessons '{}': {e:?}", lessons_path.display());
        std::process::exit(1);
    });
    if log.is_empty() {
        println!("(no lessons recorded in {})", lessons_path.display());
        return;
    }
    match format {
        "json" => {
            let arr: Vec<&oneai_evolve::LessonEntry> = log.entries().iter().collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&arr)
                    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
            );
        }
        _ => {
            println!(
                "lessons ({} generation{}) — {}",
                log.len(),
                if log.len() == 1 { "" } else { "s" },
                lessons_path.display()
            );
            let trailing_stale = log.gens_without_improvement();
            for e in log.entries() {
                let stale_marker = if e.generation == log.len() - 1 && trailing_stale > 0 {
                    format!(" | stagnant ×{trailing_stale}")
                } else {
                    String::new()
                };
                println!(
                    "  gen {} | base {:.0}% | frontier {:.0}% (tok {} lat {}ms) | {}{}",
                    e.generation,
                    e.base_pass_rate * 100.0,
                    e.frontier_pass_rate * 100.0,
                    e.frontier_total_tokens,
                    e.frontier_total_latency_ms,
                    if e.frontier_is_seed {
                        "seed"
                    } else {
                        "frontier"
                    },
                    stale_marker,
                );
                if !e.lessons_text.is_empty() {
                    for line in e.lessons_text.lines().take(1) {
                        println!("    {line}");
                    }
                }
            }
        }
    }
}

/// `oneai evolve diff <run-dir> [--gen N] [--seed <file>]` — structured
/// config diff between the seed and a generation's frontier.
pub fn cmd_evolve_diff(run_dir: &str, gen: Option<usize>, seed_file: Option<&str>, format: &str) {
    let run_dir = std::path::PathBuf::from(run_dir);
    // Resolve the frontier config: explicit --gen, else the latest
    // frontier-gen{N}.json, else none (frontier was the seed → diff is empty).
    let frontier_path = match gen {
        Some(n) => Some(run_dir.join(format!("frontier-gen{n}.json"))),
        None => latest_frontier(&run_dir),
    };
    let seed_cfg: DomainPackConfig = match seed_file {
        Some(p) => {
            let spec = DomainPackSpecFile::load(std::path::Path::new(p)).unwrap_or_else(|e| {
                eprintln!("Error loading seed pack '{p}': {e}");
                std::process::exit(1);
            });
            spec.config
        }
        None => load_json(&run_dir.join("seed.json"), "seed.json"),
    };
    let frontier_cfg = match frontier_path {
        Some(p) if p.exists() => load_json::<DomainPackConfig>(&p, &p.display().to_string()),
        _ => {
            // No persisted frontier (the run's frontier was the seed).
            let d = config_diff(&seed_cfg, &seed_cfg);
            println!(
                "(no frontier config persisted — frontier was the seed; diff is empty)\n{}",
                d.to_markdown()
            );
            return;
        }
    };
    let diff = config_diff(&seed_cfg, &frontier_cfg);
    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(&diff)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
        ),
        _ => print!("{}", diff.to_markdown()),
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

fn load_report(path: &std::path::Path) -> EvolutionReport {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Error loading report '{}': {e}", path.display());
        std::process::exit(1);
    }))
    .unwrap_or_else(|e| {
        eprintln!("Error parsing report '{}': {e}", path.display());
        std::process::exit(1);
    })
}

fn load_json<T: serde::de::DeserializeOwned>(path: &std::path::Path, label: &str) -> T {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Error loading {label} '{}': {e}", path.display());
        std::process::exit(1);
    }))
    .unwrap_or_else(|e| {
        eprintln!("Error parsing {label} '{}': {e}", path.display());
        std::process::exit(1);
    })
}

fn latest_frontier(run_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut max_gen: Option<usize> = None;
    if let Ok(entries) = std::fs::read_dir(run_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("frontier-gen") {
                if let Some(num) = rest.strip_suffix(".json") {
                    if let Ok(n) = num.parse::<usize>() {
                        max_gen = Some(max_gen.map_or(n, |m| m.max(n)));
                    }
                }
            }
        }
    }
    max_gen.map(|n| run_dir.join(format!("frontier-gen{n}.json")))
}

fn print_report(report: &EvolutionReport, format: &str) {
    match format {
        "json" => println!(
            "{}",
            serde_json::to_string_pretty(report)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
        ),
        _ => print!("{}", report.to_summary()),
    }
}
