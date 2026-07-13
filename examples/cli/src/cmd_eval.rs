//! Eval CLI commands — run and list evaluation suites.
//!
//! Subcommands:
//!   oneai eval list   — List available eval suites
//!   oneai eval run    — Run an eval suite and output a report
//!   oneai eval score  — Run metrics only (no agent execution)

use std::sync::Arc;

use oneai_eval::{
    EvalRunner, ScoreOnlyRunner,
    SuiteRegistry,
    builtin_suites,
    replay::replay_trajectory,
    swebench::{
        load_instances_filtered, render_swebench_leaderboard, write_prediction_jsonl,
        SwebenchRunner, SwebenchRunnerConfig,
    },
};

use crate::config::OneaiConfig;

/// List available eval suites.
pub fn cmd_eval_list() {
    let registry = SuiteRegistry::with_builtins();

    println!("Available Eval Suites:\n");
    for (name, desc) in registry.list() {
        let suite = registry.get(name).unwrap();
        println!("  {} — {}", name, desc);
        println!("    Cases: {} | Metrics: {} | Domain: {}",
            suite.case_count(),
            suite.metric_count(),
            suite.domain.as_deref().unwrap_or("none"));
        println!();
    }
}

/// Run an eval suite and output the report.
///
/// This requires an LLM provider to be configured (ONEAI_API_KEY env var).
/// Without a provider, cases will be marked as errors.
pub async fn cmd_eval_run(suite_name: &str, output_format: &str, profile: bool, record: Option<String>) {
    // Get the suite
    let suite = builtin_suites::get_builtin_suite(suite_name)
        .unwrap_or_else(|| {
            eprintln!("Suite '{}' not found. Available: coding_basics, tool_use, general", suite_name);
            std::process::exit(1);
        });

    println!("Running eval suite: {}", suite_name);
    println!("Cases: {} | Metrics: {}", suite.case_count(), suite.metric_count());
    println!();

    // Build the app
    let config = OneaiConfig::load_or_default();
    let provider_config = config.to_model_config_with_overrides(None);
    if provider_config.is_none() {
        eprintln!("Error: No LLM provider configured for eval.");
        eprintln!("Set ONEAI_API_KEY or configure ~/.oneai/config.toml");
        std::process::exit(1);
    }
    let model_config = provider_config.unwrap();
    let real_provider = oneai_provider::ProviderFactory::create(model_config);

    // When recording, wrap the provider so every infer() response is captured
    // into a trajectory for later `eval replay`. Otherwise use the provider
    // directly. We keep an `Arc<dyn LlmProvider>` handle either way.
    let (provider_handle, recorder) = if let Some(_path) = &record {
        let rec = std::sync::Arc::new(oneai_eval::RecordingProvider::new(
            std::sync::Arc::from(real_provider),
        ));
        let handle: std::sync::Arc<dyn oneai_core::traits::LlmProvider> = rec.clone();
        (handle, Some(rec))
    } else {
        (std::sync::Arc::from(real_provider), None)
    };

    // When recording, run only the first case (with all metrics) so the
    // trajectory isn't polluted by responses from other cases.
    let single_case_suite = if recorder.is_some() {
        if suite.cases.is_empty() {
            eprintln!("Suite has no cases to record.");
            std::process::exit(1);
        }
        Some(
            oneai_eval::EvalSuiteBuilder::new(&suite.name)
                .description(&suite.description)
                .case(suite.cases[0].clone())
                .metrics(suite.metrics.clone())
                .build(),
        )
    } else {
        None
    };
    let run_suite = single_case_suite.as_ref().unwrap_or(&suite);

    let app = oneai_app::AppBuilder::new()
        .provider(provider_handle)
        .noop_interaction_gate()
        .trace_in_memory()
        .default_parser()
        // Usage tracker + token counter feed the efficiency axis (tokens,
        // api_calls) so `--profile` has real numbers rather than zeros.
        .default_usage_tracker()
        .default_token_counter()
        .build()
        .await
        .expect("App build should succeed");

    let runner = EvalRunner::from_app(app);
    let report = runner.run(run_suite).await
        .expect("Eval run should succeed");

    // Output the report
    match output_format {
        "json" => println!("{}", report.to_json().unwrap()),
        "compact" => println!("{}", oneai_eval::render_compact(&report)),
        "markdown" | _ => println!("{}", report.to_markdown()),
    }

    if profile {
        print_efficiency_profile(&report);
    }

    // Save the recorded trajectory for the first case.
    if let (Some(rec), Some(path)) = (recorder, record) {
        let r = &report.results[0];
        let (tool_calls, iters) = if let Some(eff) = &r.efficiency {
            // We don't keep tool NAMES in EfficiencyProfile, only counts; for the
            // trajectory's recorded_tool_calls we'd need the span tree, which the
            // EvalRunner doesn't surface. Record the count-based digest; full
            // name extraction is a future enhancement. For now, empty list means
            // "determinism check skips name comparison" (replay still checks iterations).
            let _ = eff;
            (Vec::new(), eff.iterations)
        } else {
            (Vec::new(), 0)
        };
        let traj = rec.trajectory(&suite.cases[0].input, tool_calls, iters).await;
        match traj.save(std::path::Path::new(&path)) {
            Ok(_) => println!("\nRecorded trajectory ({} responses) → {}", traj.responses.len(), path),
            Err(e) => eprintln!("Warning: could not write trajectory: {}", e),
        }
    }
}

