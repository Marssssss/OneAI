//! The reference hybrid retrieval pipeline — [`RetrievalBackend`] composed
//! from a [`VectorBackend`] + [`KeywordBackend`] + optional
//! [`EmbeddingService`] + optional [`RerankerProvider`].
//!
//! Flow per [`RetrievalRequest`]:
//! 1. Run the requested legs (dense via `VectorBackend`, lexical via
//!    `KeywordBackend`). When the dense leg is needed but the request carries
//!    no embedding, the pipeline computes one via the configured
//!    `EmbeddingService` (BGE-M3, or any `oneai-rag` embedder). If no embedder
//!    is configured it logs a warning and degrades to keyword-only rather
//!    than erroring — the same "zero-burden" posture as the embedding
//!    rework.
//! 2. Fuse the legs per [`FusionMode`] — RRF (k=60, Cormack 2009) by default,
//!    DBSF if requested.
//! 3. Attach content from the in-pipeline content cache (populated at
//!    `upsert_chunk` time — see the design note below).
//! 4. Rerank the top-`rerank_pool` (default 150) via `RerankerProvider` if
//!    configured, returning `top_k`; otherwise truncate to `top_k`.
//!
//! This is the Anthropic "Contextual Retrieval" reference pipeline: BM25 +
//! dense → RRF → rerank, which cut top-20 retrieval failure rate by 67%.
//!
//! ## Why the pipeline holds a content cache
//!
//! [`VectorBackend`] / [`KeywordBackend`] return [`VectorHit`] (id + score +
//! metadata — no content), but [`RetrievalHit`] and [`RerankDoc`] need the
//! text. The low-level traits deliberately don't expose a `get(id)` for
//! stored text (they must not leak storage internals). The pipeline is the
//! single insertion path (`upsert_chunk`), so it retains `id → (content,
//! metadata)` itself. An app that brings its own `RetrievalBackend` (e.g.
//! Qdrant) stores content in its payloads and skips this pipeline entirely.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::traits::{
    EmbeddingService, FusionMode, KeywordBackend, Metadata, RerankDoc, RerankerProvider,
    RetrievalBackend, RetrievalHit, RetrievalRequest, SearchMode, VectorBackend, VectorHit,
};
use tokio::sync::Mutex;
use tracing::warn;

use crate::fusion::{dbsf_fuse, rrf_fuse};
use crate::merge_meta;
#[cfg(feature = "tantivy")]
use crate::{InMemoryVectorBackend, TantivyBm25Backend};

/// Pipeline-level configuration.
#[derive(Debug, Clone)]
pub struct StandardRetrievalPipelineConfig {
    /// How many fused candidates to feed the reranker (default 150 — the
    /// Anthropic reference). Ignored when no reranker is configured.
    pub rerank_pool: usize,
}

impl Default for StandardRetrievalPipelineConfig {
    fn default() -> Self {
        Self { rerank_pool: 150 }
    }
}

/// The reference hybrid retrieval backend.
pub struct StandardRetrievalPipeline {
    vector: Option<Arc<dyn VectorBackend>>,
    keyword: Option<Arc<dyn KeywordBackend>>,
    embedder: Option<Arc<dyn EmbeddingService>>,
    reranker: Option<Arc<dyn RerankerProvider>>,
    content: Mutex<HashMap<String, (String, Metadata)>>,
    config: StandardRetrievalPipelineConfig,
}

impl StandardRetrievalPipeline {
    /// Begin a builder.
    pub fn builder() -> StandardRetrievalPipelineBuilder {
        StandardRetrievalPipelineBuilder::default()
    }

    /// Construct with the given components and default config.
    pub fn new(
        vector: Option<Arc<dyn VectorBackend>>,
        keyword: Option<Arc<dyn KeywordBackend>>,
        embedder: Option<Arc<dyn EmbeddingService>>,
        reranker: Option<Arc<dyn RerankerProvider>>,
    ) -> Self {
        Self {
            vector,
            keyword,
            embedder,
            reranker,
            content: Mutex::new(HashMap::new()),
            config: StandardRetrievalPipelineConfig::default(),
        }
    }

