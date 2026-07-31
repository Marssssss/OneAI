//! Skill selector — lightweight top-K selection with progressive disclosure.

use std::sync::Arc;

use oneai_core::error::Result;
use oneai_core::traits::EmbeddingService;
use oneai_core::{SelectionMode, SkillDescriptor};

/// Lightweight skill selector that dynamically injects relevant skills into context.
///
/// Uses keyword matching or lightweight vector similarity to select top-K skills
/// from the registry. Progressive disclosure: only the most relevant skill
/// descriptions are injected; previous skills auto-unload when topic changes.
///
/// When an [`EmbeddingService`] is attached (`with_embedding_service`), the
/// selector promotes itself to `Hybrid` mode: it embeds the user input once and
/// ranks skills by cosine similarity against each skill's pre-computed
/// `embedding` (or, when absent, the skill's `description` embedded on the fly),
/// blended with the keyword score. Without a service it degrades to pure keyword
/// matching — backward-compatible with call sites that never configured one.
pub struct SkillSelector {
    /// Selection mode (keyword, vector, or hybrid).
    mode: SelectionMode,
    /// Number of top skills to select (default: 3).
    top_k: usize,
    /// Optional embedding service for vector / hybrid selection.
    /// `None` ⇒ keyword-only (the historical behavior).
    embedding_service: Option<Arc<dyn EmbeddingService>>,
}

impl SkillSelector {
    /// Create a new skill selector with default settings (keyword-only).
    pub fn new() -> Self {
        Self {
            mode: SelectionMode::KeywordMatch,
            top_k: 3,
            embedding_service: None,
        }
    }

    /// Create a skill selector with a specific mode and top-K (keyword-only).
    pub fn with_config(mode: SelectionMode, top_k: usize) -> Self {
        Self {
            mode,
            top_k,
            embedding_service: None,
        }
    }

    /// Create a skill selector backed by an embedding service.
    ///
    /// When `service` is `Some`, the selector runs in `Hybrid` mode (keyword +
    /// vector). When `None`, it degrades to keyword-only — so passing the
    /// builder's optional embedding service through here is always safe.
    pub fn with_embedding_service(
        mode: SelectionMode,
        top_k: usize,
        service: Option<Arc<dyn EmbeddingService>>,
    ) -> Self {
        let mode = if service.is_some() && mode != SelectionMode::KeywordMatch {
            mode
        } else if service.is_some() {
            SelectionMode::Hybrid
        } else {
            SelectionMode::KeywordMatch
        };
        Self {
            mode,
            top_k,
            embedding_service: service,
        }
    }

    /// Select the most relevant skills for a user input.
    pub async fn select_skills(
        &self,
        user_input: &str,
        registry: &[SkillDescriptor],
    ) -> Result<Vec<SkillDescriptor>> {
        // Without an embedding service — or in explicit keyword mode — use the
        // historical keyword path. This preserves backward compatibility for
        // every call site that never wired an embedding service.
        let use_vector = matches!(
            self.mode,
            SelectionMode::VectorSimilarity | SelectionMode::Hybrid
        ) && self.embedding_service.is_some();
        if !use_vector {
            return Ok(self.select_keyword(user_input, registry));
        }

        // Vector-capable path: embed the user input once, then score each skill.
        let svc = self.embedding_service.as_ref().expect("checked above");
        let user_emb = svc.embed(user_input).await?;

        let mut scored: Vec<(&SkillDescriptor, f32)> = Vec::with_capacity(registry.len());
        for skill in registry {
            let keyword_score = Self::keyword_score(user_input, skill);

            // Use the skill's pre-computed embedding when available; otherwise
            // embed its description on the fly. On-the-fly embedding lets the
            // selector work before skills have been pre-embedded — at the cost
            // of N embed calls per turn over a typically-small registry.
            let vec_sim = match &skill.embedding {
                Some(e) if !e.is_empty() => cosine_similarity(&user_emb, e),
                _ => {
                    let emb = svc.embed(&skill.description).await?;
                    if emb.is_empty() {
                        0.0
                    } else {
                        cosine_similarity(&user_emb, &emb)
                    }
                }
            };

            let score = match self.mode {
                SelectionMode::VectorSimilarity => vec_sim,
                SelectionMode::Hybrid => 0.5 * keyword_score + 0.5 * vec_sim,
                SelectionMode::KeywordMatch => keyword_score,
            };
            scored.push((skill, score));
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        Ok(scored
            .into_iter()
            .take(self.top_k)
            .filter(|(_, score)| *score > 0.0)
            .map(|(skill, _)| skill.clone())
            .collect())
    }

    /// Keyword-only selection path (historical behavior).
    fn select_keyword(
        &self,
        user_input: &str,
        registry: &[SkillDescriptor],
    ) -> Vec<SkillDescriptor> {
        let mut scored: Vec<(&SkillDescriptor, f32)> = registry
            .iter()
            .map(|skill| (skill, Self::keyword_score(user_input, skill)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(self.top_k)
            .filter(|(_, score)| *score > 0.0)
            .map(|(skill, _)| skill.clone())
            .collect()
    }

    /// Fraction of a skill's trigger keywords present in the user input (0..1).
    fn keyword_score(user_input: &str, skill: &SkillDescriptor) -> f32 {
        skill
            .trigger_keywords
            .iter()
            .map(|kw| {
                if oneai_core::keyword_matches(user_input, kw) {
                    1.0
                } else {
                    0.0
                }
            })
            .sum::<f32>()
            / skill.trigger_keywords.len().max(1) as f32
    }
}

/// Cosine similarity between two vectors, clamped to [0.0, 1.0].
///
/// Returns 0.0 for empty/mismatched-length vectors — skills with no usable
/// embedding are never ranked above a real match by the vector path.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    let cos = dot / (norm_a * norm_b);
    cos.clamp(0.0, 1.0)
}

impl Default for SkillSelector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::SkillDescriptor;

    #[tokio::test]
    async fn test_skill_selector_keyword_matching() {
        let selector = SkillSelector::new();
        let skills = vec![
            SkillDescriptor {
                name: "shell".to_string(),
                description: "Execute shell commands".to_string(),
                prompt_template: "You can use shell.".to_string(),
                trigger_keywords: vec!["shell".to_string(), "command".to_string()],
                embedding: None,
            },
            SkillDescriptor {
                name: "code_review".to_string(),
                description: "Review code".to_string(),
                prompt_template: "You can review code.".to_string(),
                trigger_keywords: vec!["review".to_string(), "code".to_string()],
                embedding: None,
            },
            SkillDescriptor {
                name: "calculator".to_string(),
                description: "Calculate numbers".to_string(),
                prompt_template: "You can calculate.".to_string(),
                trigger_keywords: vec!["calculate".to_string(), "math".to_string()],
                embedding: None,
            },
        ];

        let result = selector
            .select_skills("I need to run a shell command", &skills)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "shell");
    }

