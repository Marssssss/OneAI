//! Memory eval runner — replays a case's sessions through a fresh
//! `MemoryManager`, recalls for the question, and scores.

use std::sync::Arc;
use std::time::Instant;

use oneai_core::error::Result;
use oneai_core::traits::{EmbeddingService, LlmProvider};
use oneai_core::RecallConfig;

use oneai_memory::{MemoryManager, MemoryManagerConfig};

use super::case::MemoryEvalCase;
use super::metrics::{bleu1, f1_partial, ndcg_at_k, recall_at_k, score01};
use super::CaseOutcome;

/// Runner configuration: which services are available (semantic recall needs
/// an embedding service; LLM-judge needs a provider), plus the recall config
/// and the set of metrics to compute.
#[derive(Clone)]
pub struct MemoryEvalConfig {
    /// Embedding service (None → keyword-only baseline, the §12.1 control).
    pub embedding_service: Option<Arc<dyn EmbeddingService>>,
    /// LLM judge provider (None → skip the LLM-judge metric).
    pub llm_judge: Option<Arc<dyn LlmProvider>>,
    /// Recall config (weights / half-life / normalization / top_k).
    pub recall_config: RecallConfig,
    /// k for Recall@k / NDCG@k.
    pub k: usize,
    /// Whether to compute the LLM-judge metric (requires `llm_judge`).
    pub judge: bool,
}

impl Default for MemoryEvalConfig {
    fn default() -> Self {
        Self {
            embedding_service: None,
            llm_judge: None,
            recall_config: RecallConfig::default(),
            k: 5,
            judge: false,
        }
    }
}

impl MemoryEvalConfig {
    /// Keyword-only baseline (no embedding service) — the §12.1 control.
    pub fn no_embedding() -> Self {
        Self { embedding_service: None, ..Self::default() }
    }
    /// Semantic recall enabled (with an embedding service).
    pub fn with_embedding(svc: Arc<dyn EmbeddingService>) -> Self {
        Self { embedding_service: Some(svc), ..Self::default() }
    }
    /// Semantic recall via the bundled deterministic embedding service
    /// (offline, no API key) — a stand-in for a real embedding model that
    /// keeps the CLI demo runnable in CI. Real quality measurement should
    /// substitute an OpenAI/Ollama embedding service.
    pub fn with_deterministic_embedding() -> Self {
        Self::with_embedding(std::sync::Arc::new(DeterministicEmbeddingService))
    }
}

/// A deterministic, offline embedding service for demos / CI. Maps text to a
/// 32-dim byte-histogram vector (L2-normalized). It is NOT a meaningful
/// embedding model — it only makes the semantic-recall path exercisable
/// without network access. Shared bytes between query and fact (e.g. CJK +
/// English that overlap on ASCII tokens like "pnpm") produce non-zero cosine,
/// enough to surface synonym facts keyword recall misses.
pub struct DeterministicEmbeddingService;

#[async_trait::async_trait]
impl EmbeddingService for DeterministicEmbeddingService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0.0f32; 32];
        for (i, b) in text.bytes().enumerate() {
            v[(b as usize) % 32] += 1.0;
            v[((b as usize).wrapping_add(i)) % 32] += 0.5;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        for x in v.iter_mut() { *x /= norm; }
        Ok(v)
    }
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts { out.push(self.embed(t).await?); }
        Ok(out)
    }
    fn model(&self) -> oneai_core::traits::EmbeddingModel {
        oneai_core::traits::EmbeddingModel::allminilm_l6_v2()
    }
}

pub struct MemoryEvalRunner {
    config: MemoryEvalConfig,
}

impl MemoryEvalRunner {
    pub fn new(config: MemoryEvalConfig) -> Self {
        Self { config }
    }

    fn build_manager(&self) -> MemoryManager {
        let mut mm = MemoryManager::with_config(MemoryManagerConfig::default());
        if let Some(svc) = &self.config.embedding_service {
            mm.set_embedding_service(svc.clone());
        }
        mm
    }