    /// Build the framework's default in-memory retrieval stack as a
    /// `RetrievalBackend`: a [`TantivyBm25Backend`] (CJK-aware BM25, in-memory)
    /// for the lexical leg + an [`InMemoryVectorBackend`] sized to the
    /// embedder's dimension for the dense leg + the optional reranker.
    ///
    /// This is the zero-config stack `oneai-rag`'s `HybridDocumentIndex` and
    /// `oneai-memory`'s `MemoryFactStore` delegate to — real BM25 + dense →
    /// RRF(k=60) → optional rerank, no network/backend deps beyond what the
    /// chosen `EmbeddingService` needs.
    ///
    /// - `embedder = None` → keyword-only pipeline (no dense leg). The same
    ///   "zero-burden" posture as the embedding rework: a caller with no
    ///   embedding service still gets real BM25, never a hard error.
    /// - `embedder = Some` → the dense leg's vector dimension is resolved via
    ///   [`EmbeddingService::actual_dimension`] (BGE-M3 = 1024 fixed; FastEmbed
    ///   `all-MiniLM-L6-v2` = 384 from the known-dimensions table; Ollama is
    ///   probed by generating one test embedding).
    pub async fn in_memory_default(
        embedder: Option<Arc<dyn EmbeddingService>>,
        reranker: Option<Arc<dyn RerankerProvider>>,
    ) -> oneai_core::Result<Arc<dyn RetrievalBackend>> {
        // Lexical leg is always available — tantivy + jieba, in-memory.
        let keyword: Arc<dyn KeywordBackend> = Arc::new(TantivyBm25Backend::in_memory()?);

        let mut builder = StandardRetrievalPipelineBuilder::default().keyword(keyword);

        // Dense leg only when an embedder is configured AND its dimension is
        // resolvable (>0). A zero-dim model (e.g. an uninitialized stub) is
        // silently skipped so the pipeline degrades to keyword-only rather than
        // erroring — matching the fail-safe contract documented above.
        if let Some(e) = embedder.clone() {
            let dim = e.actual_dimension().await?;
            if dim > 0 {
                let vector: Arc<dyn VectorBackend> = Arc::new(InMemoryVectorBackend::new(dim));
                builder = builder.vector(vector).embedder(e);
            } else {
                tracing::warn!(
                    "StandardRetrievalPipeline::in_memory_default: embedder reported dim=0; \
                     building keyword-only pipeline"
                );
            }
        }

        if let Some(r) = reranker {
            builder = builder.reranker(r);
        }

        Ok(Arc::new(builder.build()) as Arc<dyn RetrievalBackend>)
    }

    #[cfg(not(feature = "tantivy"))]
    pub async fn in_memory_default(
        _embedder: Option<Arc<dyn EmbeddingService>>,
        _reranker: Option<Arc<dyn RerankerProvider>>,
    ) -> oneai_core::Result<Arc<dyn RetrievalBackend>> {
        // Without the `tantivy` feature there is no in-memory BM25 backend, so
        // the default stack can't be assembled. Surface a clear error rather
        // than silently building a dense-only pipeline that would mislead
        // callers expecting hybrid retrieval.
        Err(oneai_core::OneAIError::Rag(
            "StandardRetrievalPipeline::in_memory_default requires the `tantivy` feature \
             (on by default) — it provides the in-memory BM25 lexical leg"
                .to_string(),
        ))
    }

    /// Build the default in-memory retrieval stack with an explicit dense
    /// dimension and **no pipeline-level embedder** — the dense leg is sized
    /// to `dim` but embeddings must be supplied by the caller via
    /// `upsert_chunk(.., Some(embedding))` and `RetrievalRequest::embedding`.
    ///
    /// Use this when the caller owns the `EmbeddingService` and wants to embed
    /// once (e.g. `oneai-rag`'s `AutoEmbeddingDocumentIndex`, which embeds
    /// chunks at add time and the query at search time). Use
    /// [`in_memory_default`](Self::in_memory_default) instead when the pipeline
    /// should auto-embed (zero-config app use).
    ///
    /// - `dim = None` or `0` → keyword-only pipeline (no dense leg).
    /// - `dim = Some(d)` (d>0) → `InMemoryVectorBackend::new(d)` dense leg.
    #[cfg(feature = "tantivy")]
    pub async fn in_memory_default_with_dim(
        dim: Option<usize>,
        reranker: Option<Arc<dyn RerankerProvider>>,
    ) -> oneai_core::Result<Arc<dyn RetrievalBackend>> {
        let keyword: Arc<dyn KeywordBackend> = Arc::new(TantivyBm25Backend::in_memory()?);
        let mut builder = StandardRetrievalPipelineBuilder::default().keyword(keyword);
        if let Some(d) = dim.filter(|d| *d > 0) {
            builder = builder.vector(Arc::new(InMemoryVectorBackend::new(d)));
        }
        if let Some(r) = reranker {
            builder = builder.reranker(r);
        }
        Ok(Arc::new(builder.build()) as Arc<dyn RetrievalBackend>)
    }

