//! E5 e2e: CLI safety gates + regression gates + `step` resume.
//!
//! 1. `e5_invalid_patch_dropped_no_panic` — an out-of-range `recall.top_k`
//!    patch (value "0") is rejected by `apply_patch`'s bound guard; the
//!    candidate is dropped; the frontier is the seed; nothing panics.
//! 2. `e5_permission_safety_check_rejects_widening` — a candidate that
//!    auto-approves a tool the seed gates behind `require_confirmation` /
//!    `deny_by_default` is rejected by the E5 permission gate (a static
//!    pack-level check; no current patch op widens permissions, so this
//!    guards the forward-looking permission-axis variation).
//! 3. `e5_overfit_held_out_below_train_and_replay_deterministic` — a 3-case
//!    suite with `case_subset_ratio` selecting 2 cases: the frontier
//!    candidate beats the base on the subset but fails the held-out-only
//!    case → `held_out_pass_rate < frontier_pass_rate` (overfit flagged) +
//!    `replay_deterministic == Some(true)` (numeric mutation, direct-answer
//!    trajectory).
//! 4. `e5_semantic_mutation_skips_replay` — a `system_prompt` patch →
//!    `replay_deterministic == None` (semantic mutation skips replay per
//!    design §6.4).
//! 5. `e5_step_resumes_one_more_generation` — `run_one_more` on an existing
//!    run-dir increments the generation, appends a lesson row, and rewrites
//!    `report.json`.

use std::collections::HashMap;
use std::sync::Arc;

use oneai_agent::{mock_provider::ScriptedResponse, MockProvider};
use oneai_core::traits::LlmProvider;
use oneai_domain::{CompressionTemplateConfig, DomainPackConfig, PermissionProfileConfig};
use oneai_eval::{EvalCase, EvalSuiteBuilder, ExactMatchMetric, ExpectedOutput};
use oneai_evolve::{
    config_diff, permission_safety_check, AppBaseline, CandidateConfig, EvolutionConfig,
    EvolutionLoop, GepaConfig,
};

