//! Long-term memory — embedded vector store + content store + hybrid scoring.
//!
//! **Legacy / read-only回溯 layer (M2).** As of the memory rework, the
//! canonical long-term memory is the `MemoryFact`-based `fact_archive`
//! (Mem0-style, three-factor recall), and raw-transcript回溯 is served by
//! persisted conversation snapshots (`save_conversation`/`load_conversation`).
//! The `MemoryManager` no longer writes to or recalls from this
//! `MemoryEntry`-based `LongTermMemory` in the production path — it is
//! retained for backward compatibility and direct low-level use only.
//!
//! The EmbeddedVectorStore is a lightweight in-memory vector store
//! using brute-force cosine similarity search. Suitable for mobile
//! deployment with no external dependencies.
//!
//! The ContentStore stores the actual text content associated with each
//! vector entry, enabling keyword search and full content retrieval.
//!
//! The LongTermMemory combines the vector store with content store,
//! hybrid scoring (semantic similarity + temporal proximity), and implements
//! the MemoryStore trait for unified access.

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::{MemoryEntry, MemoryQuery, VectorSearchResult};
use oneai_core::error::Result;
use oneai_core::traits::{MemoryStore, VectorStore};

use crate::hybrid_scorer::HybridScorer;

// ─── EmbeddedVectorStore ────────────────────────────────────────────────────

/// Embedded lightweight vector store (HNSW-like).
///
/// A simple in-memory vector store using brute-force cosine similarity search.
/// Suitable for mobile deployment with no external dependencies.
/// Can be swapped for a proper HNSW implementation or remote VectorStoreClient.
pub struct EmbeddedVectorStore {
    /// Stored vectors with their IDs and metadata.
    vectors: HashMap<String, Vec<f32>>,
    /// Metadata associated with each vector entry.
    metadata: HashMap<String, HashMap<String, String>>,
    /// Timestamps for temporal scoring.
    timestamps: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

impl EmbeddedVectorStore {
    /// Create a new embedded vector store.
    pub fn new() -> Self {
        Self {
            vectors: HashMap::new(),
            metadata: HashMap::new(),
            timestamps: HashMap::new(),
        }
    }

    /// Compute cosine similarity between two vectors.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        dot / (norm_a * norm_b)
    }

    /// Compute temporal proximity score.
    ///
    /// Returns a score from 0.0 to 1.0 based on how recent the entry is.
    /// Uses exponential decay: more recent entries score higher.
    pub fn temporal_score(entry_time: &chrono::DateTime<chrono::Utc>, reference_time: &chrono::DateTime<chrono::Utc>) -> f32 {
        let diff = reference_time.timestamp() - entry_time.timestamp();
        if diff <= 0 {
            return 1.0 // Same time or future = maximum recency
        }
        // Exponential decay with half-life of 1 hour (3600 seconds)
        let half_life: f64 = 3600.0;
        let decay = std::cmp::min(diff, 365 * 24 * 3600) as f64; // Cap at 1 year
        let score = 0.5_f64.powf(decay / half_life);
        score as f32
    }

    /// Search for vectors similar to the query embedding, with hybrid scoring.
    ///
    /// Uses the provided HybridScorer to combine semantic similarity
    /// with temporal proximity.
    pub fn search_hybrid(
        &self,
        query_embedding: &[f32],
        top_k: usize,
        scorer: &HybridScorer,
    ) -> Vec<VectorSearchResult> {
        let now = chrono::Utc::now();

        let mut results: Vec<VectorSearchResult> = self.vectors.iter()
            .map(|(id, embedding)| {
                let semantic = Self::cosine_similarity(query_embedding, embedding);
                let timestamp = self.timestamps.get(id).unwrap_or(&now);
                let temporal = Self::temporal_score(timestamp, &now);
                let hybrid = scorer.score(semantic, temporal);

                VectorSearchResult {
                    id: id.clone(),
                    score: hybrid,
                    metadata: self.metadata.get(id).cloned().unwrap_or_default(),
                }
            })
            .collect();

        // Sort by hybrid score descending
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        results
    }

