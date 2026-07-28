//! RAG document indexing — stores chunks and their embeddings for retrieval.
//!
//! The index manages the lifecycle of document chunks:
//! - Adding documents (chunking + storing)
//! - Adding embeddings to chunks
//! - Removing documents
//! - Searching the index

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::error::Result;
use oneai_core::traits::{RetrievalBackend, RetrievalRequest, VectorStore};

use crate::document::{Chunk, ChunkingStrategy, Document};
use crate::retrieval::RetrievalResult;

/// A chunk entry in the index with optional embedding.
#[derive(Debug, Clone)]
pub struct IndexedChunk {
    /// The chunk data.
    pub chunk: Chunk,
    /// The embedding vector for this chunk (if computed).
    pub embedding: Option<Vec<f32>>,
}

/// RAG document index — manages document chunks and their embeddings.
///
/// The index stores chunks from multiple documents and supports:
/// - Adding documents with automatic chunking
/// - Storing embeddings for each chunk
/// - Searching chunks by vector similarity
/// - Removing documents and their chunks
///
/// ## Two interchangeable backends
///
/// - **Legacy brute-force** (`vector_store: Some`, `retrieval_backend: None`):
///   the original path — `VectorStore` upsert/search/delete with substring
///   keyword matching. Retained as the no-deps fallback.
/// - **Default retrieval stack** (`retrieval_backend: Some`): delegates to a
///   [`RetrievalBackend`] (the framework's `oneai_vector::StandardRetrievalPipeline`
///   by default — real BM25 + dense → RRF → optional rerank). This is the
///   recommended path; see [`DocumentIndex::with_default_stack`] /
///   [`DocumentIndex::with_retrieval_backend`]. When set, `search` /
///   `search_by_keyword` / `add_document` / `add_embedding` / `remove_document`
///   all route through the backend and `vector_store` is unused.
pub struct DocumentIndex {
    /// Indexed chunks keyed by chunk ID.
    chunks: HashMap<String, IndexedChunk>,
    /// Mapping from document ID to chunk IDs.
    document_chunks: HashMap<String, Vec<String>>,
    /// The legacy brute-force vector store (optional — unused when
    /// `retrieval_backend` is set).
    vector_store: Option<Arc<dyn VectorStore>>,
    /// The hybrid retrieval backend (optional — when set, all search paths
    /// delegate to it instead of the legacy `vector_store`).
    retrieval_backend: Option<Arc<dyn RetrievalBackend>>,
    /// The default chunking strategy.
    chunking_strategy: ChunkingStrategy,
}

impl DocumentIndex {
    /// Create a new document index with a (legacy) vector store.
    pub fn new(vector_store: Arc<dyn VectorStore>, chunking_strategy: ChunkingStrategy) -> Self {
        Self {
            chunks: HashMap::new(),
            document_chunks: HashMap::new(),
            vector_store: Some(vector_store),
            retrieval_backend: None,
            chunking_strategy,
        }
    }

    /// Create with default chunking strategy (fixed-size 512 chars, 64 overlap).
    pub fn with_defaults(vector_store: Arc<dyn VectorStore>) -> Self {
        Self::new(vector_store, ChunkingStrategy::default())
    }

    /// Create with a hybrid [`RetrievalBackend`] (the default retrieval stack
    /// path). The legacy `vector_store` is unused when a backend is configured.
    pub fn with_retrieval_backend(
        backend: Arc<dyn RetrievalBackend>,
        chunking_strategy: ChunkingStrategy,
    ) -> Self {
        Self {
            chunks: HashMap::new(),
            document_chunks: HashMap::new(),
            vector_store: None,
            retrieval_backend: Some(backend),
            chunking_strategy,
        }
    }

    /// Create with a hybrid [`RetrievalBackend`] and default chunking strategy.
    pub fn with_defaults_and_backend(backend: Arc<dyn RetrievalBackend>) -> Self {
        Self::with_retrieval_backend(backend, ChunkingStrategy::default())
    }