/// Print the efficiency axis (three-axis: quality × tokens × latency) for
/// each case + an aggregate. Backs the `oneai eval run --profile` flag.
fn print_efficiency_profile(report: &oneai_eval::EvalReport) {
    println!("\n── Efficiency Axis (能力 × 成本 × 效率) ────────────────");
    println!(
        "{:<24} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "case", "infer_ms", "tool_ms", "overhd", "iters", "tokens", "cache%", "3axis"
    );

    let mut tot_infer = 0u64;
    let mut tot_tool = 0u64;
    let mut tot_overhead = 0u64;
    let mut tot_tokens = 0u64;
    let mut tot_cache_read = 0u64;
    let mut n = 0usize;
    let mut three_axis_sum = 0.0f64;

    for r in &report.results {
        let p = match r.efficiency.as_ref() {
            Some(p) => p,
            None => {
                println!("{:<24} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
                    truncate(&r.case_id, 24), "-", "-", "-", "-", "-", "-", "n/a");
                continue;
            }
        };
        let quality = r.avg_score();
        let three = p.three_axis_score(quality);
        let cache_pct = p.cache_hit_ratio() * 100.0;
        println!(
            "{:<24} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7.1}% {:>8.3}",
            truncate(&r.case_id, 24),
            p.inference_ms,
            p.tool_ms,
            p.overhead_ms,
            p.iterations,
            p.total_tokens,
            cache_pct,
            three,
        );
        tot_infer += p.inference_ms;
        tot_tool += p.tool_ms;
        tot_overhead += p.overhead_ms;
        tot_tokens += p.total_tokens;
        tot_cache_read += p.cache_read_tokens;
        n += 1;
        three_axis_sum += three;
    }

    if n > 0 {
        println!(
            "\n{:<24} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7.1}% {:>8.3}",
            "TOTAL/AVG",
            tot_infer,
            tot_tool,
            tot_overhead,
            "",
            tot_tokens,
            if tot_tokens + tot_cache_read == 0 { 0.0 } else {
                tot_cache_read as f64 / (tot_tokens + tot_cache_read) as f64 * 100.0
            },
            three_axis_sum / n as f64,
        );
    }
    println!("\n3-axis = quality / (1 + 0.1·log(1+tokens) + 0.1·log(1+latency_ms))");
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n.saturating_sub(1)])
    }
}