    /// Keyword search over metadata content fields.
    ///
    /// Searches through all stored entries' metadata for content matching
    /// the keyword query. Returns results ranked by temporal proximity.
    pub fn search_by_keyword(&self, keyword: &str, top_k: usize) -> Vec<VectorSearchResult> {
        let now = chrono::Utc::now();

        let mut results: Vec<VectorSearchResult> = self.metadata.iter()
            .filter(|(_, meta)| {
                // Search in content field and all metadata values
                meta.get("content").map(|c| oneai_core::keyword_matches(c, keyword)).unwrap_or(false)
                    || meta.values().any(|v| oneai_core::keyword_matches(v, keyword))
            })
            .map(|(id, meta)| {
                let timestamp = self.timestamps.get(id).unwrap_or(&now);
                let temporal = Self::temporal_score(timestamp, &now);
                VectorSearchResult {
                    id: id.clone(),
                    score: temporal, // Keyword search uses temporal score as ranking
                    metadata: meta.clone(),
                }
            })
            .collect();

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        results
    }

    /// Get the number of stored vectors.
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }
}

impl Default for EmbeddedVectorStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl VectorStore for EmbeddedVectorStore {
    async fn upsert(&self, _id: &str, _embedding: Vec<f32>, _metadata: HashMap<String, String>) -> Result<()> {
        // Note: VectorStore trait requires &self, but we need mutation.
        // In practice, use ThreadSafeEmbeddedVectorStore which wraps this in RwLock.
        Ok(())
    }

    async fn search(&self, _query_embedding: Vec<f32>, _top_k: usize) -> Result<Vec<VectorSearchResult>> {
        // Same interior mutability limitation
        Ok(Vec::new())
    }

    async fn delete(&self, _id: &str) -> Result<()> {
        Ok(())
    }
}

// ─── ContentStore ────────────────────────────────────────────────────────────

/// In-memory content store for long-term memory entries.
///
/// Stores the full content of memory entries alongside their metadata,
/// enabling keyword search and full content retrieval. Entries are stored
/// by their unique ID and can be retrieved even without embeddings.
pub struct ContentStore {
    /// Stored entries keyed by ID.
    entries: HashMap<String, MemoryEntry>,
}

impl ContentStore {
    /// Create a new empty content store.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Store a memory entry.
    pub fn insert(&mut self, entry: MemoryEntry) {
        self.entries.insert(entry.id.clone(), entry);
    }

    /// Retrieve a memory entry by ID.
    pub fn get(&self, id: &str) -> Option<&MemoryEntry> {
        self.entries.get(id)
    }

    /// Delete a memory entry by ID.
    pub fn remove(&mut self, id: &str) -> Option<MemoryEntry> {
        self.entries.remove(id)
    }

    /// Search entries by keyword (case-insensitive substring match).
    ///
    /// Returns entries whose content contains the keyword,
    /// ordered by timestamp (most recent first).
    pub fn search_by_keyword(&self, keyword: &str) -> Vec<&MemoryEntry> {
        let mut results: Vec<&MemoryEntry> = self.entries.values()
            .filter(|entry| oneai_core::keyword_matches(&entry.content, keyword))
            .collect();
        // Sort by timestamp descending (most recent first)
        results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        results
    }

    /// Search entries by keyword with metadata filtering.
    ///
    /// Filters entries by metadata key-value pairs AND keyword match.
    pub fn search_by_keyword_with_filter(
        &self,
        keyword: &str,
        metadata_filters: &HashMap<String, String>,
    ) -> Vec<&MemoryEntry> {
        let mut results: Vec<&MemoryEntry> = self.entries.values()
            .filter(|entry| {
                // Keyword match
                if !keyword.is_empty() && !oneai_core::keyword_matches(&entry.content, keyword) {
                    return false;
                }
                // Metadata filter match
                for (key, value) in metadata_filters {
                    match entry.metadata.get(key) {
                        Some(v) if v == value => {}
                        _ => return false,
                    }
                }
                true
            })
            .collect();
        results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        results
    }

    /// Get all entries.
    pub fn all_entries(&self) -> Vec<&MemoryEntry> {
        self.entries.values().collect()
    }