    /// Create backed by the framework's default in-memory retrieval stack
    /// (`oneai_vector::StandardRetrievalPipeline`: BM25 + dense → RRF → optional
    /// rerank). The pipeline carries the dense leg sized to the embedder's
    /// dimension but **no pipeline-level embedder** — embeddings are supplied
    /// explicitly via [`add_embedding`](Self::add_embedding) (the
    /// `AutoEmbeddingDocumentIndex` wrapper does this), so there is no
    /// double-embedding. `embedder=None` → keyword-only pipeline (real BM25,
    /// no dense leg).
    pub async fn with_default_stack(
        embedder: Option<Arc<dyn oneai_core::traits::EmbeddingService>>,
        reranker: Option<Arc<dyn oneai_core::traits::RerankerProvider>>,
    ) -> Result<Self> {
        let dim = match &embedder {
            Some(e) => e.actual_dimension().await?,
            None => 0,
        };
        let dim = if dim > 0 { Some(dim) } else { None };
        let backend =
            oneai_vector::StandardRetrievalPipeline::in_memory_default_with_dim(dim, reranker)
                .await?;
        Ok(Self::with_defaults_and_backend(backend))
    }

    /// The configured retrieval backend, if any.
    pub fn retrieval_backend(&self) -> Option<&Arc<dyn RetrievalBackend>> {
        self.retrieval_backend.as_ref()
    }

    /// Build the metadata map stored alongside a chunk: the chunk's own
    /// metadata plus the document/content/offset fields the search path
    /// reconstructs chunks from.
    fn chunk_metadata(indexed: &IndexedChunk) -> HashMap<String, String> {
        let mut metadata = indexed.chunk.metadata.clone();
        metadata.insert("document_id".to_string(), indexed.chunk.document_id.clone());
        metadata.insert("content".to_string(), indexed.chunk.content.clone());
        metadata.insert(
            "start_offset".to_string(),
            indexed.chunk.start_offset.to_string(),
        );
        metadata.insert(
            "end_offset".to_string(),
            indexed.chunk.end_offset.to_string(),
        );
        metadata
    }

    /// Add a document to the index.
    ///
    /// The document will be chunked according to the configured strategy.
    /// Each chunk is stored in the index. When a [`RetrievalBackend`] is
    /// configured, each chunk's text + metadata are also pushed into the
    /// backend's lexical leg (BM25) and content cache immediately — so
    /// [`search_by_keyword`](Self::search_by_keyword) works right away, with
    /// real BM25 ranking. The dense leg is populated once embeddings are added
    /// via [`add_embedding`](Self::add_embedding).
    pub async fn add_document(&mut self, mut document: Document) -> Result<Vec<String>> {
        // Chunk the document
        document.chunk(&self.chunking_strategy);

        let chunk_ids: Vec<String> = document
            .chunks
            .iter()
            .map(|chunk| chunk.id.clone())
            .collect();

        // Store each chunk in the index, and push its text into the retrieval
        // backend's lexical leg + content cache when configured.
        let backend = self.retrieval_backend.clone();
        for chunk in document.chunks.iter() {
            let indexed = IndexedChunk {
                chunk: chunk.clone(),
                embedding: None,
            };
            let metadata = Self::chunk_metadata(&indexed);
            self.chunks.insert(chunk.id.clone(), indexed);
            if let Some(b) = &backend {
                // No embedding here → the pipeline indexes the lexical leg +
                // content cache only (no pipeline embedder is wired, so no
                // auto-embedding; dense leg is populated by add_embedding).
                b.upsert_chunk(&chunk.id, &chunk.content, metadata, None)
                    .await?;
            }
        }

        // Track document → chunk mapping
        self.document_chunks
            .insert(document.id.clone(), chunk_ids.clone());

        Ok(chunk_ids)
    }

    /// Add an embedding vector for a chunk.
    ///
    /// Once embeddings are added, the chunk becomes searchable via
    /// vector similarity search. When a `RetrievalBackend` is configured, the
    /// chunk's text + embedding are pushed into both legs (lexical + dense) of
    /// the backend; otherwise the legacy `vector_store` is used.
    pub async fn add_embedding(&mut self, chunk_id: &str, embedding: Vec<f32>) -> Result<()> {
        let indexed = match self.chunks.get_mut(chunk_id) {
            Some(i) => i,
            None => {
                return Err(oneai_core::error::OneAIError::Rag(format!(
                    "Chunk '{}' not found in index",
                    chunk_id
                )));
            }
        };
        let metadata = Self::chunk_metadata(indexed);
        let content = indexed.chunk.content.clone();

        if let Some(b) = &self.retrieval_backend {
            b.upsert_chunk(chunk_id, &content, metadata, Some(&embedding))
                .await?;
        } else if let Some(vs) = &self.vector_store {
            vs.upsert(chunk_id, embedding.clone(), metadata).await?;
        }
        indexed.embedding = Some(embedding);
        Ok(())
    }

