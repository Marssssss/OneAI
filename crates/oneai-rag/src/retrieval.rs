//! RAG document retrieval — query processing and result ranking.
//!
//! The retrieval module handles:
//! - Processing retrieval queries (keyword and vector)
//! - Ranking and filtering results
//! - Reranking results for better relevance
//! - Assembling retrieval results for injection into LLM context

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::document::Chunk;

/// A retrieval query — describes what to search for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalQuery {
    /// The text query.
    pub text: String,

    /// Optional pre-computed embedding for vector search.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,

    /// Maximum number of results to return.
    #[serde(default = "default_top_k")]
    pub top_k: usize,

    /// Metadata filters (e.g., source, author).
    #[serde(default)]
    pub metadata_filters: HashMap<String, String>,
}

fn default_top_k() -> usize {
    5
}

impl RetrievalQuery {
    /// Create a keyword-based retrieval query.
    pub fn keyword(text: impl Into<String>, top_k: usize) -> Self {
        Self {
            text: text.into(),
            embedding: None,
            top_k,
            metadata_filters: HashMap::new(),
        }
    }

    /// Create a vector-based retrieval query.
    pub fn vector(text: impl Into<String>, embedding: Vec<f32>, top_k: usize) -> Self {
        Self {
            text: text.into(),
            embedding: Some(embedding),
            top_k,
            metadata_filters: HashMap::new(),
        }
    }

    /// Create a hybrid retrieval query (both keyword and vector).
    pub fn hybrid(text: impl Into<String>, embedding: Vec<f32>, top_k: usize) -> Self {
        Self {
            text: text.into(),
            embedding: Some(embedding),
            top_k,
            metadata_filters: HashMap::new(),
        }
    }

    /// Add a metadata filter.
    pub fn with_filter(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata_filters.insert(key.into(), value.into());
        self
    }
}

/// A retrieval result — a chunk with its relevance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalResult {
    /// The retrieved chunk.
    pub chunk: Chunk,

    /// The relevance score (0.0 to 1.0 for vector search, variable for keyword).
    pub score: f32,

    /// The embedding of this chunk (if available).
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
}

impl RetrievalResult {
    /// Create a new retrieval result.
    pub fn new(chunk: Chunk, score: f32) -> Self {
        Self {
            chunk,
            score,
            embedding: None,
        }
    }
}

/// Reranking strategy for retrieval results.
///
/// After initial retrieval, results can be reranked to improve relevance:
/// - `ScoreOnly`: Use the initial score only (no reranking)
/// - `CrossEncoder`: Use a cross-encoder model for reranking (requires LLM)
/// - `RecencyWeighted`: Weight results by recency (timestamp-based)
/// - `Diversity`: Ensure diverse results (different documents/sources)
#[derive(Debug, Clone)]
pub enum RerankingStrategy {
    /// No reranking — use initial scores.
    ScoreOnly,

    /// Weight results by recency (timestamp metadata).
    RecencyWeighted {
        /// Weight for recency (0.0 = ignore recency, 1.0 = only recency).
        recency_weight: f32,
    },

    /// Ensure diversity across documents (avoid too many chunks from same doc).
    Diversity {
        /// Maximum chunks per document.
        max_per_document: usize,
    },
}

impl Default for RerankingStrategy {
    fn default() -> Self {
        Self::ScoreOnly
    }
}

/// Rerank retrieval results according to a strategy.
pub fn rerank(results: &mut Vec<RetrievalResult>, strategy: &RerankingStrategy) {
    match strategy {
        RerankingStrategy::ScoreOnly => {
            // Already sorted by score — no reranking needed
        }
        RerankingStrategy::RecencyWeighted { recency_weight } => {
            // Weight results by recency
            let now = chrono::Utc::now();
            for result in results.iter_mut() {
                // Use timestamp metadata for recency scoring
                let recency_score = result.chunk.metadata.get("timestamp")
                    .and_then(|ts| ts.parse::<i64>().ok())
                    .map(|ts| {
                        let age_seconds = now.timestamp() - ts;
                        let half_life: f64 = 3600.0;
                        0.5_f64.powf(age_seconds as f64 / half_life) as f32
                    })
                    .unwrap_or(0.5); // Default recency if no timestamp

                let score_weight = 1.0 - *recency_weight;
                result.score = score_weight * result.score + *recency_weight * recency_score;
            }
            // Re-sort by adjusted score
            results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        }
        RerankingStrategy::Diversity { max_per_document } => {
            // Limit chunks per document
            let mut doc_counts: HashMap<String, usize> = HashMap::new();
            results.retain(|result| {
                let count = doc_counts.entry(result.chunk.document_id.clone()).or_insert(0);
                if *count < *max_per_document {
                    *count += 1;
                    true
                } else {
                    false
                }
            });
        }
    }
}