    #[cfg(not(feature = "tantivy"))]
    pub async fn in_memory_default_with_dim(
        _dim: Option<usize>,
        _reranker: Option<Arc<dyn RerankerProvider>>,
    ) -> oneai_core::Result<Arc<dyn RetrievalBackend>> {
        Err(oneai_core::OneAIError::Rag(
            "StandardRetrievalPipeline::in_memory_default_with_dim requires the `tantivy` \
             feature (on by default) — it provides the in-memory BM25 lexical leg"
                .to_string(),
        ))
    }
}

/// Builder for [`StandardRetrievalPipeline`].
#[derive(Default)]
pub struct StandardRetrievalPipelineBuilder {
    vector: Option<Arc<dyn VectorBackend>>,
    keyword: Option<Arc<dyn KeywordBackend>>,
    embedder: Option<Arc<dyn EmbeddingService>>,
    reranker: Option<Arc<dyn RerankerProvider>>,
    config: StandardRetrievalPipelineConfig,
}

impl StandardRetrievalPipelineBuilder {
    pub fn vector(mut self, v: Arc<dyn VectorBackend>) -> Self {
        self.vector = Some(v);
        self
    }
    pub fn keyword(mut self, k: Arc<dyn KeywordBackend>) -> Self {
        self.keyword = Some(k);
        self
    }
    pub fn embedder(mut self, e: Arc<dyn EmbeddingService>) -> Self {
        self.embedder = Some(e);
        self
    }
    pub fn reranker(mut self, r: Arc<dyn RerankerProvider>) -> Self {
        self.reranker = Some(r);
        self
    }
    pub fn config(mut self, c: StandardRetrievalPipelineConfig) -> Self {
        self.config = c;
        self
    }
    pub fn build(self) -> StandardRetrievalPipeline {
        StandardRetrievalPipeline {
            vector: self.vector,
            keyword: self.keyword,
            embedder: self.embedder,
            reranker: self.reranker,
            content: Mutex::new(HashMap::new()),
            config: self.config,
        }
    }
}

#[async_trait]
impl RetrievalBackend for StandardRetrievalPipeline {
    async fn search_hybrid(&self, req: &RetrievalRequest) -> oneai_core::Result<Vec<RetrievalHit>> {
        let fetch_k = req.top_k.max(self.config.rerank_pool).max(1);
        let filter = req.filter.as_ref();

        let need_vector = matches!(req.mode, SearchMode::Vector | SearchMode::Hybrid);
        let need_keyword = matches!(req.mode, SearchMode::Keyword | SearchMode::Hybrid);

        let mut legs: Vec<Vec<VectorHit>> = Vec::new();
        let mut leg_metas: HashMap<String, Metadata> = HashMap::new();

        // Dense leg.
        if need_vector {
            if let Some(vb) = &self.vector {
                let embedding = match &req.embedding {
                    Some(e) => Some(e.clone()),
                    None => match &self.embedder {
                        Some(emb) => Some(emb.embed(&req.text).await?),
                        None => {
                            warn!(
                                "StandardRetrievalPipeline: dense leg requested but no embedding \
                                 supplied and no EmbeddingService configured — degrading to keyword-only"
                            );
                            None
                        }
                    },
                };
                if let Some(emb) = embedding {
                    let hits = vb.search(&emb, fetch_k, filter).await?;
                    for h in &hits {
                        leg_metas
                            .entry(h.id.clone())
                            .and_modify(|m| *m = merge_meta(m, &h.metadata))
                            .or_insert_with(|| h.metadata.clone());
                    }
                    legs.push(hits);
                }
            } else {
                warn!("StandardRetrievalPipeline: dense leg requested but no VectorBackend configured");
            }
        }

        // Lexical leg.
        if need_keyword {
            if let Some(kb) = &self.keyword {
                let hits = kb.search(&req.text, fetch_k, filter).await?;
                for h in &hits {
                    leg_metas
                        .entry(h.id.clone())
                        .and_modify(|m| *m = merge_meta(m, &h.metadata))
                        .or_insert_with(|| h.metadata.clone());
                }
                legs.push(hits);
            } else {
                warn!("StandardRetrievalPipeline: lexical leg requested but no KeywordBackend configured");
            }
        }

        // Fuse legs → ranked (id, fused_score).
        let leg_scores: Vec<Vec<(String, f32)>> = legs
            .iter()
            .map(|leg| leg.iter().map(|h| (h.id.clone(), h.score)).collect())
            .collect();
        let fused: Vec<(String, f32)> = match &req.fusion {
            FusionMode::Rrf { k, weights } => rrf_fuse(&leg_scores, *k, weights.as_deref()),
            FusionMode::Dbsf => dbsf_fuse(&leg_scores),
            // `FusionMode` is #[non_exhaustive]; unknown variants fall back to
            // the default RRF(k=60) so a future core addition can't break us.
            _ => rrf_fuse(&leg_scores, 60, None),
        };

        // Attach content + metadata; cap at rerank_pool for the rerank step.
        let pool = fused.len().min(self.config.rerank_pool.max(req.top_k));
        let mut hits: Vec<RetrievalHit> = Vec::with_capacity(pool);
        let content = self.content.lock().await;
        for (id, score) in fused.into_iter().take(pool) {
            let (c, m) = match content.get(&id) {
                Some((c, m)) => (c.clone(), m.clone()),
                None => (String::new(), leg_metas.get(&id).cloned().unwrap_or_default()),
            };
            hits.push(RetrievalHit {
                id,
                content: c,
                score,
                embedding: None,
                metadata: m,
            });
        }
        drop(content);

        // Rerank (top-150 → top-k) if a reranker is configured.
        if let Some(reranker) = &self.reranker {
            if hits.is_empty() {
                return Ok(Vec::new());
            }
            let docs: Vec<RerankDoc> = hits
                .iter()
                .map(|h| RerankDoc::new(h.id.clone(), h.content.clone()))
                .collect();
            let ranked = reranker.rerank(&req.text, &docs, req.top_k).await?;
            let content = self.content.lock().await;
            let out: Vec<RetrievalHit> = ranked
                .into_iter()
                .map(|r| {
                    let metadata = content
                        .get(&r.id)
                        .map(|(_, m)| m.clone())
                        .or_else(|| leg_metas.get(&r.id).cloned())
                        .unwrap_or_default();
                    RetrievalHit {
                        id: r.id,
                        content: r.content,
                        score: r.score,
                        embedding: None,
                        metadata,
                    }
                })
                .collect();
            Ok(out)
        } else {
            hits.truncate(req.top_k);
            Ok(hits)
        }
    }

