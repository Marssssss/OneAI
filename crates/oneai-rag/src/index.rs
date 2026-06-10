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
use oneai_core::traits::VectorStore;

use crate::document::{Chunk, Document, ChunkingStrategy};
use crate::retrieval::{RetrievalResult, RetrievalQuery};

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
pub struct DocumentIndex {
    /// Indexed chunks keyed by chunk ID.
    chunks: HashMap<String, IndexedChunk>,
    /// Mapping from document ID to chunk IDs.
    document_chunks: HashMap<String, Vec<String>>,
    /// The vector store for similarity search.
    vector_store: Arc<dyn VectorStore>,
    /// The default chunking strategy.
    chunking_strategy: ChunkingStrategy,
}

impl DocumentIndex {
    /// Create a new document index with a vector store.
    pub fn new(
        vector_store: Arc<dyn VectorStore>,
        chunking_strategy: ChunkingStrategy,
    ) -> Self {
        Self {
            chunks: HashMap::new(),
            document_chunks: HashMap::new(),
            vector_store,
            chunking_strategy,
        }
    }

    /// Create with default chunking strategy (fixed-size 512 chars, 64 overlap).
    pub fn with_defaults(vector_store: Arc<dyn VectorStore>) -> Self {
        Self::new(vector_store, ChunkingStrategy::default())
    }

    /// Add a document to the index.
    ///
    /// The document will be chunked according to the configured strategy.
    /// Each chunk is stored in the index and will be available for retrieval
    /// once embeddings are added via `add_embedding()`.
    pub fn add_document(&mut self, mut document: Document) -> Result<Vec<String>> {
        // Chunk the document
        document.chunk(&self.chunking_strategy);

        let chunk_ids: Vec<String> = document.chunks.iter()
            .map(|chunk| chunk.id.clone())
            .collect();

        // Store each chunk in the index
        for chunk in document.chunks.iter() {
            let indexed = IndexedChunk {
                chunk: chunk.clone(),
                embedding: None,
            };
            self.chunks.insert(chunk.id.clone(), indexed);
        }

        // Track document → chunk mapping
        self.document_chunks.insert(document.id.clone(), chunk_ids.clone());

        Ok(chunk_ids)
    }

    /// Add an embedding vector for a chunk.
    ///
    /// Once embeddings are added, the chunk becomes searchable via
    /// vector similarity search.
    pub async fn add_embedding(&mut self, chunk_id: &str, embedding: Vec<f32>) -> Result<()> {
        if let Some(indexed) = self.chunks.get_mut(chunk_id) {
            // Store the embedding in the vector store
            let mut metadata = indexed.chunk.metadata.clone();
            metadata.insert("document_id".to_string(), indexed.chunk.document_id.clone());
            metadata.insert("content".to_string(), indexed.chunk.content.clone());
            metadata.insert("start_offset".to_string(), indexed.chunk.start_offset.to_string());
            metadata.insert("end_offset".to_string(), indexed.chunk.end_offset.to_string());

            self.vector_store.upsert(
                chunk_id,
                embedding.clone(),
                metadata,
            ).await?;

            // Store the embedding in the indexed chunk
            indexed.embedding = Some(embedding);
            Ok(())
        } else {
            Err(oneai_core::error::OneAIError::Rag(format!(
                "Chunk '{}' not found in index", chunk_id
            )))
        }
    }