/// Assemble retrieval results into a context string for LLM injection.
///
/// Formats results as numbered entries with source information,
/// suitable for injecting into an LLM context window.
pub fn assemble_context(results: &[RetrievalResult], max_tokens: usize) -> String {
    let mut context = String::from("[Retrieved context]:\n");
    let mut estimated_tokens = 0;

    for (i, result) in results.iter().enumerate() {
        let entry = format!(
            "{}. [Source: {}] {}\n",
            i + 1,
            result.chunk.document_id,
            result.chunk.content
        );
        let entry_tokens = entry.len() / 4 + 20; // Rough estimate

        if estimated_tokens + entry_tokens > max_tokens {
            break;
        }

        context.push_str(&entry);
        estimated_tokens += entry_tokens;
    }

    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Chunk;

    fn make_chunk(doc_id: &str, content: &str) -> Chunk {
        Chunk::new(doc_id, content, 0, content.len())
    }

    #[test]
    fn test_retrieval_query_creation() {
        let keyword_query = RetrievalQuery::keyword("rust programming", 5);
        assert_eq!(keyword_query.text, "rust programming");
        assert!(keyword_query.embedding.is_none());
        assert_eq!(keyword_query.top_k, 5);

        let vector_query = RetrievalQuery::vector("rust", vec![0.1, 0.2], 3);
        assert!(vector_query.embedding.is_some());
        assert_eq!(vector_query.top_k, 3);

        let hybrid_query = RetrievalQuery::hybrid("rust", vec![0.1, 0.2], 10);
        assert!(hybrid_query.embedding.is_some());
    }

    #[test]
    fn test_retrieval_query_with_filter() {
        let query = RetrievalQuery::keyword("test", 5)
            .with_filter("source", "wikipedia");
        assert_eq!(query.metadata_filters.get("source"), Some(&"wikipedia".to_string()));
    }

    #[test]
    fn test_rerank_score_only() {
        let mut results = vec![
            RetrievalResult::new(make_chunk("1", "First"), 0.5),
            RetrievalResult::new(make_chunk("2", "Second"), 0.8),
        ];
        rerank(&mut results, &RerankingStrategy::ScoreOnly);
        // ScoreOnly doesn't change order (already sorted by score)
        assert_eq!(results[0].score, 0.5);
        assert_eq!(results[1].score, 0.8);
    }

    #[test]
    fn test_rerank_diversity() {
        let mut results = vec![
            RetrievalResult::new(make_chunk("doc1", "First from doc1"), 0.9),
            RetrievalResult::new(make_chunk("doc1", "Second from doc1"), 0.8),
            RetrievalResult::new(make_chunk("doc1", "Third from doc1"), 0.7),
            RetrievalResult::new(make_chunk("doc2", "First from doc2"), 0.6),
        ];
        rerank(&mut results, &RerankingStrategy::Diversity { max_per_document: 2 });
        // Only 2 chunks per document should remain
        let doc1_count = results.iter().filter(|r| r.chunk.document_id == "doc1").count();
        let doc2_count = results.iter().filter(|r| r.chunk.document_id == "doc2").count();
        assert_eq!(doc1_count, 2);
        assert_eq!(doc2_count, 1);
    }

    #[test]
    fn test_assemble_context() {
        let results = vec![
            RetrievalResult::new(make_chunk("doc1", "Rust is fast"), 0.9),
            RetrievalResult::new(make_chunk("doc2", "Python is easy"), 0.7),
        ];
        let context = assemble_context(&results, 200);
        assert!(context.contains("[Retrieved context]"));
        assert!(context.contains("Rust is fast"));
        assert!(context.contains("Python is easy"));
    }

    #[test]
    fn test_assemble_context_token_limit() {
        let results = vec![
            RetrievalResult::new(make_chunk("doc1", "A very long piece of text that takes many tokens"), 0.9),
            RetrievalResult::new(make_chunk("doc2", "Another long text"), 0.7),
        ];
        // Very low token limit — should truncate
        let context = assemble_context(&results, 10);
        // Should not contain the second result
        assert!(!context.contains("Another long text"));
    }

    #[test]
    fn test_retrieval_result_creation() {
        let chunk = make_chunk("doc1", "Test content");
        let result = RetrievalResult::new(chunk, 0.95);
        assert_eq!(result.chunk.document_id, "doc1");
        assert_eq!(result.score, 0.95);
        assert!(result.embedding.is_none());
    }
}