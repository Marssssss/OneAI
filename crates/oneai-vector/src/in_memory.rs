//! In-memory brute-force cosine `VectorBackend`.
//!
//! No external dependencies and always available (not behind a feature flag).
//! Intended for tests and tiny document sets — O(n·d) per query is fine up to
//! ~10⁴ vectors; beyond that switch to [`SqliteVecBackend`](crate::SqliteVecBackend)
//! (exact KNN) or [`UsearchBackend`](crate::UsearchBackend) (HNSW ANN).

use std::collections::HashMap;

use async_trait::async_trait;
use oneai_core::traits::{Filter, Metadata, VectorBackend, VectorHit};
use tokio::sync::Mutex;

use crate::cosine;

/// A brute-force cosine vector backend backed by a `Vec` guarded by a mutex.
pub struct InMemoryVectorBackend {
    dim: usize,
    rows: Mutex<Vec<(String, Vec<f32>, Metadata)>>,
    by_id: Mutex<HashMap<String, usize>>,
}

impl InMemoryVectorBackend {
    /// Create a backend that only accepts vectors of `dim` dimensions.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            rows: Mutex::new(Vec::new()),
            by_id: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl VectorBackend for InMemoryVectorBackend {
    async fn upsert(&self, id: &str, embedding: &[f32], metadata: Metadata) -> oneai_core::Result<()> {
        if embedding.len() != self.dim {
            return Err(oneai_core::OneAIError::Rag(format!(
                "InMemoryVectorBackend: dim mismatch (got {}, expected {})",
                embedding.len(),
                self.dim
            )));
        }
        let mut rows = self.rows.lock().await;
        let mut by_id = self.by_id.lock().await;
        if let Some(&idx) = by_id.get(id) {
            rows[idx].1 = embedding.to_vec();
            rows[idx].2 = metadata;
        } else {
            by_id.insert(id.to_string(), rows.len());
            rows.push((id.to_string(), embedding.to_vec(), metadata));
        }
        Ok(())
    }

    async fn search(
        &self,
        query: &[f32],
        top_k: usize,
        filter: Option<&Filter>,
    ) -> oneai_core::Result<Vec<VectorHit>> {
        if query.len() != self.dim {
            return Err(oneai_core::OneAIError::Rag(format!(
                "InMemoryVectorBackend: query dim mismatch (got {}, expected {})",
                query.len(),
                self.dim
            )));
        }
        let rows = self.rows.lock().await;
        let mut scored: Vec<VectorHit> = rows
            .iter()
            .filter(|(_, _, meta)| filter.map(|f| f.matches(meta)).unwrap_or(true))
            .map(|(id, emb, meta)| VectorHit {
                id: id.clone(),
                score: cosine(query, emb),
                metadata: meta.clone(),
            })
            .collect();
        // Descending by score.
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        Ok(scored)
    }

    async fn delete(&self, id: &str) -> oneai_core::Result<()> {
        let mut by_id = self.by_id.lock().await;
        if let Some(idx) = by_id.remove(id) {
            let mut rows = self.rows.lock().await;
            // Swap-remove; rebuild id→index map for the moved element.
            let is_last = idx == rows.len() - 1;
            rows.swap_remove(idx);
            if !is_last {
                // The element that moved into `idx` needs its index fixed.
                let moved_id = rows[idx].0.clone();
                by_id.insert(moved_id, idx);
            }
        }
        Ok(())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upsert_search_filter_delete() {
        let be = InMemoryVectorBackend::new(4);
        be.upsert("a", &[0.1, 0.2, 0.3, 0.4], Metadata::from([("k".into(), "x".into())]))
            .await
            .unwrap();
        be.upsert("b", &[0.11, 0.21, 0.31, 0.41], Metadata::from([("k".into(), "y".into())]))
            .await
            .unwrap();
        be.upsert("c", &[0.9, 0.8, 0.7, 0.6], Metadata::from([("k".into(), "x".into())]))
            .await
            .unwrap();

        // Nearest to a is b, then c.
        let hits = be.search(&[0.1, 0.2, 0.3, 0.4], 3, None).await.unwrap();
        assert_eq!(hits[0].id, "a");
        assert_eq!(hits[1].id, "b");
        assert!(hits[0].score > 0.99);

        // Filter k==x → only a and c.
        let f = Filter::new().with_eq("k", "x");
        let hits = be.search(&[0.1, 0.2, 0.3, 0.4], 5, Some(&f)).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.metadata["k"] == "x"));

        // Upsert replaces.
        be.upsert("a", &[0.9, 0.8, 0.7, 0.6], Metadata::new()).await.unwrap();
        let hits = be.search(&[0.9, 0.8, 0.7, 0.6], 2, None).await.unwrap();
        assert!(hits.iter().any(|h| h.id == "a"));

        be.delete("b").await.unwrap();
        let hits = be.search(&[0.11, 0.21, 0.31, 0.41], 10, None).await.unwrap();
        assert!(!hits.iter().any(|h| h.id == "b"));
    }

    #[tokio::test]
    async fn dim_mismatch_errors() {
        let be = InMemoryVectorBackend::new(4);
        let err = be.upsert("a", &[0.1, 0.2, 0.3], Metadata::new()).await;
        assert!(err.is_err());
        assert!(be.search(&[0.1, 0.2, 0.3], 1, None).await.is_err());
    }
}
