//! E2 e2e: a failing case (model emits a wrong DirectAnswer) →
//! `EvolutionLoop::run` diagnoses it: `report.diagnoses` carries one record
//! whose `suspect_params` contains `PackSystemPrompt`, and the per-case
//! `diagnosis-<id>.json` round-trips to a `Diagnosis` with the same suspect.
//!
//! The seed's non-empty `system_prompt` makes `PackSystemPrompt` a candidate
//! param; its influence set (all `SpanKind::LLM` spans) intersects the failing
//! inference's ancestry → it's a suspect (the non-fallback path — the agent
//! loop instruments an `LLM` span per inference, so the tree isn't empty).
//! Tools have no `TOOL` spans here (no tool was called) → they're not
//! suspects, demonstrating the influence map prunes correctly.

use std::collections::HashMap;
use std::sync::Arc;

use oneai_agent::{mock_provider::ScriptedResponse, MockProvider};
use oneai_core::traits::LlmProvider;
use oneai_domain::{CompressionTemplateConfig, DomainPackConfig, PermissionProfileConfig};
use oneai_eval::{EvalCase, EvalSuiteBuilder, ExactMatchMetric, ExpectedOutput};

use oneai_evolve::{
    AppBaseline, CandidateConfig, EvolutionConfig, EvolutionLoop, ParamRef, SubgraphDiagnostician,
};

fn coding_seed_config() -> DomainPackConfig {
    DomainPackConfig {
        name: "coding_seed".to_string(),
        description: "Coding pack seed for evolve E2 e2e".to_string(),
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
        // A non-empty prompt → PackSystemPrompt is a candidate param.
        system_prompt: "You are a coding agent. Answer directly.".to_string(),
        memory_profile: Default::default(),
    }
}

/// One failing case + one passing case, so the diagnosis pass produces exactly
/// one `DiagnosisRecord` (the passing case is skipped).
fn mixed_suite() -> oneai_eval::EvalSuite {
    let metrics: Vec<Arc<dyn oneai_eval::EvalMetric>> = vec![Arc::new(ExactMatchMetric)];
    EvalSuiteBuilder::new("evolve_e2_diag")
        .description("1-fail + 1-pass suite for E2 diagnosis")
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

#[tokio::test]
async fn e2_diagnoses_failed_case_with_system_prompt_suspect() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Script: case 0 answers wrong (5 instead of 4 → fail); case 1 correct (7).
    // MockProvider advances its shared index across cases in suite order.
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("5"),
        ScriptedResponse::direct_answer("7"),
    ]);
    let provider: Arc<dyn LlmProvider> = Arc::new(provider);

    let baseline = AppBaseline::new(provider, tmp.path().to_string_lossy().to_string());
    let config = EvolutionConfig::new(tmp.path().to_path_buf());
    let ev = EvolutionLoop::new(baseline).with_config(config);

    let seed = CandidateConfig::from_pack_config(coding_seed_config());
    let suite = mixed_suite();
    let report = ev.run(&seed, &suite).await.expect("evolve run");

    // Shape: 2 records, gen 0, 1 pass / 1 fail.
    assert_eq!(report.case_records.len(), 2);
    assert_eq!(report.pass_rate, 0.5, "1 of 2 passes");
    let failed = report
        .case_records
        .iter()
        .find(|c| !c.passed)
        .expect("a failed record exists");
    assert_eq!(failed.case_id, "math_add");

    // Exactly one diagnosis, for the failed case.
    assert_eq!(report.diagnoses.len(), 1, "1 diagnosis for 1 failure");
    let drec = &report.diagnoses[0];
    assert_eq!(drec.case_id, "math_add");
    assert!(
        drec.suspect_params.contains(&ParamRef::PackSystemPrompt),
        "PackSystemPrompt should be a suspect (shaped the failing LLM span), got {:?}",
        drec.suspect_params
    );
    // Tools weren't called → no TOOL spans → not suspects (influence map
    // prunes them; this is the non-fallback path).
    assert!(
        !drec
            .suspect_params
            .contains(&ParamRef::PackTool("calculator".into())),
        "calculator has no TOOL span → should not be a suspect"
    );
    assert!(!drec.critique.is_empty());

    // The full diagnosis (with subtrace) round-trips from disk.
    let diag_rel = drec
        .diagnosis_file
        .as_ref()
        .expect("diagnosis_file set for failed case");
    let diag_path = report.run_dir.join(diag_rel);
    assert!(
        diag_path.exists(),
        "diagnosis file missing: {}",
        diag_path.display()
    );
    let body = std::fs::read_to_string(&diag_path).unwrap();
    let d: oneai_evolve::subgraph::Diagnosis =
        serde_json::from_str(&body).expect("diagnosis json parses");
    assert_eq!(d.case_id, "math_add");
    assert!(d.suspect_params.contains(&ParamRef::PackSystemPrompt));
    assert!(
        !d.subtrace.failure_path.is_empty(),
        "failure path non-empty"
    );
    assert!(!d.subtrace.used_fallback, "non-fallback path expected");

    // The report summary renders the diagnosis.
    assert!(report.to_summary().contains("diagnoses"));
}

