//! E1 e2e: MockProvider + CodingPack seed + 3-case exact-match suite →
//! `EvolutionLoop::run` produces a report with 3 `CaseRecord`s and 3
//! per-case trajectory files persisted under the run dir.
//!
//! This is the "plumbing" acceptance for E1: the seed hot-loads through
//! `DomainPackSpecFile::validate_and_build`, the `TrajectoryCollector` drives
//! the loop per case and captures `(Trajectory, TraceTree)`, and the report +
//! trajectories round-trip to disk. No diagnosis / variation / Pareto (those
//! are E2–E3).

use std::collections::HashMap;
use std::sync::Arc;

use oneai_agent::{mock_provider::ScriptedResponse, MockProvider};
use oneai_core::traits::LlmProvider;
use oneai_domain::{CompressionTemplateConfig, DomainPackConfig, PermissionProfileConfig};
use oneai_eval::{EvalCase, EvalSuiteBuilder, ExactMatchMetric, ExpectedOutput};

use oneai_evolve::{AppBaseline, CandidateConfig, EvolutionConfig, EvolutionLoop};

/// Minimal valid coding-seed config (mirrors `CodingPack`'s shape: a couple of
/// tools, an allow-list permission profile, a compression template, and the
/// E0 default memory profile). Validates clean through `DomainPackValidator`.
fn coding_seed_config() -> DomainPackConfig {
    DomainPackConfig {
        name: "coding_seed".to_string(),
        description: "Coding pack seed for evolve E1 e2e".to_string(),
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
        system_prompt: "You are a coding agent. Answer directly.".to_string(),
        memory_profile: Default::default(),
    }
}

/// A 3-case exact-match suite: each case is one inference (DirectAnswer), so a
/// MockProvider with 3 scripted responses drives all three deterministically.
fn three_case_suite() -> oneai_eval::EvalSuite {
    let metrics: Vec<Arc<dyn oneai_eval::EvalMetric>> = vec![Arc::new(ExactMatchMetric)];
    EvalSuiteBuilder::new("evolve_e2e")
        .description("3-case exact-match suite for E1 e2e")
        .case(EvalCase::with_id(
            "math_add",
            "What is 2+2?",
            ExpectedOutput::exact("4"),
        ))
        .case(EvalCase::with_id(
            "math_multiply",
            "What is 3*5?",
            ExpectedOutput::exact("15"),
        ))
        .case(EvalCase::with_id(
            "math_subtract",
            "What is 10-3?",
            ExpectedOutput::exact("7"),
        ))
        .metrics(metrics)
        .build()
}

#[tokio::test]
async fn e1_run_produces_report_and_trajectories() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // 3 scripted DirectAnswers, one per case (MockProvider's shared index
    // advances across cases).
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("4"),
        ScriptedResponse::direct_answer("15"),
        ScriptedResponse::direct_answer("7"),
    ]);
    let provider: Arc<dyn LlmProvider> = Arc::new(provider);

    let baseline = AppBaseline::new(provider, tmp.path().to_string_lossy().to_string());
    let config = EvolutionConfig::new(tmp.path().to_path_buf());
    let ev = EvolutionLoop::new(baseline).with_config(config);

    let seed = CandidateConfig::from_pack_config(coding_seed_config());
    let suite = three_case_suite();
    let report = ev.run(&seed, &suite).await.expect("evolve run");

    // Report shape: 3 records, generation 0, no_optimize, pass_rate 1.0.
    assert_eq!(report.generation, 0);
    assert!(report.no_optimize);
    assert_eq!(report.case_records.len(), 3, "3 case records expected");
    assert_eq!(
        report.pass_rate, 1.0,
        "all 3 should pass (scripted exact answers), got {report:?}"
    );

    // Each record carries a trajectory-file pointer whose target exists and
    // holds a 1-response trajectory with ≥1 iteration.
    for rec in &report.case_records {
        let traj_rel = rec
            .trajectory_file
            .as_ref()
            .unwrap_or_else(|| panic!("no trajectory_file for {}", rec.case_id));
        let traj_path = report.run_dir.join(traj_rel);
        assert!(
            traj_path.exists(),
            "trajectory file missing: {}",
            traj_path.display()
        );

        let body = std::fs::read_to_string(&traj_path).unwrap();
        let traj: oneai_eval::Trajectory =
            serde_json::from_str(body.trim()).expect("trajectory json parses");
        assert_eq!(
            traj.responses.len(),
            1,
            "{} should have 1 recorded response",
            rec.case_id
        );
        assert!(
            traj.recorded_iterations >= 1,
            "{} should have ≥1 iteration",
            rec.case_id
        );
        assert!(rec.passed, "{} should pass", rec.case_id);
    }

    // Aggregate report.json persisted.
    let report_path = report.run_dir.join("report.json");
    assert!(report_path.exists(), "report.json missing");
    let rb = std::fs::read_to_string(&report_path).unwrap();
    let r2: oneai_evolve::EvolutionReport = serde_json::from_str(&rb).expect("report round-trips");
    assert_eq!(r2.case_records.len(), 3);
    assert_eq!(r2.pass_rate, 1.0);

    // Token accounting: each script carries 150 tokens (100+50), 3 cases → 450.
    assert_eq!(report.total_tokens, 450);
}