    /// Replay one case and score it.
    pub(crate) async fn run_case(&self, case: &MemoryEvalCase) -> Result<CaseOutcome> {
        let start = Instant::now();
        let mm = self.build_manager();
        // Stable eval user id so facts namespace deterministically.
        mm.set_user_id("eval_user").await;

        // Replay sessions in order: archive each session's planted facts.
        // Respect `invalidate_after` (KU cases) — after planting a session's
        // facts, invalidate the listed (session_index, fact_index) facts so
        // the OLD value is soft-failed and recall returns the new one.
        for (sidx, session) in case.sessions.iter().enumerate() {
            mm.set_session_id(session.id.clone()).await;
            let facts: Vec<_> = session.facts.iter()
                .map(|pf| pf.to_memory_fact("eval_user", &session.id))
                .collect();
            mm.archive_facts(facts).await;
            for (s, fidx) in &case.invalidate_after {
                if *s == sidx {
                    if let Some(pf) = session.facts.get(*fidx) {
                        mm.invalidate_fact("eval_user", &pf.subject, &pf.predicate).await;
                    }
                }
            }
        }

        // Recall for the question.
        let mut cfg = self.config.recall_config.clone();
        cfg.top_k = self.config.k;
        let retrieved = mm.recall_facts_with_config(&case.question, &cfg).await?;

        let retrieved_keys: Vec<(String, String)> = retrieved.iter()
            .map(|f| (f.subject.clone(), f.predicate.clone()))
            .collect();

        // Build a candidate answer from the retrieved facts (the eval
        // measures the memory subsystem, not the LLM's prose — so we
        // synthesize a deterministic answer from retrieved content).
        let answer = synthesize_answer(case, &retrieved);

        // Score retrieval (no LLM).
        let mut metrics = vec![
            score01("recall_at_k", recall_at_k(&retrieved_keys, &case.evidence_keys, self.config.k),
                format!("recall@{} of gold evidence keys", self.config.k)),
            score01("ndcg_at_k", ndcg_at_k(&retrieved_keys, &case.evidence_keys, self.config.k),
                format!("ndcg@{} vs gold evidence", self.config.k)),
        ];

        // Score answer quality (lexical).
        if case.requires_abstention {
            // Abstention: the correct behavior is to refuse. A robust
            // abstention signal is "no retrieved fact's *content* (the actual
            // answer text, not the structural subject/predicate like
            // `user.package_manager`) matches a substantive query token" — a
            // tokenized keyword matcher will weakly match subjects like
            // `user.*` via the word "user" in many questions, which is NOT a
            // substantive hit and must not count as a hallucinated recall.
            let abstained = retrieved.iter().all(|f| {
                !oneai_core::keyword_matches_any_token(&f.content, &case.question)
            });
            let reason = if abstained { "correctly abstained (no substantive content recall)".to_string() } else { "hallucinated content recall".to_string() };
            metrics.push(score01("abstention", if abstained { 1.0 } else { 0.0 }, reason));
        } else {
            metrics.push(score01("f1", f1_partial(&answer, &case.gold_answer), "token-level F1 vs gold"));
            metrics.push(score01("bleu1", bleu1(&answer, &case.gold_answer), "unigram precision vs gold"));
        }

        // Optional LLM-judge.
        if self.config.judge {
            if let Some(provider) = &self.config.llm_judge {
                let score = llm_judge(provider, case, &answer).await.unwrap_or(0.0);
                metrics.push(score01("llm_judge", score, "LLM-as-judge correctness"));
            }
        }

        Ok(CaseOutcome {
            case_id: case.id.clone(),
            ability: case.ability,
            question: case.question.clone(),
            gold_answer: case.gold_answer.clone(),
            retrieved,
            answer,
            metrics,
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
}

/// Synthesize a deterministic answer from recalled facts (and abstain when
/// nothing relevant was recalled). This isolates memory-subsystem quality from
/// LLM prose variability — the eval measures recall, not generation.
fn synthesize_answer(case: &MemoryEvalCase, retrieved: &[oneai_core::MemoryFact]) -> String {
    if case.requires_abstention {
        if retrieved.is_empty() {
            return "I don't know".to_string();
        }
        // Retrieved something for an abstention case → treat as hallucination.
        return retrieved.iter()
            .map(|f| f.content.clone())
            .collect::<Vec<_>>()
            .join(", ");
    }
    if retrieved.is_empty() {
        return String::new();
    }
    retrieved.iter()
        .map(|f| f.content.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse the first f64 in the response.
async fn llm_judge(
    provider: &Arc<dyn LlmProvider>,
    case: &MemoryEvalCase,
    answer: &str,
) -> Result<f64> {
    use oneai_core::{Conversation, InferenceRequest, Message};
    let prompt = format!(
        "You are an evaluator. Score the candidate answer's correctness for the question on a \
        0.0–1.0 scale. Output ONLY a single decimal number. Question: {}\nGold answer: {}\n\
        Candidate answer: {}",
        case.question, case.gold_answer, answer
    );
    let mut conv = Conversation::new();
    conv.add_message(Message::system(prompt));
    let req = InferenceRequest {
        conversation: conv,
        tools: vec![],
        max_tokens: Some(16),
        temperature: Some(0.0),
        top_p: None,
        stop_sequences: vec![],
        constrained_output: None,
        thinking_budget: None,
        metadata: std::collections::HashMap::new(),
    };
    let resp = provider.infer(req).await?;
    let text = resp.message.text_content();
    // Parse the first f64 in the response.
    Ok(text.split_whitespace()
        .find_map(|t| t.trim_end_matches(',').parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(0.0))
}