    // ─── Deterministic mock embedding service for vector/hybrid tests ─────────
    //
    // Maps text to a 3-axis one-hot-ish vector by domain keyword. Deterministic
    // (no external model), so the assertions below are invariant tests, not
    // change-detector tests (戒律 #6).
    use oneai_core::traits::EmbeddingService;
    use oneai_core::EmbeddingModel;

    struct MockEmbeddingService;

    #[async_trait::async_trait]
    impl EmbeddingService for MockEmbeddingService {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let lower = text.to_lowercase();
            if lower.contains("shell") || lower.contains("command") {
                Ok(vec![1.0, 0.0, 0.0])
            } else if lower.contains("review") || lower.contains("code") {
                Ok(vec![0.0, 1.0, 0.0])
            } else if lower.contains("calc") || lower.contains("math") {
                Ok(vec![0.0, 0.0, 1.0])
            } else {
                Ok(vec![0.1, 0.1, 0.1])
            }
        }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }
        fn model(&self) -> EmbeddingModel {
            EmbeddingModel::new("mock")
        }
    }

    fn sample_registry() -> Vec<SkillDescriptor> {
        vec![
            SkillDescriptor {
                name: "shell".to_string(),
                description: "Execute shell commands".to_string(),
                prompt_template: "You can use shell.".to_string(),
                trigger_keywords: vec!["shell".to_string(), "command".to_string()],
                embedding: None, // embedded on the fly from description
            },
            SkillDescriptor {
                name: "code_review".to_string(),
                description: "Review code".to_string(),
                prompt_template: "You can review code.".to_string(),
                trigger_keywords: vec!["review".to_string(), "code".to_string()],
                embedding: None,
            },
            SkillDescriptor {
                name: "calculator".to_string(),
                description: "Calculate numbers".to_string(),
                prompt_template: "You can calculate.".to_string(),
                trigger_keywords: vec!["calculate".to_string(), "math".to_string()],
                embedding: None,
            },
        ]
    }

    #[tokio::test]
    async fn test_skill_selector_vector_ranks_relevant_first() {
        let selector = SkillSelector::with_embedding_service(
            SelectionMode::VectorSimilarity,
            3,
            Some(Arc::new(MockEmbeddingService)),
        );
        let result = selector
            .select_skills("I need to run a shell command", &sample_registry())
            .await
            .unwrap();
        assert!(!result.is_empty());
        assert_eq!(result[0].name, "shell");
    }

    #[tokio::test]
    async fn test_skill_selector_hybrid_uses_precomputed_embedding() {
        // Skill with a pre-computed embedding matching the query axis.
        let mut reg = sample_registry();
        reg[0].embedding = Some(vec![1.0, 0.0, 0.0]);

        let selector = SkillSelector::with_embedding_service(
            SelectionMode::Hybrid,
            3,
            Some(Arc::new(MockEmbeddingService)),
        );
        let result = selector
            .select_skills("run a shell command", &reg)
            .await
            .unwrap();
        assert!(!result.is_empty());
        assert_eq!(result[0].name, "shell");
    }

    #[tokio::test]
    async fn test_skill_selector_degrades_to_keyword_without_service() {
        // No embedding service ⇒ keyword path, even if mode says Vector.
        let selector =
            SkillSelector::with_embedding_service(SelectionMode::VectorSimilarity, 3, None);
        let result = selector
            .select_skills("I need to run a shell command", &sample_registry())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "shell");
    }

    #[tokio::test]
    async fn test_skill_selector_vector_excludes_zero_similarity() {
        // Query embeds to the "math" axis; shell/review skills embed to other
        // axes → cosine 0 → filtered out by score > 0.
        let selector = SkillSelector::with_embedding_service(
            SelectionMode::VectorSimilarity,
            3,
            Some(Arc::new(MockEmbeddingService)),
        );
        let result = selector
            .select_skills("help with math", &sample_registry())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "calculator");
    }
}