#[tokio::test]
async fn e2_all_pass_yields_no_diagnoses() {
    // Sanity: when every case passes, the diagnosis pass is a no-op (no
    // FailedCases). This guards the E1 path against accidental diagnosis
    // noise on green runs.
    let tmp = tempfile::tempdir().expect("tempdir");
    let provider = MockProvider::from_script(vec![
        ScriptedResponse::direct_answer("4"),
        ScriptedResponse::direct_answer("7"),
    ]);
    let provider: Arc<dyn LlmProvider> = Arc::new(provider);
    let baseline = AppBaseline::new(provider, tmp.path().to_string_lossy().to_string());
    let ev =
        EvolutionLoop::new(baseline).with_config(EvolutionConfig::new(tmp.path().to_path_buf()));
    let seed = CandidateConfig::from_pack_config(coding_seed_config());
    let report = ev.run(&seed, &mixed_suite()).await.expect("evolve run");
    assert_eq!(report.pass_rate, 1.0);
    assert!(report.diagnoses.is_empty(), "no failures → no diagnoses");
}

#[tokio::test]
async fn e2_diagnostician_trait_object_overrides_default() {
    // The loop accepts any SubgraphDiagnostician; a custom one (here, a
    // trivial marker that always returns PackSystemPrompt) overrides the
    // default heuristic. This is the seam E5 uses to inject an
    // LlmDiagnostician.
    use oneai_evolve::{subgraph::Diagnosis, FailedCase};
    struct AlwaysPrompt;
    #[async_trait::async_trait]
    impl SubgraphDiagnostician for AlwaysPrompt {
        async fn diagnose(&self, fc: &FailedCase<'_>) -> Diagnosis {
            Diagnosis::new(
                fc.case.id.clone(),
                vec![ParamRef::PackSystemPrompt],
                Default::default(),
                "custom diagnostician",
            )
        }
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer("5")]);
    let provider: Arc<dyn LlmProvider> = Arc::new(provider);
    let baseline = AppBaseline::new(provider, tmp.path().to_string_lossy().to_string());
    let ev = EvolutionLoop::new(baseline)
        .with_config(EvolutionConfig::new(tmp.path().to_path_buf()))
        .with_diagnostician(Arc::new(AlwaysPrompt));
    let seed = CandidateConfig::from_pack_config(coding_seed_config());
    let suite = EvalSuiteBuilder::new("one")
        .case(EvalCase::with_id(
            "m",
            "What is 2+2?",
            ExpectedOutput::exact("4"),
        ))
        .metrics(vec![
            Arc::new(ExactMatchMetric) as Arc<dyn oneai_eval::EvalMetric>
        ])
        .build();
    let report = ev.run(&seed, &suite).await.expect("evolve run");
    assert_eq!(report.diagnoses.len(), 1);
    assert_eq!(report.diagnoses[0].critique, "custom diagnostician");
    assert_eq!(
        report.diagnoses[0].suspect_params,
        vec![ParamRef::PackSystemPrompt]
    );
}
