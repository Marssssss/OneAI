//! E4 e2e: the multi-generation loop. Two integration tests:
//!
//! 1. `e4_three_gen_loop_monotonic_and_lessons_persisted` — `max_generations=3`
//!    drives the loop across generations. The merger carries each frontier
//!    forward as the next-gen base; `lessons.jsonl` records one row per gen;
//!    frontier pass_rate is monotonic non-decreasing; the run stops at the
//!    generation cap (target unreachable, patience disabled).
//!
//! 2. `e4_recall_top_k_patch_flows_through_loop` — the E4 MemoryProfile axis:
//!    a variation patch to `pack.memory.recall.top_k` is applied + validated +
//!    carried into the persisted frontier config. The mock scripts the
//!    candidate provider to answer right *only* for the candidate (so the
//!    candidate beats the seed → becomes the frontier), letting us assert the
//!    patched `top_k` round-trips through the whole loop. (Behavioral
//!    recall-set change needs live embedding-backed memory — E5 territory;
//!    this asserts the config-level axis end-to-end.)
//!
//! The mocks are *scripted* — they validate the loop plumbing (vary → score →
//! merge → carry → record → converge), not that prompt/memory text actually
//! changes model behavior (that needs a live LLM; E5 smoke-tests it).

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
        description: "Coding pack seed for evolve E4 e2e".to_string(),
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

/// 2-case exact-match suite. `case_subset_ratio = 1.0` keeps the subset == full
/// suite so seed-vs-candidate comparison is fair + deterministic under mock.
fn two_case_suite() -> oneai_eval::EvalSuite {
    let metrics: Vec<Arc<dyn oneai_eval::EvalMetric>> = vec![Arc::new(ExactMatchMetric)];
    EvalSuiteBuilder::new("evolve_e4")
        .description("2-case exact-match suite for E4 multi-gen loop")
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

/// A patch-list fixing the system prompt — the gen-0 variation.
fn prompt_fix_patch_json() -> String {
    serde_json::json!({
        "patches": [
            {"param": "pack.system_prompt", "op": "set", "value": "Answer with just the number."}
        ]
    })
    .to_string()
}

/// A patch-list bumping recall.top_k — the gen-1 variation (applied on top of
/// the carried-forward gen-0 frontier, exercising the MemoryProfile axis).
/// 8 differs from the `RecallConfig` default of 5 so the patch is observably
/// applied (not the default).
fn recall_top_k_patch_json() -> String {
    serde_json::json!({
        "patches": [
            {"param": "pack.memory.recall.top_k", "op": "set", "value": "8"}
        ]
    })
    .to_string()
}

/// A patch-list adding a fact type to extraction_schema — the gen-2 variation.
fn extraction_schema_patch_json() -> String {
    serde_json::json!({
        "patches": [
            {"param": "pack.memory.extraction_schema", "op": "add", "value": "user_pref"}
        ]
    })
    .to_string()
}

#[tokio::test]
async fn e4_three_gen_loop_monotonic_and_lessons_persisted() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Candidate provider script (12 responses): gen0 base wrong×2, then
    // everything right (gen0 candidate×2, gen1 base×2, gen1 candidate×2,
    // gen2 base×2, gen2 candidate×2).
    let mut script: Vec<ScriptedResponse> = Vec::new();
    script.push(ScriptedResponse::direct_answer("5")); // gen0 base case 0 (wrong)
    script.push(ScriptedResponse::direct_answer("10")); // gen0 base case 1 (wrong)
    for _ in 0..10 {
        // gen0 candidate (2) + gen1 base (2) + gen1 candidate (2) + gen2 base (2) + gen2 candidate (2)
        script.push(ScriptedResponse::direct_answer("4"));
        script.push(ScriptedResponse::direct_answer("7"));
    }
    // Flatten: the loop above pushed 2-per-iteration × 10 = 20, but we only
    // need 10 right answers (5 right pairs). Trim to the exact count.
    script.truncate(2 + 10); // 2 wrong + 10 right = 12
    let candidate_provider = MockProvider::from_script(script);
    let candidate_provider: Arc<dyn LlmProvider> = Arc::new(candidate_provider);

    // Separate variation provider: 3 patch-lists (one per gen).
    let variation_provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer(prompt_fix_patch_json()),
        ScriptedResponse::direct_answer(recall_top_k_patch_json()),
        ScriptedResponse::direct_answer(extraction_schema_patch_json()),
    ]);
    let variation_provider: Arc<dyn LlmProvider> = Arc::new(variation_provider);

    let baseline = AppBaseline::new(candidate_provider, tmp.path().to_string_lossy().to_string());
    // target unreachable (2.0) so convergence can't fire; patience=5 disables
    // early-stop for this 3-gen run → the loop runs to the generation cap.
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
                .with_max_generations(3),
        )
        .with_optimizer(optimizer);

    let seed = CandidateConfig::from_pack_config(coding_seed_config(
        "You are a coding agent. Answer directly.",
    ));
    let report = ev.run(&seed, &two_case_suite()).await.expect("evolve run");

    // Ran all 3 generations.
    assert_eq!(report.generations.len(), 3, "three generations recorded");
    assert_eq!(report.generation, 2, "final generation index is 2");
    assert_eq!(
        report.stop_reason.as_deref(),
        Some("max_generations 2 reached"),
        "stopped at the generation cap"
    );

    // Monotonic non-decreasing frontier pass_rate (gen0 fixes the prompt → 1.0;
    // gen1/gen2 carry it forward).
    let passes: Vec<f64> = report
        .generations
        .iter()
        .map(|g| g.frontier_pass_rate)
        .collect();
    assert_eq!(passes, vec![1.0, 1.0, 1.0], "frontier pass_rate monotonic");
    for w in passes.windows(2) {
        assert!(
            w[1] >= w[0],
            "frontier pass_rate must not decrease: {passes:?}"
        );
    }

    // gen0 frontier is the candidate (not the seed) → config persisted.
    assert!(!report.generations[0].frontier_is_seed);
    assert!(report.generations[0].frontier_config_file.is_some());
    // gen1/gen2 frontier-best == base (carried forward) → no new config.
    assert!(
        report.generations[1].frontier_is_seed,
        "gen1 frontier is the carried-forward base"
    );
    assert!(
        report.generations[2].frontier_is_seed,
        "gen2 frontier is the carried-forward base"
    );

    // lessons.jsonl persisted with one row per generation.
    let lessons_rel = report.lessons_file.as_ref().expect("lessons_file present");
    let lessons_path = report.run_dir.join(lessons_rel);
    assert!(lessons_path.exists(), "lessons.jsonl exists");
    let body = std::fs::read_to_string(&lessons_path).unwrap();
    assert_eq!(body.lines().count(), 3, "one lesson row per generation");
    // Each row carries the generation index + frontier axes.
    for (i, line) in body.lines().enumerate() {
        let entry: serde_json::Value = serde_json::from_str(line).expect("lesson row is JSON");
        assert_eq!(entry["generation"], i, "lesson row {i} generation");
        assert_eq!(
            entry["frontier_pass_rate"], 1.0,
            "lesson row {i} frontier pass"
        );
    }

    // gen0 frontier config round-trips with the patched prompt.
    let gen0_cfg_rel = report.generations[0]
        .frontier_config_file
        .as_ref()
        .expect("gen0 frontier config persisted");
    let gen0_cfg_path = report.run_dir.join(gen0_cfg_rel);
    let body = std::fs::read_to_string(&gen0_cfg_path).unwrap();
    let pack: DomainPackConfig = serde_json::from_str(&body).expect("frontier config parses");
    assert_eq!(
        pack.system_prompt, "Answer with just the number.",
        "gen0 frontier carries the patched prompt"
    );

    // Summary renders the multi-gen table + stop reason + lessons pointer.
    let summary = report.to_summary();
    assert!(summary.contains("generations (3)"), "{summary}");
    assert!(summary.contains("stop:"), "{summary}");
    assert!(summary.contains("lessons:"), "{summary}");
}

