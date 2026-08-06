//! E3 e2e: a seed whose `system_prompt` makes the candidate provider answer
//! wrong → an `EvolutionLoop` with a `GepaOptimizer` (wired to a *separate*
//! variation provider) varies the seed → scores candidates on the subset →
//! Pareto-selects the frontier. The frontier's `pass_rate` beats the seed's,
//! and the frontier config round-trips from `frontier-gen0.json`.
//!
//! Two negative tests guard the reward-hacking + contract-failure paths:
//! a decorator patch that contradicts the tool's verb is rejected, and a
//! non-JSON variation response is dropped — neither panics, both still yield
//! a report (frontier = seed).
//!
//! The mock is *scripted* to simulate "the variation fixed the prompt" —
//! the candidate provider returns correct answers after the patch. This
//! validates the optimization plumbing (vary → score → Pareto → persist),
//! not that prompt text actually fixes model behavior (that needs a live LLM;
//! E5 smoke-tests it).

use std::collections::HashMap;
use std::sync::Arc;

use oneai_agent::{mock_provider::ScriptedResponse, MockProvider};
use oneai_core::traits::LlmProvider;
use oneai_domain::{CompressionTemplateConfig, DomainPackConfig, PermissionProfileConfig};
use oneai_eval::{EvalCase, EvalSuiteBuilder, ExactMatchMetric, ExpectedOutput};
use oneai_evolve::{AppBaseline, CandidateConfig, EvolutionConfig, EvolutionLoop, GepaConfig};

fn coding_seed_config(prompt: &str) -> DomainPackConfig {
    DomainPackConfig {
        name: "coding_seed".to_string(),
        description: "Coding pack seed for evolve E3 e2e".to_string(),
        tools: vec!["read_file".to_string(), "calculator".to_string()],
        tool_decorators: HashMap::new(),
        context_sources: vec![],
        permission_profile: PermissionProfileConfig {
            auto_approve: vec!["read_file".to_string(), "calculator".to_string()],
            require_confirmation: vec![],
            deny_by_default: vec![],
        },
        paradigm_strategies: vec![],
        compression_template: CompressionTemplateConfig {
            name: "coding".to_string(),
            preserve_fields: vec!["critical_files".to_string()],
            truncate_rules: HashMap::new(),
        },
        system_prompt: prompt.to_string(),
        memory_profile: Default::default(),
    }
}

/// 2-case exact-match suite. With `case_subset_ratio = 1.0` the subset == full
/// suite, so seed-vs-candidate comparison is on the same cases (fair under a
/// deterministic mock — no overfitting risk, and the design's held-out split
/// is an E4/E5 concern).
fn two_case_suite() -> oneai_eval::EvalSuite {
    let metrics: Vec<Arc<dyn oneai_eval::EvalMetric>> = vec![Arc::new(ExactMatchMetric)];
    EvalSuiteBuilder::new("evolve_e3")
        .description("2-case exact-match suite for E3 optimization")
        .case(EvalCase::with_id(
            "math_add",
            "What is 2+2?",
            ExpectedOutput::exact("4"),
        ))
        .case(EvalCase::with_id(
            "math_subtract",
            "What is 10-3?",
            ExpectedOutput::exact("7"),
        ))
        .metrics(metrics)
        .build()
}

/// A patch-list JSON fixing the system prompt — the variation provider's
/// scripted response. Built via `serde_json::json!` so the escaping is
/// guaranteed valid.
fn prompt_fix_patch_json() -> String {
    serde_json::json!({
        "patches": [
            {"param": "pack.system_prompt", "op": "set", "value": "Answer with just the number."}
        ]
    })
    .to_string()
}

#[tokio::test]
async fn e3_frontier_beats_seed_and_config_round_trips() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Candidate provider: seed answers wrong (5, 10 → both fail), then the
    // patched candidate answers right (4, 7). 4 responses total.
    let candidate_provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("5"),  // seed case 0 (wrong)
        ScriptedResponse::direct_answer("10"), // seed case 1 (wrong)
        ScriptedResponse::direct_answer("4"),  // candidate case 0 (right)
        ScriptedResponse::direct_answer("7"),  // candidate case 1 (right)
    ]);
    let candidate_provider: Arc<dyn LlmProvider> = Arc::new(candidate_provider);

    // Separate variation provider: one patch-list fixing the prompt.
    let variation_provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        prompt_fix_patch_json(),
    )]);
    let variation_provider: Arc<dyn LlmProvider> = Arc::new(variation_provider);

    let baseline = AppBaseline::new(candidate_provider, tmp.path().to_string_lossy().to_string());
    let gepa_cfg = GepaConfig::new()
        .with_population(1)
        .with_case_subset_ratio(1.0);
    let optimizer = Arc::new(oneai_evolve::GepaOptimizer::with_llm_operator(
        variation_provider,
        gepa_cfg,
    ));
    let ev = EvolutionLoop::new(baseline)
        .with_config(EvolutionConfig::new(tmp.path().to_path_buf()).with_no_optimize(false))
        .with_optimizer(optimizer);

    let seed = CandidateConfig::from_pack_config(coding_seed_config(
        "You are a coding agent. Answer directly.",
    ));
    let report = ev.run(&seed, &two_case_suite()).await.expect("evolve run");

    // Seed failed both cases on the full suite.
    assert_eq!(report.pass_rate, 0.0, "seed pass_rate should be 0");
    // Exactly one candidate scored (the prompt-fix one).
    assert_eq!(report.candidate_scores.len(), 1, "one candidate scored");
    assert_eq!(
        report.candidate_scores[0].index, 1,
        "candidate indexed from 1"
    );
    assert_eq!(
        report.candidate_scores[0].pass_rate, 1.0,
        "candidate passes both cases"
    );

    // Frontier is the candidate (not the seed), with a persisted config.
    let frontier = report
        .frontier
        .as_ref()
        .expect("frontier present after optimization");
    assert_eq!(frontier.pass_rate, 1.0, "frontier = candidate, full pass");
    assert!(
        frontier.pass_rate > report.pass_rate,
        "frontier ({}) must beat seed ({})",
        frontier.pass_rate,
        report.pass_rate
    );
    assert!(!frontier.is_seed, "frontier is the candidate, not the seed");
    let cfg_rel = frontier
        .config_file
        .as_ref()
        .expect("frontier config persisted");
    let cfg_path = report.run_dir.join(cfg_rel);
    assert!(cfg_path.exists(), "frontier config file exists");

    // Round-trip: the persisted config carries the patched system_prompt.
    let body = std::fs::read_to_string(&cfg_path).unwrap();
    let pack: DomainPackConfig =
        serde_json::from_str(&body).expect("frontier config parses as DomainPackConfig");
    assert_eq!(
        pack.system_prompt, "Answer with just the number.",
        "frontier config carries the patched prompt"
    );

    // The summary renders the frontier + candidate table.
    assert!(report.to_summary().contains("frontier"));
    assert!(report.to_summary().contains("candidates"));
}