fn coding_seed_config(prompt: &str) -> DomainPackConfig {
    DomainPackConfig {
        name: "coding_seed".to_string(),
        description: "Coding pack seed for evolve E5 e2e".to_string(),
        tools: vec!["read_file".to_string(), "calculator".to_string()],
        tool_decorators: HashMap::new(),
        context_sources: vec![],
        permission_profile: PermissionProfileConfig {
            auto_approve: vec!["read_file".to_string(), "calculator".to_string()],
            require_confirmation: vec![],
            deny_by_default: vec![],
            ..Default::default()
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

/// A seed with a strictly-gated tool (shell behind require_confirmation) —
/// for the permission-safety unit test.
fn gated_seed_config() -> DomainPackConfig {
    let mut cfg = coding_seed_config("You are a coding agent.");
    cfg.tools.push("shell".to_string());
    cfg.permission_profile.require_confirmation = vec!["shell".to_string()];
    cfg
}

fn two_case_suite() -> oneai_eval::EvalSuite {
    let metrics: Vec<Arc<dyn oneai_eval::EvalMetric>> = vec![Arc::new(ExactMatchMetric)];
    EvalSuiteBuilder::new("evolve_e5")
        .description("2-case exact-match suite for E5")
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

/// 3-case suite: the third case (`math_multiply`) is the held-out-only case
/// under a 2-case subset.
fn three_case_suite() -> oneai_eval::EvalSuite {
    let metrics: Vec<Arc<dyn oneai_eval::EvalMetric>> = vec![Arc::new(ExactMatchMetric)];
    EvalSuiteBuilder::new("evolve_e5_overfit")
        .description("3-case exact-match suite for E5 overfit gate")
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
        .case(EvalCase::with_id(
            "math_multiply",
            "What is 2*4?",
            ExpectedOutput::exact("8"),
        ))
        .metrics(metrics)
        .build()
}

fn recall_top_k_patch_json(val: &str) -> String {
    serde_json::json!({
        "patches": [
            {"param": "pack.memory.recall.top_k", "op": "set", "value": val}
        ]
    })
    .to_string()
}

fn prompt_patch_json() -> String {
    serde_json::json!({
        "patches": [
            {"param": "pack.system_prompt", "op": "set", "value": "Answer with just the number."}
        ]
    })
    .to_string()
}

#[tokio::test]
async fn e5_invalid_patch_dropped_no_panic() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Candidate provider: 2 base (wrong) + 2 candidate subset (right) + 2
    // held-out (right) — but the candidate is dropped before scoring because
    // the patch is invalid (top_k=0). So only the 2 base responses are
    // consumed; the rest are unused.
    let candidate_provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("5"), // base math_add (wrong)
        ScriptedResponse::direct_answer("5"), // base math_subtract (wrong)
    ]);
    let candidate_provider: Arc<dyn LlmProvider> = Arc::new(candidate_provider);
    // Variation provider emits an out-of-range patch (top_k=0).
    let variation_provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        recall_top_k_patch_json("0"),
    )]);
    let variation_provider: Arc<dyn LlmProvider> = Arc::new(variation_provider);

    let baseline = AppBaseline::new(candidate_provider, tmp.path().to_string_lossy().to_string());
    let gepa_cfg = GepaConfig::new()
        .with_population(1)
        .with_case_subset_ratio(1.0)
        .with_target_pass_rate(2.0)
        .with_early_stop_patience(5);
    let optimizer = Arc::new(oneai_evolve::GepaOptimizer::with_llm_operator(
        variation_provider,
        gepa_cfg,
    ));
    let ev = EvolutionLoop::new(baseline)
        .with_config(
            EvolutionConfig::new(tmp.path().to_path_buf())
                .with_no_optimize(false)
                .with_max_generations(1),
        )
        .with_optimizer(optimizer);

    let seed = CandidateConfig::from_pack_config(coding_seed_config(
        "You are a coding agent. Answer directly.",
    ));
    let report = ev.run(&seed, &two_case_suite()).await.expect("evolve run");

    // No candidate survived → frontier is the seed (base). The held-out /
    // replay gates skip on a seed frontier.
    let frontier = report.frontier.as_ref().expect("frontier record present");
    assert!(
        frontier.is_seed,
        "invalid candidate dropped → frontier is seed"
    );
    assert_eq!(frontier.replay_deterministic, None);
    assert_eq!(report.generations[0].held_out_pass_rate, None);
}

#[test]
fn e5_permission_safety_check_rejects_widening() {
    // Seed gates `shell` behind require_confirmation; candidate auto-approves
    // it → rejected.
    let seed = CandidateConfig::from_pack_config(gated_seed_config());
    let mut widened = gated_seed_config();
    widened
        .permission_profile
        .auto_approve
        .push("shell".to_string());
    widened
        .permission_profile
        .require_confirmation
        .retain(|t| t != "shell");
    let candidate = CandidateConfig::from_pack_config(widened);

    let err = permission_safety_check(&seed, &candidate).expect_err("widening must be rejected");
    assert!(
        err.contains("shell"),
        "error names the regressed tool: {err}"
    );

    // A tightening move (auto_approve → require_confirmation) is allowed.
    let mut tightened = coding_seed_config("p");
    tightened
        .permission_profile
        .require_confirmation
        .push("calculator".to_string());
    tightened
        .permission_profile
        .auto_approve
        .retain(|t| t != "calculator");
    let seed2 = CandidateConfig::from_pack_config(coding_seed_config("p"));
    permission_safety_check(&seed2, &CandidateConfig::from_pack_config(tightened))
        .expect("tightening is allowed");

    // No strictly-gated tools → Ok regardless.
    let s = CandidateConfig::from_pack_config(coding_seed_config("p"));
    permission_safety_check(&s, &s).expect("identical → Ok");
}