#[tokio::test]
async fn e4_recall_top_k_patch_flows_through_loop() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Candidate provider: seed answers wrong (2), patched candidate right (2).
    let candidate_provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("5"),  // seed case 0 (wrong)
        ScriptedResponse::direct_answer("10"), // seed case 1 (wrong)
        ScriptedResponse::direct_answer("4"),  // candidate case 0 (right)
        ScriptedResponse::direct_answer("7"),  // candidate case 1 (right)
    ]);
    let candidate_provider: Arc<dyn LlmProvider> = Arc::new(candidate_provider);

    // Variation provider: one patch bumping recall.top_k to 5.
    let variation_provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(
        recall_top_k_patch_json(),
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

    // Seed with a non-default recall.top_k baseline (default core budget
    // arms memory so recall is a candidate param). Use the coding seed.
    let seed = CandidateConfig::from_pack_config(coding_seed_config("Answer directly."));
    let report = ev.run(&seed, &two_case_suite()).await.expect("evolve run");

    // The candidate (recall.top_k=5) beats the seed → it's the frontier.
    let frontier = report.frontier.as_ref().expect("frontier present");
    assert!(!frontier.is_seed, "frontier is the patched candidate");
    let cfg_rel = frontier
        .config_file
        .as_ref()
        .expect("frontier config persisted");
    let cfg_path = report.run_dir.join(cfg_rel);

    // The patched recall.top_k round-trips through validate + persist.
    let body = std::fs::read_to_string(&cfg_path).unwrap();
    let pack: DomainPackConfig = serde_json::from_str(&body).expect("frontier config parses");
    assert_eq!(
        pack.memory_profile.recall.top_k, 8,
        "recall.top_k patch flowed through to the persisted frontier config"
    );
}