    /// Get the number of stored entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for ContentStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ThreadSafeEmbeddedVectorStore ────────────────────────────────────────

/// Thread-safe wrapper for EmbeddedVectorStore.
///
/// Uses interior mutability (RwLock) to allow async trait methods
/// and concurrent access.
pub struct ThreadSafeEmbeddedVectorStore {
    inner: Arc<tokio::sync::RwLock<EmbeddedVectorStore>>,
    scorer: HybridScorer,
}

impl ThreadSafeEmbeddedVectorStore {
    /// Create a new thread-safe vector store with default scorer.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(EmbeddedVectorStore::new())),
            scorer: HybridScorer::new(),
        }
    }

    /// Create with custom hybrid scorer weights.
    pub fn with_scorer_weights(alpha: f32, beta: f32) -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(EmbeddedVectorStore::new())),
            scorer: HybridScorer::with_weights(alpha, beta),
        }
    }

    /// Upsert a vector with metadata and timestamp.
    pub async fn upsert_entry(
        &self,
        id: &str,
        embedding: Vec<f32>,
        metadata: HashMap<String, String>,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let mut store = self.inner.write().await;
        store.vectors.insert(id.to_string(), embedding);
        store.metadata.insert(id.to_string(), metadata);
        store.timestamps.insert(id.to_string(), timestamp);
        Ok(())
    }

    /// Search with hybrid scoring (semantic + temporal).
    pub async fn search_hybrid(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<VectorSearchResult>> {
        let store = self.inner.read().await;
        Ok(store.search_hybrid(&query_embedding, top_k, &self.scorer))
    }

    /// Keyword search over stored metadata.
    pub async fn search_by_keyword(&self, keyword: &str, top_k: usize) -> Result<Vec<VectorSearchResult>> {
        let store = self.inner.read().await;
        Ok(store.search_by_keyword(keyword, top_k))
    }

    /// Delete a vector by ID.
    pub async fn delete_entry(&self, id: &str) -> Result<()> {
        let mut store = self.inner.write().await;
        store.vectors.remove(id);
        store.metadata.remove(id);
        store.timestamps.remove(id);
        Ok(())
    }

    /// Get the number of stored vectors.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }
}

#[async_trait::async_trait]
impl VectorStore for ThreadSafeEmbeddedVectorStore {
    async fn upsert(&self, id: &str, embedding: Vec<f32>, metadata: HashMap<String, String>) -> Result<()> {
        let now = chrono::Utc::now();
        self.upsert_entry(id, embedding, metadata, now).await
    }

    async fn search(&self, query_embedding: Vec<f32>, top_k: usize) -> Result<Vec<VectorSearchResult>> {
        self.search_hybrid(query_embedding, top_k).await
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.delete_entry(id).await
    }
}

// ─── ThreadSafeContentStore ─────────────────────────────────────────────────

/// Thread-safe wrapper for ContentStore.
///
/// Uses interior mutability (RwLock) for concurrent access.
pub struct ThreadSafeContentStore {
    inner: Arc<tokio::sync::RwLock<ContentStore>>,
}