    /// Remove a document and all its chunks from the index.
    pub async fn remove_document(&mut self, document_id: &str) -> Result<()> {
        if let Some(chunk_ids) = self.document_chunks.remove(document_id) {
            for chunk_id in chunk_ids {
                // Remove from vector store
                self.vector_store.delete(&chunk_id).await?;
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

    /// Search the index for chunks similar to the query embedding.
    pub async fn search(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<RetrievalResult>> {
        let search_results = self.vector_store.search(query_embedding, top_k).await?;

        let results: Vec<RetrievalResult> = search_results.iter()
            .filter_map(|result| {
                // Look up the full chunk data from our index
                self.chunks.get(&result.id).map(|indexed| {
                    RetrievalResult {
                        chunk: indexed.chunk.clone(),
                        score: result.score,
                        embedding: indexed.embedding.clone(),
                    }
                })
            })
            .collect();

        Ok(results)
    }

    /// Search by keyword (for when embeddings are not available).
    ///
    /// Simple case-insensitive substring matching on chunk content.
    pub fn search_by_keyword(&self, keyword: &str, top_k: usize) -> Vec<RetrievalResult> {
        let keyword_lower = keyword.to_lowercase();

        let mut results: Vec<RetrievalResult> = self.chunks.values()
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
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
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
    use crate::document::{Document, ChunkingStrategy};

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
        async fn upsert(&self, id: &str, embedding: Vec<f32>, metadata: HashMap<String, String>) -> Result<()> {
            self.vectors.write().await.insert(id.to_string(), (embedding, metadata));
            Ok(())
        }

        async fn search(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<oneai_core::VectorSearchResult>> {
            let vectors = self.vectors.read().await;
            let mut results: Vec<oneai_core::VectorSearchResult> = vectors.iter()
                .map(|(id, (embedding, metadata))| {
                    // Simple cosine similarity
                    let dot: f32 = query_embedding.iter().zip(embedding.iter()).map(|(a, b)| a * b).sum();
                    let norm_q: f32 = query_embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let norm_e: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let score = if norm_q > 0.0 && norm_e > 0.0 { dot / (norm_q * norm_e) } else { 0.0 };

                    oneai_core::VectorSearchResult {
                        id: id.clone(),
                        score: score.max(0.0),
                        metadata: metadata.clone(),
                    }
                })
                .collect();

            results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
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
        let doc = Document::with_id("doc1", "Rust is a programming language. It is fast and safe.");
        let chunk_ids = index.add_document(doc).unwrap();
        assert!(chunk_ids.len() > 0);

        // Add embeddings for each chunk
        for chunk_id in &chunk_ids {
            index.add_embedding(chunk_id, vec![0.1, 0.2, 0.3]).await.unwrap();
        }

        // Search by embedding
        let results = index.search(vec![0.1, 0.2, 0.3], 5).await.unwrap();
        assert!(results.len() > 0);
        assert_eq!(results[0].chunk.document_id, "doc1");
    }

    #[tokio::test]
    async fn test_document_index_keyword_search() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        // Add a document
        let doc = Document::with_id("doc1", "Rust programming language is great for system programming");
        index.add_document(doc).unwrap();

        // Search by keyword
        let results = index.search_by_keyword("programming", 5);
        assert!(results.len() > 0);
    }

    #[tokio::test]
    async fn test_document_index_remove() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        let doc = Document::with_id("doc1", "Test content for removal");
        index.add_document(doc).unwrap();

        assert_eq!(index.document_count(), 1);
        assert!(index.chunk_count() > 0);

        // Remove the document
        index.remove_document("doc1").await.unwrap();
        assert_eq!(index.document_count(), 0);
        assert_eq!(index.chunk_count(), 0);
    }

    #[test]
    fn test_document_index_get_chunk() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        let doc = Document::with_id("doc1", "Short test");
        let chunk_ids = index.add_document(doc).unwrap();

        // Should be able to get each chunk
        for chunk_id in &chunk_ids {
            assert!(index.get_chunk(chunk_id).is_some());
        }
    }

    #[test]
    fn test_document_index_document_chunk_ids() {
        let vector_store = Arc::new(TestVectorStore::new());
        let mut index = DocumentIndex::with_defaults(vector_store);

        let doc = Document::with_id("doc1", "Test document content");
        index.add_document(doc).unwrap();

        let chunk_ids = index.document_chunk_ids("doc1");
        assert!(chunk_ids.is_some());
        assert!(chunk_ids.unwrap().len() > 0);
    }
}