#[tokio::test]
async fn e5_overfit_held_out_below_train_and_replay_deterministic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let suite = three_case_suite();

    // Candidate provider script (8 responses):
    // - base full-suite (3, all wrong): "5","5","5"
    // - candidate subset run (2, right): "4","7"  (subset = math_add, math_subtract)
    // - held-out full-suite run (3): "4","7","99" (math_multiply wrong → held_out 2/3)
    let script: Vec<ScriptedResponse> = vec![
        ScriptedResponse::direct_answer("5"),  // base math_add
        ScriptedResponse::direct_answer("5"),  // base math_subtract
        ScriptedResponse::direct_answer("5"),  // base math_multiply
        ScriptedResponse::direct_answer("4"),  // candidate math_add
        ScriptedResponse::direct_answer("7"),  // candidate math_subtract
        ScriptedResponse::direct_answer("4"),  // held-out math_add
        ScriptedResponse::direct_answer("7"),  // held-out math_subtract
        ScriptedResponse::direct_answer("99"), // held-out math_multiply (wrong)
    ];
    let candidate_provider = MockProvider::from_script(script);
    let candidate_provider: Arc<dyn LlmProvider> = Arc::new(candidate_provider);
    let variation_provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        recall_top_k_patch_json("8"),
    )]);
    let variation_provider: Arc<dyn LlmProvider> = Arc::new(variation_provider);

    let baseline = AppBaseline::new(candidate_provider, tmp.path().to_string_lossy().to_string());
    // ratio 0.34 → subset cap = ceil(3*0.34) = 2 = {math_add, math_subtract}.
    let gepa_cfg = GepaConfig::new()
        .with_population(1)
        .with_case_subset_ratio(0.34)
        .with_target_pass_rate(2.0)
        .with_early_stop_patience(5);
    let optimizer = Arc::new(oneai_evolve::GepaOptimizer::with_llm_operator(
        variation_provider,
        gepa_cfg,
    ));
    let ev = EvolutionLoop::new(baseline)
        .with_config(
            EvolutionConfig::new(tmp.path().to_path_buf())
                .with_no_optimize(false)
                .with_max_generations(1),
        )
        .with_optimizer(optimizer);

    let seed = CandidateConfig::from_pack_config(coding_seed_config(
        "You are a coding agent. Answer directly.",
    ));
    let report = ev.run(&seed, &suite).await.expect("evolve run");

    // Frontier is the candidate (beat the base on the subset).
    let frontier = report.frontier.as_ref().expect("frontier present");
    assert!(!frontier.is_seed, "candidate beat base on subset");
    assert_eq!(frontier.pass_rate, 1.0, "frontier subset pass 2/2");

    // Held-out gate: frontier on full suite = 2/3 < 1.0 → overfit flagged.
    let held = report.generations[0]
        .held_out_pass_rate
        .expect("held-out ran on final gen");
    assert!(
        held + 1e-9 < frontier.pass_rate,
        "held-out {held} < train {} (overfit)",
        frontier.pass_rate
    );
    assert!(
        report.to_summary().contains("overfit"),
        "report flags overfit"
    );

    // Numeric mutation (recall.top_k only) + direct-answer trajectory →
    // replay ran + confirmed determinism.
    assert_eq!(
        frontier.replay_deterministic,
        Some(true),
        "numeric mutation → replay deterministic"
    );

    // config_diff reports the numeric-only change.
    let frontier_cfg_path = report.run_dir.join(
        frontier
            .config_file
            .as_ref()
            .expect("frontier config persisted"),
    );
    let frontier_cfg: DomainPackConfig =
        serde_json::from_str(&std::fs::read_to_string(&frontier_cfg_path).unwrap()).unwrap();
    let d = config_diff(&seed.pack_config, &frontier_cfg);
    assert!(!d.is_empty());
    assert!(d.numeric_only, "recall.top_k-only diff is numeric-only");
}

