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
pub async fn cmd_eval_run(suite_name: &str, output_format: &str) {
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
    let provider = oneai_provider::ProviderFactory::create(model_config);

    let app = oneai_app::AppBuilder::new()
        .provider(Arc::from(provider))
        .auto_approval_gate()
        .trace_in_memory()
        .default_parser()
        .build()
        .await
        .expect("App build should succeed");

    let runner = EvalRunner::from_app(app);
    let report = runner.run(&suite).await
        .expect("Eval run should succeed");

    // Output the report
    match output_format {
        "json" => println!("{}", report.to_json().unwrap()),
        "compact" => println!("{}", oneai_eval::render_compact(&report)),
        "markdown" | _ => println!("{}", report.to_markdown()),
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
