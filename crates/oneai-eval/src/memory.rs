//! # Memory evaluation harness
//!
//! A self-contained evaluation subsystem for the OneAI memory engine, aligned
//! with the methodology of three authoritative benchmarks:
//!
//! - **LongMemEval** (arXiv:2410.10813, ~427 citations) — 5 long-term memory
//!   abilities: Information Extraction (IE), Multi-session Reasoning (MR),
//!   Temporal Reasoning (TR), Knowledge Updates (KU), Abstention (ABS).
//! - **Mem0** (arXiv:2504.19413) — the F1 + BLEU-1 + LLM-as-Judge scoring
//!   triad that downstream papers report against.
//! - **MemBench** (arXiv:2506.21605) — direct memory-mechanism metrics
//!   (recall accuracy, capacity vs. budget), scoring the store itself, not
//!   just the downstream answer.
//!
//! The harness drives a fresh `MemoryManager` per case (replaying planted
//! multi-session facts, optionally with an `EmbeddingService` for the semantic
//! path), then scores retrieval (Recall@k / NDCG@k against gold evidence keys,
//! no LLM needed) and answer quality (F1 / BLEU-1 lexical, optional
//! LLM-judge). It does NOT run the full AgentLoop — the evaluator's own
//! nondeterminism would otherwise pollute memory-subsystem scores.
//!
//! The builtin synthetic suite covers all 5 abilities, including a deliberate
//! **synonym anti-example**: a fact stored in one language and queried in
//! another, which keyword recall scores 0 and only the §12.1 semantic path can
//! surface. This is the harness's primary anchor for measuring the semantic-
//! recall optimization's effect: run `--no-embedding` (keyword-only baseline)
//! vs. default (semantic) and compare Recall@5.

use serde::{Deserialize, Serialize};

use oneai_core::error::Result;
use oneai_core::MemoryFact;

use crate::eval_metric::{EvalScore, MetricScore};
use crate::eval_result::{EvalReport, EvalResult};

mod case;
mod metrics;
mod suite;
mod runner;

#[cfg(test)]
mod runner_tests;

pub use case::{MemoryAbility, MemoryEvalCase, MemoryEvalSession, PlantedFact};
pub use metrics::{bleu1, f1_partial, ndcg_at_k, recall_at_k};
pub use runner::{DeterministicEmbeddingService, MemoryEvalConfig, MemoryEvalRunner};
pub use suite::{builtin_suite, load_suite_jsonl};

/// One metric computed for a case, with the value + reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMetricOutcome {
    pub metric_name: String,
    pub score: EvalScore,
}

/// Per-case evaluation outcome (internal — folded into an `EvalResult`).
#[derive(Debug, Clone)]
#[allow(dead_code)] // gold_answer/retrieved retained for future report enrichment.
pub(crate) struct CaseOutcome {
    pub(crate) case_id: String,
    pub(crate) ability: MemoryAbility,
    pub(crate) question: String,
    pub(crate) gold_answer: String,
    pub(crate) retrieved: Vec<MemoryFact>,
    pub(crate) answer: String,
    pub(crate) metrics: Vec<MemoryMetricOutcome>,
    pub(crate) duration_ms: u64,
}

impl CaseOutcome {
    /// Fold this outcome into the existing `EvalResult` shape so the memory
    /// harness reuses the standard `EvalReport` (JSON/Markdown exporters,
    /// summary aggregation).
    pub fn into_eval_result(self) -> EvalResult {
        let input = format!("[{:?}] {}", self.ability, self.question);
        let mut r = EvalResult::new(self.case_id, &input, &self.answer);
        r.scores = self.metrics.into_iter()
            .map(|m| MetricScore { metric_name: m.metric_name, score: m.score })
            .collect();
        r.duration_ms = self.duration_ms;
        r
    }
}

/// Run a memory eval suite and produce a standard `EvalReport`.
pub async fn run_memory_eval(
    suite_name: &str,
    cases: &[MemoryEvalCase],
    config: &MemoryEvalConfig,
) -> Result<EvalReport> {
    let runner = MemoryEvalRunner::new(config.clone());
    let mut results = Vec::with_capacity(cases.len());
    for case in cases {
        let outcome = runner.run_case(case).await?;
        results.push(outcome.into_eval_result());
    }
    Ok(EvalReport::new(suite_name, results))
}

// Re-export the manager bits the runner needs from oneai-memory.
pub use oneai_memory::{MemoryManager, MemoryManagerConfig};