#[tokio::test]
async fn e5_semantic_mutation_skips_replay() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let suite = two_case_suite();

    // Candidate provider: 2 base (wrong) + 2 candidate subset (right) + 2
    // held-out (right) = 6 responses.
    let script: Vec<ScriptedResponse> = vec![
        ScriptedResponse::direct_answer("5"), // base math_add
        ScriptedResponse::direct_answer("5"), // base math_subtract
        ScriptedResponse::direct_answer("4"), // candidate math_add
        ScriptedResponse::direct_answer("7"), // candidate math_subtract
        ScriptedResponse::direct_answer("4"), // held-out math_add
        ScriptedResponse::direct_answer("7"), // held-out math_subtract
    ];
    let candidate_provider = MockProvider::from_script(script);
    let candidate_provider: Arc<dyn LlmProvider> = Arc::new(candidate_provider);
    let variation_provider =
        MockProvider::from_script(vec![ScriptedResponse::direct_answer(prompt_patch_json())]);
    let variation_provider: Arc<dyn LlmProvider> = Arc::new(variation_provider);

    let baseline = AppBaseline::new(candidate_provider, tmp.path().to_string_lossy().to_string());
    let gepa_cfg = GepaConfig::new()
        .with_population(1)
        .with_case_subset_ratio(1.0)
        .with_target_pass_rate(2.0)
        .with_early_stop_patience(5);
    let optimizer = Arc::new(oneai_evolve::GepaOptimizer::with_llm_operator(
        variation_provider,
        gepa_cfg,
    ));
    let ev = EvolutionLoop::new(baseline)
        .with_config(
            EvolutionConfig::new(tmp.path().to_path_buf())
                .with_no_optimize(false)
                .with_max_generations(1),
        )
        .with_optimizer(optimizer);

    let seed = CandidateConfig::from_pack_config(coding_seed_config(
        "You are a coding agent. Answer directly.",
    ));
    let report = ev.run(&seed, &suite).await.expect("evolve run");

    let frontier = report.frontier.as_ref().expect("frontier present");
    assert!(!frontier.is_seed, "candidate beat base");
    // Semantic mutation (system_prompt) → replay skipped.
    assert_eq!(
        frontier.replay_deterministic, None,
        "semantic mutation skips replay"
    );
    // No overfit (held-out == train, both 1.0).
    let held = report.generations[0]
        .held_out_pass_rate
        .expect("held-out ran");
    assert!(
        held + 1e-9 >= frontier.pass_rate,
        "no overfit: held {held} >= train {}",
        frontier.pass_rate
    );
}

#[tokio::test]
async fn e5_step_resumes_one_more_generation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let suite = two_case_suite();

    // First run: no_optimize, 1 generation, all right.
    let provider1 = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("4"),
        ScriptedResponse::direct_answer("7"),
    ]);
    let provider1: Arc<dyn LlmProvider> = Arc::new(provider1);
    let baseline1 = AppBaseline::new(provider1, tmp.path().to_string_lossy().to_string());
    let ev1 = EvolutionLoop::new(baseline1).with_config(
        EvolutionConfig::new(tmp.path().to_path_buf())
            .with_no_optimize(true)
            .with_max_generations(1),
    );
    let seed =
        CandidateConfig::from_pack_config(coding_seed_config("Answer with just the number."));
    let report1 = ev1.run(&seed, &suite).await.expect("first run");
    assert_eq!(report1.generation, 0);
    assert_eq!(report1.generations.len(), 1);
    assert!(
        report1.run_dir.join("seed.json").exists(),
        "seed.json persisted"
    );

    // Step: fresh provider + a new EvolutionLoop, run_one_more on the run-dir.
    let provider2 = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("4"),
        ScriptedResponse::direct_answer("7"),
    ]);
    let provider2: Arc<dyn LlmProvider> = Arc::new(provider2);
    let baseline2 = AppBaseline::new(provider2, tmp.path().to_string_lossy().to_string());
    let ev2 = EvolutionLoop::new(baseline2).with_config(
        EvolutionConfig::new(tmp.path().to_path_buf())
            .with_no_optimize(true)
            .with_max_generations(1),
    );
    let report2 = ev2
        .run_one_more(&report1.run_dir, &suite)
        .await
        .expect("step run");

    // Generation advanced to 1; the report carries 2 generation summaries.
    assert_eq!(report2.generation, 1, "gen advanced");
    assert_eq!(report2.generations.len(), 2, "two generation summaries");
    assert_eq!(
        report2.stop_reason.as_deref(),
        Some("step: resumed at gen 1"),
        "step stop reason"
    );

    // lessons.jsonl now has 2 rows (gen0 + gen1).
    let lessons_rel = report2.lessons_file.as_ref().expect("lessons_file");
    let body = std::fs::read_to_string(report2.run_dir.join(lessons_rel)).unwrap();
    assert_eq!(body.lines().count(), 2, "two lesson rows");
    for (i, line) in body.lines().enumerate() {
        let entry: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(entry["generation"], i, "lesson row {i} generation");
    }
}