/// Replay a recorded trajectory (ghost replay) — re-run with a frozen provider
/// (no live LLM) and check determinism. Backs `oneai eval replay <path>`.
pub async fn cmd_eval_replay(path: &str) {
    let p = std::path::Path::new(path);
    println!("Replaying trajectory: {}", p.display());

    let result = replay_trajectory(p).await;
    match result {
        Ok(r) => {
            println!("\n── Replay Result (ghost replay / loop test) ────────");
            println!("deterministic: {}", r.deterministic);
            println!("tool calls match: {} (replayed {} vs recorded {})",
                r.tool_calls_match(),
                r.replayed_tool_calls.len(),
                r.recorded_tool_calls.len());
            if r.replayed_tool_calls != r.recorded_tool_calls {
                println!("  replayed:  {:?}", r.replayed_tool_calls);
                println!("  recorded:  {:?}", r.recorded_tool_calls);
            }
            println!("iterations: replayed {} vs recorded {}",
                r.replayed_iterations, r.recorded_iterations);
            if let Some(eff) = &r.efficiency {
                println!("\nreplay efficiency (frozen — wall-clock not meaningful):");
                println!("  inference_calls: {}, tool_calls: {}, iterations: {}, tokens: {}",
                    eff.inference_calls, eff.tool_calls, eff.iterations, eff.total_tokens);
            }
            if !r.deterministic {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Error replaying trajectory: {}", e);
            std::process::exit(1);
        }
    }
}

/// Run metrics only — score pre-collected outputs without agent execution.
///
/// This is useful for testing metrics, re-scoring outputs, or CI integration
/// where agent execution was done separately.
pub async fn cmd_eval_score(suite_name: &str) {
    let suite = builtin_suites::get_builtin_suite(suite_name)
        .unwrap_or_else(|| {
            eprintln!("Suite '{}' not found. Available: coding_basics, tool_use, general", suite_name);
            std::process::exit(1);
        });

    println!("Running metrics-only eval: {}", suite_name);

    // Build cases for score-only evaluation
    // For demo purposes, we use the expected answers as actual outputs
    // (simulating a perfect agent)
    let cases: Vec<(String, String, oneai_eval::ExpectedOutput)> = suite.cases.iter()
        .map(|case| {
            let actual = match &case.expected {
                oneai_eval::ExpectedOutput::Exact { answer } => answer.clone(),
                oneai_eval::ExpectedOutput::Contains { substrings, .. } => substrings.join(" "),
                oneai_eval::ExpectedOutput::Regex { pattern } => format!("matches {}", pattern),
                oneai_eval::ExpectedOutput::LlmJudge { rubric, .. } => format!("judged on: {}", rubric),
                oneai_eval::ExpectedOutput::Trajectory { expected_tools, .. } => expected_tools.join(" "),
                _ => String::new(),
            };
            (case.input.clone(), actual, case.expected.clone())
        })
        .collect();

    let metrics: Vec<Arc<dyn oneai_eval::EvalMetric>> = suite.metrics.clone();

    let report = ScoreOnlyRunner::score(&cases, &metrics, &suite.name).await;

    println!("{}", report.to_markdown());
}

/// Run SWE-bench instances — the three-axis eval (resolved × cost × efficiency).
///
/// Loads instances from a local JSONL dataset, clones each repo at base_commit,
/// drives the agent on the problem statement, captures `git diff` as the patch,
/// and judges it via the external SWE-bench harness. Writes the prediction
/// JSONL + leaderboard JSON into the workspace alongside the report.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_eval_swebench(
    dataset: &str,
    instances: Option<&str>,
    workspace: &str,
    python: Option<&str>,
    modal: bool,
    dataset_name: &str,
    limit: usize,
    format: &str,
    run_id: &str,
) {
    // Load instances (optionally filtered by id list).
    let ids: Vec<String> = instances
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let dataset_path = std::path::Path::new(dataset);
    let all_instances = load_instances_filtered(dataset_path, &ids);
    if all_instances.is_empty() {
        eprintln!(
            "No instances loaded from '{}' (filter: {:?}). \
             Download the SWE-bench dataset JSONL or use scripts/swebench/fetch_instance.py.",
            dataset, ids,
        );
        std::process::exit(1);
    }
    let runnable = all_instances.iter().filter(|i| i.is_runnable()).count();
    println!(
        "SWE-bench: loaded {} instances ({} runnable) from {}",
        all_instances.len(),
        runnable,
        dataset_path.display(),
    );

    // Build the provider — required (real agent = real cost).
    let config = OneaiConfig::load_or_default();
    let provider_config = config.to_model_config_with_overrides(None);
    if provider_config.is_none() {
        eprintln!("Error: No LLM provider configured for SWE-bench eval.");
        eprintln!("Set ONEAI_API_KEY or configure ~/.oneai/config.toml");
        std::process::exit(1);
    }
    let model_config = provider_config.unwrap();
    let provider = oneai_provider::ProviderFactory::create(model_config);

    // CodingPack gives read_file/edit_file/grep/glob/shell — what the agent
    // needs to navigate + edit a real repo. Cost tracker + trace wire the
    // 成本 / 效率 axes; auto-approval so the run is unattended.
    let project_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .into_owned();
    let coding_pack = oneai_domain::coding_pack(&project_dir);

    let app = oneai_app::AppBuilder::new()
        .provider(Arc::from(provider))
        .noop_interaction_gate()
        .default_parser()
        .default_usage_tracker()
        .default_token_counter()
        .trace_in_memory()
        .domain_pack(coding_pack)
        .build()
        .await
        .expect("App build should succeed");

    let python_path = python
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{}/.venvs/swebench/bin/python", home)
        });

    let runner_config = SwebenchRunnerConfig {
        workspace_dir: std::path::PathBuf::from(workspace),
        python_path,
        use_modal: modal,
        dataset_name: dataset_name.to_string(),
        max_instances: limit,
        run_id: run_id.to_string(),
    };

    println!(
        "Workspace: {} | python: {} | modal: {} | dataset: {} | run_id: {}",
        runner_config.workspace_dir.display(),
        runner_config.python_path,
        runner_config.use_modal,
        runner_config.dataset_name,
        runner_config.run_id,
    );

    let runner = SwebenchRunner::new(app, runner_config);
    let report = runner
        .run(&all_instances)
        .await
        .expect("SWE-bench run should succeed");

    // Write artifacts into the workspace.
    let ws = std::path::Path::new(workspace);
    let _ = std::fs::create_dir_all(ws);
    let predictions_path = ws.join("predictions.jsonl");
    let leaderboard_path = ws.join("leaderboard.json");
    match write_prediction_jsonl(&report, &predictions_path) {
        Ok(n) => println!("Wrote {} prediction(s) → {}", n, predictions_path.display()),
        Err(e) => eprintln!("Warning: could not write predictions: {}", e),
    }
    let leaderboard = render_swebench_leaderboard(&report);
    match serde_json::to_string_pretty(&leaderboard) {
        Ok(json) => {
            let _ = std::fs::write(&leaderboard_path, json);
            println!(
                "Leaderboard: {}/{} resolved ({:.1}%) | {} api calls → {}",
                leaderboard.resolved_count,
                leaderboard.total_instances,
                leaderboard.resolution_rate * 100.0,
                leaderboard.instance_calls,
                leaderboard_path.display(),
            );
        }
        Err(e) => eprintln!("Warning: could not serialize leaderboard: {}", e),
    }

    // Report.
    match format {
        "json" => println!("{}", report.to_json().unwrap()),
        "compact" => println!("{}", oneai_eval::render_compact(&report)),
        "markdown" | _ => println!("{}", report.to_markdown()),
    }
}
