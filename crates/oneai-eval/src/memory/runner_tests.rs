//! End-to-end tests for the memory eval harness. The headline test compares
//! `--no-embedding` (keyword baseline) against the semantic path on the
//! builtin suite's synonym anti-example — proving the harness surfaces the
//! §12.1 gain.

use std::sync::Arc;

use oneai_core::traits::EmbeddingService;

use super::case::MemoryAbility;
use super::suite::builtin_suite;
use super::{run_memory_eval, MemoryEvalConfig};

/// Deterministic embedding service mirroring the manager.rs §12.1 test:
/// byte-histogram vectors, L2-normalized — enough to make a Chinese fact
/// and an English query share enough bytes to be cos-similar, while unrelated
/// text differs.
struct HashEmbeddingService;
#[async_trait::async_trait]
impl EmbeddingService for HashEmbeddingService {
    async fn embed(&self, text: &str) -> oneai_core::error::Result<Vec<f32>> {
        let mut v = vec![0.0f32; 32];
        for (i, b) in text.bytes().enumerate() {
            v[(b as usize) % 32] += 1.0;
            v[((b as usize).wrapping_add(i)) % 32] += 0.5;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        for x in v.iter_mut() { *x /= norm; }
        Ok(v)
    }
    async fn embed_batch(&self, texts: &[String]) -> oneai_core::error::Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts { out.push(self.embed(t).await?); }
        Ok(out)
    }
    fn model(&self) -> oneai_core::traits::EmbeddingModel {
        oneai_core::traits::EmbeddingModel::allminilm_l6_v2()
    }
}

#[tokio::test]
async fn builtin_suite_runs_and_produces_report() {
    let cases = builtin_suite();
    assert!(cases.len() >= 8);
    // No-embedding baseline — must still produce a report (keyword-only).
    let report = run_memory_eval("builtin_no_emb", &cases, &MemoryEvalConfig::no_embedding())
        .await
        .unwrap();
    assert_eq!(report.results.len(), cases.len());
    assert!(report.results.iter().any(|r| r.case_id == "ie_synonym_cross_lang"));
}

#[tokio::test]
async fn semantic_path_beats_keyword_on_synonym_anti_example() {
    // §12.1 anchor: the synonym anti-example (Chinese fact, English query)
    // has zero keyword overlap → keyword baseline Recall@5 = 0, while the
    // semantic path (with the deterministic embedding) must surface it.
    let cases: Vec<_> = builtin_suite().into_iter()
        .filter(|c| c.id == "ie_synonym_cross_lang")
        .collect();
    assert_eq!(cases.len(), 1);

    let kw_report = run_memory_eval("kw", &cases, &MemoryEvalConfig::no_embedding())
        .await.unwrap();
    let kw_recall = kw_report.results[0].scores.iter()
        .find(|s| s.metric_name == "recall_at_k").unwrap().score.value;
    assert!((kw_recall - 0.0).abs() < 1e-9, "keyword recall must miss the synonym fact");

    let sem_cfg = MemoryEvalConfig::with_embedding(Arc::new(HashEmbeddingService));
    let sem_report = run_memory_eval("sem", &cases, &sem_cfg).await.unwrap();
    let sem_recall = sem_report.results[0].scores.iter()
        .find(|s| s.metric_name == "recall_at_k").unwrap().score.value;
    assert!(sem_recall > 0.0, "semantic recall must surface the synonym fact");
}

#[tokio::test]
async fn knowledge_update_case_returns_current_value() {
    // §12.2 anchor: KU case invalidates the old auth value; recall must
    // return "session" (the current truth), and the old "JWT" must not be
    // the synthesized answer.
    let cases: Vec<_> = builtin_suite().into_iter()
        .filter(|c| c.id == "ku_auth_switch")
        .collect();
    let cfg = MemoryEvalConfig::with_embedding(Arc::new(HashEmbeddingService));
    let report = run_memory_eval("ku", &cases, &cfg).await.unwrap();
    let answer = &report.results[0].actual_output;
    assert!(answer.contains("session"), "current value must be recalled; got: {}", answer);
    assert!(!answer.to_lowercase().starts_with("jwt"), "old superseded value must not lead");
}

#[tokio::test]
async fn abstention_cases_score_correctly() {
    // ABS cases: no relevant facts → abstention metric = 1.0.
    let cases: Vec<_> = builtin_suite().into_iter()
        .filter(|c| c.ability == MemoryAbility::Abstention)
        .collect();
    assert!(cases.len() >= 2);
    let report = run_memory_eval("abs", &cases, &MemoryEvalConfig::no_embedding())
        .await.unwrap();
    for r in &report.results {
        let abst = r.scores.iter().find(|s| s.metric_name == "abstention").unwrap();
        assert!((abst.score.value - 1.0).abs() < 1e-9, "case {} should abstain", r.case_id);
    }
}