    /// Remove a document and all its chunks from the index.
    pub async fn remove_document(&mut self, document_id: &str) -> Result<()> {
        if let Some(chunk_ids) = self.document_chunks.remove(document_id) {
            let backend = self.retrieval_backend.clone();
            for chunk_id in chunk_ids {
                // Remove from backend (both legs) or legacy vector store.
                if let Some(b) = &backend {
                    b.delete(&chunk_id).await?;
                } else if let Some(vs) = &self.vector_store {
                    vs.delete(&chunk_id).await?;
                }
                // Remove from chunks map
                self.chunks.remove(&chunk_id);
            }
        }
        Ok(())
    }

    /// Get a chunk by ID.
    pub fn get_chunk(&self, chunk_id: &str) -> Option<&IndexedChunk> {
        self.chunks.get(chunk_id)
    }

    /// Get all chunk IDs for a document.
    pub fn document_chunk_ids(&self, document_id: &str) -> Option<&Vec<String>> {
        self.document_chunks.get(document_id)
    }

    /// Map backend `RetrievalHit`s → `RetrievalResult`s by looking up the full
    /// chunk (with its embedding) from the local index. Hits whose id is not
    /// in the local map are dropped (stale backend entries).
    fn map_hits(&self, hits: Vec<oneai_core::traits::RetrievalHit>) -> Vec<RetrievalResult> {
        hits.into_iter()
            .filter_map(|hit| {
                self.chunks.get(&hit.id).map(|indexed| RetrievalResult {
                    chunk: indexed.chunk.clone(),
                    score: hit.score,
                    // Prefer the backend's retained embedding (None for the
                    // standard pipeline) then the locally-cached one.
                    embedding: hit.embedding.or_else(|| indexed.embedding.clone()),
                })
            })
            .collect()
    }