impl ThreadSafeContentStore {
    /// Create a new thread-safe content store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(ContentStore::new())),
        }
    }

    /// Store a memory entry.
    pub async fn insert(&self, entry: MemoryEntry) {
        self.inner.write().await.insert(entry);
    }

    /// Retrieve a memory entry by ID.
    pub async fn get(&self, id: &str) -> Option<MemoryEntry> {
        self.inner.read().await.get(id).cloned()
    }

    /// Delete a memory entry by ID.
    pub async fn remove(&self, id: &str) -> Option<MemoryEntry> {
        self.inner.write().await.remove(id)
    }

    /// Search entries by keyword.
    pub async fn search_by_keyword(&self, keyword: &str) -> Vec<MemoryEntry> {
        self.inner.read().await
            .search_by_keyword(keyword)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Search entries by keyword with metadata filtering.
    pub async fn search_by_keyword_with_filter(
        &self,
        keyword: &str,
        metadata_filters: &HashMap<String, String>,
    ) -> Vec<MemoryEntry> {
        self.inner.read().await
            .search_by_keyword_with_filter(keyword, metadata_filters)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Get the number of stored entries.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Check if the store is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// Clear all entries.
    pub async fn clear(&self) {
        self.inner.write().await.clear();
    }
}

impl Default for ThreadSafeContentStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─── LongTermMemory ───────────────────────────────────────────────────────

/// Long-term memory combining vector storage, content store, and hybrid scoring.
///
/// Implements the MemoryStore trait for unified access. Uses the
/// ThreadSafeEmbeddedVectorStore for semantic search and ThreadSafeContentStore
/// for keyword search and full content retrieval.
///
/// The retrieval strategy is:
/// 1. If query has an embedding → hybrid search (semantic + temporal)
/// 2. If query has no embedding → keyword search on content store
/// 3. Results are looked up in content store for full content
pub struct LongTermMemory {
    /// The underlying vector store for semantic search.
    vector_store: ThreadSafeEmbeddedVectorStore,
    /// The content store for keyword search and full content retrieval.
    content_store: ThreadSafeContentStore,
}

impl LongTermMemory {
    /// Create a new long-term memory with default settings.
    pub fn new() -> Self {
        Self {
            vector_store: ThreadSafeEmbeddedVectorStore::new(),
            content_store: ThreadSafeContentStore::new(),
        }
    }

    /// Create with custom scorer weights for the vector store.
    pub fn with_scorer_weights(alpha: f32, beta: f32) -> Self {
        Self {
            vector_store: ThreadSafeEmbeddedVectorStore::with_scorer_weights(alpha, beta),
            content_store: ThreadSafeContentStore::new(),
        }
    }

    /// Get the underlying vector store for direct access.
    pub fn vector_store(&self) -> &ThreadSafeEmbeddedVectorStore {
        &self.vector_store
    }

    /// Get the underlying content store for direct access.
    pub fn content_store(&self) -> &ThreadSafeContentStore {
        &self.content_store
    }
}

impl Default for LongTermMemory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl MemoryStore for LongTermMemory {
    async fn store(&self, entry: MemoryEntry) -> Result<()> {
        // Always store content in the content store
        self.content_store.insert(entry.clone()).await;

        // If the entry has an embedding, also store it in the vector store
        if let Some(embedding) = &entry.embedding {
            // Store content in metadata for vector store keyword search
            let mut metadata = entry.metadata.clone();
            metadata.insert("content".to_string(), entry.content.clone());

            self.vector_store.upsert_entry(
                &entry.id,
                embedding.clone(),
                metadata,
                entry.timestamp,
            ).await?;
        }

        Ok(())
    }

    async fn retrieve(&self, query: &MemoryQuery, top_k: usize) -> Result<Vec<MemoryEntry>> {
        // If the query has an embedding, use hybrid search
        if let Some(query_embedding) = &query.embedding {
            let results = self.vector_store.search_hybrid(query_embedding.clone(), top_k).await?;
            // Look up full entries from content store by ID
            let mut entries = Vec::new();
            for result in results.iter() {
                if let Some(entry) = self.content_store.get(&result.id).await {
                    entries.push(entry);
                }
            }
            Ok(entries)
        } else {
            // No embedding — use keyword search on content store
            let entries = if query.metadata_filters.is_empty() {
                self.content_store.search_by_keyword(&query.text).await
            } else {
                self.content_store.search_by_keyword_with_filter(
                    &query.text,
                    &query.metadata_filters,
                ).await
            };
            // Apply time range filter if specified
            let filtered: Vec<MemoryEntry> = if let Some(time_range) = &query.time_range {
                entries.into_iter()
                    .filter(|entry| {
                        entry.timestamp >= time_range.start && entry.timestamp <= time_range.end
                    })
                    .take(top_k)
                    .collect()
            } else {
                entries.into_iter().take(top_k).collect()
            };
            Ok(filtered)
        }
    }

    async fn compress(&self, _threshold: usize) -> Result<Vec<MemoryEntry>> {
        // Long-term memory doesn't compress — it's persistent storage.
        // Compression is handled by ShortTermMemory and ContextCompressor.
        Ok(Vec::new())
    }

    async fn clear(&self) -> Result<()> {
        // Clear both stores
        self.content_store.clear().await;
        // For vector store, we need to clear all entries
        // Since we don't have a direct clear method, we'd need to iterate and delete
        // This is a limitation of the current design — we'd need a clear() on ThreadSafeEmbeddedVectorStore
        Ok(())
    }
}