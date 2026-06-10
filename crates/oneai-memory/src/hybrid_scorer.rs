//! Hybrid scorer — combines semantic similarity with temporal proximity.

/// Hybrid scorer for long-term memory retrieval.
///
/// Combines two scoring dimensions:
/// - Semantic similarity (cosine similarity of embeddings)
/// - Temporal proximity (how recently the memory was created)
///
/// Final score = semantic_similarity * α + temporal_proximity * β
pub struct HybridScorer {
    /// Weight for semantic similarity (default: 0.7).
    alpha: f32,
    /// Weight for temporal proximity (default: 0.3).
    beta: f32,
}

impl HybridScorer {
    /// Create a new hybrid scorer with default weights.
    pub fn new() -> Self {
        Self { alpha: 0.7, beta: 0.3 }
    }

    /// Create a hybrid scorer with custom weights.
    pub fn with_weights(alpha: f32, beta: f32) -> Self {
        Self { alpha, beta }
    }

    /// Compute the hybrid score for a memory entry.
    ///
    /// `semantic_score` is cosine similarity (0.0 to 1.0).
    /// `temporal_score` is a normalized recency score (0.0 to 1.0).
    pub fn score(&self, semantic_score: f32, temporal_score: f32) -> f32 {
        self.alpha * semantic_score + self.beta * temporal_score
    }
}

impl Default for HybridScorer {
    fn default() -> Self {
        Self::new()
    }
}