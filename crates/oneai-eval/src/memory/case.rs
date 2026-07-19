//! Memory eval case + ability taxonomy (LongMemEval 5-ability aligned).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The 5 long-term memory abilities evaluated (LongMemEval taxonomy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemoryAbility {
    /// Recall a fact stated in a session (single-session extraction).
    InformationExtraction,
    /// Combine facts across multiple sessions.
    MultiSessionReasoning,
    /// Answer requiring timestamp awareness ("last weekend", "before X").
    TemporalReasoning,
    /// The answer changed over time — return the *current* value (exercises
    /// §12.2 supersede / soft-invalidate).
    KnowledgeUpdate,
    /// Refuse an unanswerable question rather than hallucinate.
    Abstention,
}

impl MemoryAbility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InformationExtraction => "information_extraction",
            Self::MultiSessionReasoning => "multi_session_reasoning",
            Self::TemporalReasoning => "temporal_reasoning",
            Self::KnowledgeUpdate => "knowledge_update",
            Self::Abstention => "abstention",
        }
    }
}

/// A fact planted into a session's memory during replay, with a known
/// `(subject, predicate)` key so Recall@k / NDCG@k can be scored against it
/// without an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlantedFact {
    pub fact_type: String,
    pub subject: String,
    pub predicate: String,
    pub content: String,
    #[serde(default = "default_importance")]
    pub importance: f32,
}

fn default_importance() -> f32 { 0.7 }

impl PlantedFact {
    /// Build a `MemoryFact` namespaced to the given user/session.
    pub fn to_memory_fact(&self, user_id: &str, session_id: &str) -> oneai_core::MemoryFact {
        use oneai_core::FactType;
        oneai_core::MemoryFact {
            id: format!("{}_{}_{}", user_id, self.subject, self.predicate),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            fact_type: FactType::new(self.fact_type.clone()),
            subject: self.subject.clone(),
            predicate: self.predicate.clone(),
            content: self.content.clone(),
            embedding: None, // embedded by archive_facts (§12.1).
            metadata: HashMap::new(),
            importance: self.importance,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
            superseded: false,
            superseded_at: None,
            pinned: false,
        }
    }

    /// The evidence key for Recall@k scoring: `(subject, predicate)`.
    pub fn evidence_key(&self) -> (String, String) {
        (self.subject.clone(), self.predicate.clone())
    }
}

/// A replayed conversation session with its planted facts and timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvalSession {
    pub id: String,
    /// When this session occurred (for temporal-reasoning cases). RFC3339.
    pub at: String,
    /// Facts the agent would have archived from this session.
    #[serde(default)]
    pub facts: Vec<PlantedFact>,
    /// The conversation turns (role, text) — for the LLM-judge prompt context.
    #[serde(default)]
    pub messages: Vec<(String, String)>,
}

/// A single memory-eval case (LongMemEval-style).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvalCase {
    pub id: String,
    pub ability: MemoryAbility,
    /// Free-form category tag (e.g. "single_session_user", "knowledge_update").
    pub category: String,
    /// The user's question to recall an answer for.
    pub question: String,
    /// The reference answer (or "I don't know" for abstention cases).
    pub gold_answer: String,
    /// The `(subject, predicate)` keys whose facts must be retrieved for the
    /// question to be answerable. Drives Recall@k / NDCG@k. Empty for ABS.
    #[serde(default)]
    pub evidence_keys: Vec<(String, String)>,
    /// The multi-session history to replay before asking the question.
    pub sessions: Vec<MemoryEvalSession>,
    /// Whether the correct behavior is to abstain (refuse). For ABS cases.
    #[serde(default)]
    pub requires_abstention: bool,
    /// Whether this case deliberately uses a synonym / cross-language query
    /// that keyword recall cannot match — the §12.1 semantic-recall anchor.
    #[serde(default)]
    pub synonym_anti_example: bool,
    /// Soft-invalidate instructions: `(session_index, fact_index)` pairs in
    /// the replay whose fact should be invalidated (KU cases — the old value
    /// must NOT be returned). Drives §12.2 verification.
    #[serde(default)]
    pub invalidate_after: Vec<(usize, usize)>,
}