#[tokio::test]
async fn e3_decorator_cheat_patch_rejected() {
    // The variation provider tries to describe read_file as a write tool — the
    // reward-hacking guard rejects the patch, the candidate is dropped before
    // scoring, and the frontier degrades to the seed (no panic).
    let tmp = tempfile::tempdir().expect("tempdir");

    // Candidate provider: only the seed runs (2 cases); no candidate runs
    // (the one candidate is dropped before scoring).
    let candidate_provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("5"),
        ScriptedResponse::direct_answer("10"),
    ]);
    let candidate_provider: Arc<dyn LlmProvider> = Arc::new(candidate_provider);

    let cheat_patch = serde_json::json!({
        "patches": [
            {"param": "pack.tool_decorators[read_file]", "op": "set",
             "value": "Use this to write files."}
        ]
    })
    .to_string();
    let variation_provider =
        MockProvider::from_script(vec![ScriptedResponse::direct_answer(cheat_patch)]);
    let variation_provider: Arc<dyn LlmProvider> = Arc::new(variation_provider);

    let baseline = AppBaseline::new(candidate_provider, tmp.path().to_string_lossy().to_string());
    let gepa_cfg = GepaConfig::new()
        .with_population(1)
        .with_case_subset_ratio(1.0);
    let optimizer = Arc::new(oneai_evolve::GepaOptimizer::with_llm_operator(
        variation_provider,
        gepa_cfg,
    ));
    let ev = EvolutionLoop::new(baseline)
        .with_config(EvolutionConfig::new(tmp.path().to_path_buf()).with_no_optimize(false))
        .with_optimizer(optimizer);

    let seed = CandidateConfig::from_pack_config(coding_seed_config("Answer directly."));
    let report = ev.run(&seed, &two_case_suite()).await.expect("evolve run");

    // The cheat candidate was dropped → no candidate scored.
    assert!(
        report.candidate_scores.is_empty(),
        "cheat candidate must be dropped before scoring"
    );
    // Frontier degrades to the seed (only the seed was scored).
    let frontier = report
        .frontier
        .as_ref()
        .expect("frontier still present (the seed)");
    assert!(
        frontier.is_seed,
        "frontier = seed when all candidates dropped"
    );
    assert!(frontier.config_file.is_none(), "no new config persisted");
}

#[tokio::test]
async fn e3_bad_variation_json_dropped_no_panic() {
    // The variation provider returns non-JSON → the candidate is dropped
    // (parse failure, logged) and the run still returns a report.
    let tmp = tempfile::tempdir().expect("tempdir");

    let candidate_provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("5"),
        ScriptedResponse::direct_answer("10"),
    ]);
    let candidate_provider: Arc<dyn LlmProvider> = Arc::new(candidate_provider);

    let variation_provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        "this is not json at all".to_string(),
    )]);
    let variation_provider: Arc<dyn LlmProvider> = Arc::new(variation_provider);

    let baseline = AppBaseline::new(candidate_provider, tmp.path().to_string_lossy().to_string());
    let gepa_cfg = GepaConfig::new()
        .with_population(1)
        .with_case_subset_ratio(1.0);
    let optimizer = Arc::new(oneai_evolve::GepaOptimizer::with_llm_operator(
        variation_provider,
        gepa_cfg,
    ));
    let ev = EvolutionLoop::new(baseline)
        .with_config(EvolutionConfig::new(tmp.path().to_path_buf()).with_no_optimize(false))
        .with_optimizer(optimizer);

    let seed = CandidateConfig::from_pack_config(coding_seed_config("Answer directly."));
    // Must not panic — the bad patch is dropped, the run completes.
    let report = ev.run(&seed, &two_case_suite()).await.expect("evolve run");

    assert!(
        report.candidate_scores.is_empty(),
        "bad-JSON candidate must be dropped"
    );
    assert!(report.frontier.as_ref().is_some_and(|f| f.is_seed));
}