    /// Search the index for chunks similar to the query embedding (dense leg
    /// only). When a `RetrievalBackend` is configured, delegates to it (the
    /// pipeline runs the dense leg and returns fused hits); otherwise the
    /// legacy brute-force `vector_store` is used.
    pub async fn search(
        &self,
        query_embedding: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<RetrievalResult>> {
        if let Some(b) = &self.retrieval_backend {
            let req = RetrievalRequest::vector(String::new(), query_embedding, top_k);
            let hits = b.search_hybrid(&req).await?;
            return Ok(self.map_hits(hits));
        }
        let Some(vs) = &self.vector_store else {
            return Ok(Vec::new());
        };
        let search_results = vs.search(query_embedding, top_k).await?;
        let results: Vec<RetrievalResult> = search_results
            .iter()
            .filter_map(|result| {
                self.chunks.get(&result.id).map(|indexed| RetrievalResult {
                    chunk: indexed.chunk.clone(),
                    score: result.score,
                    embedding: indexed.embedding.clone(),
                })
            })
            .collect();
        Ok(results)
    }

    /// Hybrid search (dense + lexical, fused via RRF by default). Requires a
    /// `RetrievalBackend`. `query_text` drives the lexical (BM25) leg;
    /// `query_embedding` drives the dense leg.
    pub async fn search_hybrid(
        &self,
        query_text: &str,
        query_embedding: Vec<f32>,
        top_k: usize,
    ) -> Result<Vec<RetrievalResult>> {
        let Some(b) = &self.retrieval_backend else {
            // No backend → fall back to dense-only search.
            return self.search(query_embedding, top_k).await;
        };
        let req = RetrievalRequest::hybrid(query_text, query_embedding, top_k);
        let hits = b.search_hybrid(&req).await?;
        Ok(self.map_hits(hits))
    }

    /// Search by keyword. When a `RetrievalBackend` is configured, delegates
    /// to it for real BM25 ranking (CJK-aware via tantivy-jieba); otherwise
    /// falls back to case-insensitive substring matching on chunk content.
    pub async fn search_by_keyword(&self, keyword: &str, top_k: usize) -> Vec<RetrievalResult> {
        if let Some(b) = &self.retrieval_backend {
            let req = RetrievalRequest::keyword(keyword, top_k);
            return match b.search_hybrid(&req).await {
                Ok(hits) => self.map_hits(hits),
                Err(e) => {
                    tracing::warn!("DocumentIndex::search_by_keyword backend error, falling back to substring: {e}");
                    self.search_by_keyword_substring(keyword, top_k)
                }
            };
        }
        self.search_by_keyword_substring(keyword, top_k)
    }

    /// Legacy case-insensitive substring keyword search (no backend).
    fn search_by_keyword_substring(&self, keyword: &str, top_k: usize) -> Vec<RetrievalResult> {
        let keyword_lower = keyword.to_lowercase();

        let mut results: Vec<RetrievalResult> = self
            .chunks
            .values()
            .filter(|indexed| oneai_core::keyword_matches(&indexed.chunk.content, keyword))
            .map(|indexed| {
                // Keyword search score is based on term frequency / length ratio
                let content_lower = indexed.chunk.content.to_lowercase();
                let count = content_lower.matches(&keyword_lower).count();
                let score = count as f32 / indexed.chunk.content.len() as f32;
                RetrievalResult {
                    chunk: indexed.chunk.clone(),
                    score,
                    embedding: indexed.embedding.clone(),
                }
            })
            .collect();

        // Sort by score descending
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(top_k);
        results
    }

    /// Get the number of indexed chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Get the number of indexed documents.
    pub fn document_count(&self) -> usize {
        self.document_chunks.len()
    }

    /// Get the chunking strategy.
    pub fn chunking_strategy(&self) -> &ChunkingStrategy {
        &self.chunking_strategy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use oneai_core::traits::EmbeddingService;

    /// A simple in-memory vector store for testing.
    struct TestVectorStore {
        vectors: tokio::sync::RwLock<HashMap<String, (Vec<f32>, HashMap<String, String>)>>,
    }

    impl TestVectorStore {
        fn new() -> Self {
            Self {
                vectors: tokio::sync::RwLock::new(HashMap::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl VectorStore for TestVectorStore {
        async fn upsert(
            &self,
            id: &str,
            embedding: Vec<f32>,
            metadata: HashMap<String, String>,
        ) -> Result<()> {
            self.vectors
                .write()
                .await
                .insert(id.to_string(), (embedding, metadata));
            Ok(())
        }

        async fn search(
            &self,
            query_embedding: Vec<f32>,
            top_k: usize,
        ) -> Result<Vec<oneai_core::VectorSearchResult>> {
            let vectors = self.vectors.read().await;
            let mut results: Vec<oneai_core::VectorSearchResult> = vectors
                .iter()
                .map(|(id, (embedding, metadata))| {
                    // Simple cosine similarity
                    let dot: f32 = query_embedding
                        .iter()
                        .zip(embedding.iter())
                        .map(|(a, b)| a * b)
                        .sum();
                    let norm_q: f32 = query_embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let norm_e: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let score = if norm_q > 0.0 && norm_e > 0.0 {
                        dot / (norm_q * norm_e)
                    } else {
                        0.0
                    };

                    oneai_core::VectorSearchResult {
                        id: id.clone(),
                        score: score.max(0.0),
                        metadata: metadata.clone(),
                    }
                })
                .collect();

            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            results.truncate(top_k);
            Ok(results)
        }

        async fn delete(&self, id: &str) -> Result<()> {
            self.vectors.write().await.remove(id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_document_index_add_and_search() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        // Add a document
        let doc = Document::with_id(
            "doc1",
            "Rust is a programming language. It is fast and safe.",
        );
        let chunk_ids = index.add_document(doc).await.unwrap();
        assert!(!chunk_ids.is_empty());

        // Add embeddings for each chunk
        for chunk_id in &chunk_ids {
            index
                .add_embedding(chunk_id, vec![0.1, 0.2, 0.3])
                .await
                .unwrap();
        }

        // Search by embedding
        let results = index.search(vec![0.1, 0.2, 0.3], 5).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].chunk.document_id, "doc1");
    }

    #[tokio::test]
    async fn test_document_index_keyword_search() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        // Add a document
        let doc = Document::with_id(
            "doc1",
            "Rust programming language is great for system programming",
        );
        index.add_document(doc).await.unwrap();

        // Search by keyword (legacy substring path — no backend configured)
        let results = index.search_by_keyword("programming", 5).await;
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn test_document_index_remove() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        let doc = Document::with_id("doc1", "Test content for removal");
        index.add_document(doc).await.unwrap();

        assert_eq!(index.document_count(), 1);
        assert!(index.chunk_count() > 0);

        // Remove the document
        index.remove_document("doc1").await.unwrap();
        assert_eq!(index.document_count(), 0);
        assert_eq!(index.chunk_count(), 0);
    }

    #[tokio::test]
    async fn test_document_index_get_chunk() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        let doc = Document::with_id("doc1", "Short test");
        let chunk_ids = index.add_document(doc).await.unwrap();

        // Should be able to get each chunk
        for chunk_id in &chunk_ids {
            assert!(index.get_chunk(chunk_id).is_some());
        }
    }

    #[tokio::test]
    async fn test_document_index_document_chunk_ids() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        let doc = Document::with_id("doc1", "Test document content");
        index.add_document(doc).await.unwrap();

        let chunk_ids = index.document_chunk_ids("doc1");
        assert!(chunk_ids.is_some());
        assert!(!chunk_ids.unwrap().is_empty());
    }

    // ─── Default retrieval stack (oneai-vector) path ────────────────────────

    /// Deterministic 4-d embedder for the backend-path tests (no ort / model file).
    struct StubEmbedder;
    #[async_trait::async_trait]
    impl oneai_core::traits::EmbeddingService for StubEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let mut v = vec![0.0f32; 4];
            for (i, b) in text.as_bytes().iter().enumerate() {
                v[i % 4] += *b as f32;
            }
            let n = (v.iter().map(|x| x * x).sum::<f32>().sqrt()).max(1e-6);
            v.iter_mut().for_each(|x| *x /= n);
            Ok(v)
        }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                out.push(self.embed(t).await?);
            }
            Ok(out)
        }
        fn model(&self) -> oneai_core::EmbeddingModel {
            oneai_core::EmbeddingModel::new("stub-4d")
        }
        fn dimension(&self) -> usize {
            4
        }
    }

    #[tokio::test]
    async fn test_document_index_default_stack_cjk_bm25() {
        // with_default_stack builds the BM25 lexical leg; keyword search hits
        // CJK tokens (real BM25, not substring).
        let mut index = DocumentIndex::with_default_stack(None, None).await.unwrap();
        index
            .add_document(Document::with_id("d1", "机器学习是人工智能的一个分支"))
            .await
            .unwrap();
        index
            .add_document(Document::with_id("d2", "今天天气不错适合出门散步"))
            .await
            .unwrap();

        let hits = index.search_by_keyword("人工智能", 5).await;
        assert!(hits.iter().any(|r| r.chunk.document_id == "d1"));
    }

    #[tokio::test]
    async fn test_document_index_default_stack_hybrid_search() {
        let embedder = Arc::new(StubEmbedder);
        let mut index = DocumentIndex::with_default_stack(Some(embedder), None)
            .await
            .unwrap();
        index
            .add_document(Document::with_id("d1", "alpha beta gamma"))
            .await
            .unwrap();
        index
            .add_document(Document::with_id("d2", "delta epsilon zeta"))
            .await
            .unwrap();
        // Populate the dense leg with explicit embeddings (the pipeline carries
        // no embedder, so embeddings are supplied here).
        let chunk_ids: Vec<String> = index.chunks.values().map(|c| c.chunk.id.clone()).collect();
        for cid in &chunk_ids {
            let content = index.get_chunk(cid).unwrap().chunk.content.clone();
            let emb = StubEmbedder.embed(&content).await.unwrap();
            index.add_embedding(cid, emb).await.unwrap();
        }

        // Hybrid: text + embedding both point at d1.
        let emb = StubEmbedder.embed("alpha beta gamma").await.unwrap();
        let hits = index.search_hybrid("alpha beta", emb, 5).await.unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].chunk.document_id, "d1");
    }

    #[tokio::test]
    async fn test_document_index_default_stack_remove() {
        let mut index = DocumentIndex::with_default_stack(None, None).await.unwrap();
        index
            .add_document(Document::with_id("d1", "content to delete"))
            .await
            .unwrap();
        assert_eq!(index.document_count(), 1);
        index.remove_document("d1").await.unwrap();
        assert_eq!(index.document_count(), 0);
        // After removal the backend no longer surfaces the chunk.
        let hits = index.search_by_keyword("delete", 5).await;
        assert!(hits.iter().all(|r| r.chunk.document_id != "d1"));
    }
}