    async fn upsert_chunk(
        &self,
        id: &str,
        content: &str,
        metadata: Metadata,
        embedding: Option<&[f32]>,
    ) -> oneai_core::Result<()> {
        // Auto-embed when the caller supplied no embedding but an embedder is
        // configured — the "zero-burden" default so a caller can index text
        // without managing embeddings itself.
        let computed = match (embedding, &self.embedder) {
            (Some(_), _) => None,
            (None, Some(emb)) => Some(emb.embed(content).await?),
            (None, None) => None,
        };
        let emb_to_store: Option<&[f32]> = embedding.or(computed.as_deref());

        {
            let mut cache = self.content.lock().await;
            cache.insert(id.to_string(), (content.to_string(), metadata.clone()));
        }

        if let (Some(vb), Some(emb)) = (&self.vector, emb_to_store) {
            vb.upsert(id, emb, metadata.clone()).await?;
        }
        if let Some(kb) = &self.keyword {
            kb.upsert_doc(id, content, metadata).await?;
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> oneai_core::Result<()> {
        {
            let mut cache = self.content.lock().await;
            cache.remove(id);
        }
        if let Some(vb) = &self.vector {
            let _ = vb.delete(id).await;
        }
        if let Some(kb) = &self.keyword {
            let _ = kb.delete(id).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::Filter;
    use crate::InMemoryVectorBackend;

    /// A deterministic stub embedder: maps text → a 4-d vector by hashing.
    /// Lets us exercise the dense leg without ort / a model file.
    struct StubEmbedder;
    #[async_trait]
    impl EmbeddingService for StubEmbedder {
        async fn embed(&self, text: &str) -> oneai_core::Result<Vec<f32>> {
            // Simple deterministic embedding: 4 dims from char sums.
            let mut v = vec![0.0f32; 4];
            for (i, b) in text.as_bytes().iter().enumerate() {
                v[i % 4] += *b as f32;
            }
            let n = (v.iter().map(|x| x * x).sum::<f32>().sqrt()).max(1e-6);
            v.iter_mut().for_each(|x| *x /= n);
            Ok(v)
        }
        async fn embed_batch(&self, texts: &[String]) -> oneai_core::Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }
        fn model(&self) -> oneai_core::EmbeddingModel {
            oneai_core::EmbeddingModel::new("stub-4d")
        }
    }

    fn make_pipeline() -> StandardRetrievalPipeline {
        let vector = Arc::new(InMemoryVectorBackend::new(4));
        StandardRetrievalPipeline::builder()
            .vector(vector)
            .keyword(Arc::new(crate::TantivyBm25Backend::in_memory().unwrap()))
            .embedder(Arc::new(StubEmbedder))
            .build()
    }

    #[tokio::test]
    async fn upsert_and_hybrid_search() {
        let pipe = make_pipeline();
        // Index a few docs with both content and auto-computed embeddings.
        pipe.upsert_chunk(
            "d1",
            "今天天气不错适合出门散步",
            Metadata::from([("tag".into(), "weather".into())]),
            None,
        )
        .await
        .unwrap();
        pipe.upsert_chunk(
            "d2",
            "机器学习是人工智能的一个分支",
            Metadata::from([("tag".into(), "ai".into())]),
            None,
        )
        .await
        .unwrap();

        // Keyword path finds d2 for "人工智能".
        let req = RetrievalRequest::keyword("人工智能", 5);
        let hits = pipe.search_hybrid(&req).await.unwrap();
        assert!(hits.iter().any(|h| h.id == "d2"));
        assert!(hits.iter().any(|h| h.content.contains("机器学习")));

        // Hybrid path with a filter.
        let emb = StubEmbedder.embed("今天天气").await.unwrap();
        let req = RetrievalRequest::hybrid("天气", emb, 5)
            .with_filter(Filter::new().with_eq("tag", "weather"));
        let hits = pipe.search_hybrid(&req).await.unwrap();
        assert!(hits.iter().all(|h| h.metadata["tag"] == "weather"));

        // Delete removes from both legs + content cache.
        pipe.delete("d1").await.unwrap();
        let hits = pipe.search_hybrid(&RetrievalRequest::keyword("天气", 5)).await.unwrap();
        assert!(!hits.iter().any(|h| h.id == "d1"));
    }

    #[tokio::test]
    async fn rrf_fuses_dense_and_lexical() {
        // Two docs share no content overlap but the dense embedder makes d1
        // closest to a query whose text also lexically matches d1 — RRF should
        // rank d1 first.
        let pipe = make_pipeline();
        pipe.upsert_chunk("d1", "alpha beta gamma", Metadata::new(), None).await.unwrap();
        pipe.upsert_chunk("d2", "delta epsilon zeta", Metadata::new(), None).await.unwrap();
        let emb = StubEmbedder.embed("alpha beta gamma").await.unwrap();
        let req = RetrievalRequest::hybrid("alpha beta", emb, 5);
        let hits = pipe.search_hybrid(&req).await.unwrap();
        assert_eq!(hits[0].id, "d1");
    }

    #[tokio::test]
    async fn in_memory_default_with_embedder_builds_dense_leg() {
        // The default-stack constructor wires both legs + the embedder, so
        // upsert_chunk(text, None) auto-embeds and a dense-only query returns
        // the right doc.
        let backend = StandardRetrievalPipeline::in_memory_default(
            Some(Arc::new(StubEmbedder)),
            None,
        )
        .await
        .unwrap();
        backend
            .upsert_chunk("d1", "rust programming language", Metadata::new(), None)
            .await
            .unwrap();
        let emb = StubEmbedder.embed("rust language").await.unwrap();
        let req = RetrievalRequest::vector("rust", emb, 5);
        let hits = backend.search_hybrid(&req).await.unwrap();
        assert!(hits.iter().any(|h| h.id == "d1" && h.content.contains("rust")));
    }

    #[tokio::test]
    async fn in_memory_default_without_embedder_degrades_to_keyword() {
        // No embedder → keyword-only pipeline. Dense-only queries return nothing
        // (no vector leg), keyword queries still hit via BM25.
        let backend = StandardRetrievalPipeline::in_memory_default(None, None)
            .await
            .unwrap();
        backend
            .upsert_chunk("d1", "机器学习是人工智能的一个分支", Metadata::new(), None)
            .await
            .unwrap();
        // Dense-only with a fabricated embedding → no vector leg → empty.
        let req = RetrievalRequest::vector("人工智能", vec![0.0; 4], 5);
        assert!(backend.search_hybrid(&req).await.unwrap().is_empty());
        // Keyword path hits.
        let req = RetrievalRequest::keyword("人工智能", 5);
        let hits = backend.search_hybrid(&req).await.unwrap();
        assert!(hits.iter().any(|h| h.content.contains("机器学习")));
    }
}
