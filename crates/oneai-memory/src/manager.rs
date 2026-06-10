//! Memory manager — unified entry point for the memory system.
//!
//! The MemoryManager orchestrates short-term memory, long-term memory,
//! and context compression. It provides a single interface for:
//! - Storing new memories (STM + LTM)
//! - Retrieving relevant memories (STM keyword search → LTM semantic/keyword)
//! - Context compression when STM exceeds token threshold
//! - Evicted STM entries are automatically stored in LTM

use std::sync::Arc;

use oneai_core::{Conversation, MemoryEntry, MemoryQuery};
use oneai_core::error::Result;
use oneai_core::traits::{LlmProvider, MemoryStore};

use crate::short_term::ShortTermMemorySync;
use crate::long_term::LongTermMemory;
use crate::compression::{ContextCompressor, CompressedResult};

/// Configuration for the MemoryManager.
#[derive(Debug, Clone)]
pub struct MemoryManagerConfig {
    /// Short-term memory window size (max number of entries).
    pub stm_window_size: usize,
    /// Token threshold for triggering context compression.
    pub compression_threshold_tokens: usize,
    /// Number of recent turns to keep intact during compression.
    pub compression_keep_recent_turns: usize,
    /// Whether to store evicted STM entries in LTM.
    pub evict_to_ltm: bool,
}

impl Default for MemoryManagerConfig {
    fn default() -> Self {
        Self {
            stm_window_size: 20,
            compression_threshold_tokens: 4000,
            compression_keep_recent_turns: 6,
            evict_to_ltm: true,
        }
    }
}

/// Unified memory manager that orchestrates STM, LTM, and context compression.
///
/// Provides a single entry point for all memory operations:
/// - `add()`: Store a new memory entry in STM (evicted entries go to LTM)
/// - `retrieve()`: Search STM first (recent, keyword), then LTM (semantic/keyword)
/// - `compress()`: When STM exceeds token threshold, compress context using LLM
/// - `get_context()`: Assemble current STM context for LLM injection
pub struct MemoryManager {
    /// Short-term memory (sliding window).
    stm: ShortTermMemorySync,
    /// Long-term memory (vector store + content store).
    ltm: Arc<LongTermMemory>,
    /// Context compressor (uses LLM for summarization).
    compressor: Option<Arc<ContextCompressor>>,
    /// Configuration.
    config: MemoryManagerConfig,
}

impl MemoryManager {
    /// Create a new memory manager with default configuration.
    ///
    /// Without an LLM provider, context compression is not available.
    pub fn new() -> Self {
        Self {
            stm: ShortTermMemorySync::new(MemoryManagerConfig::default().stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: None,
            config: MemoryManagerConfig::default(),
        }
    }

    /// Create a new memory manager with custom configuration.
    pub fn with_config(config: MemoryManagerConfig) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: None,
            config,
        }
    }

    /// Create a memory manager with LLM-based context compression.
    pub fn with_compressor(
        config: MemoryManagerConfig,
        summarizer: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: Some(Arc::new(ContextCompressor::new(
                config.compression_threshold_tokens,
                config.compression_keep_recent_turns,
                summarizer,
            ))),
            config,
        }
    }

    /// Add a new memory entry.
    ///
    /// Stores the entry in STM. If STM is full, the oldest entry is evicted.
    /// Evicted entries are optionally stored in LTM for long-term retrieval.
    pub async fn add(&self, entry: MemoryEntry) -> Result<()> {
        let evicted = self.stm.push(entry).await;

        // If STM evicted an entry and evict_to_ltm is enabled, store it in LTM
        if let Some(evicted_entry) = evicted {
            if self.config.evict_to_ltm {
                self.ltm.store(evicted_entry).await?;
            }
        }

        Ok(())
    }

    /// Retrieve relevant memories for a query.
    ///
    /// Search strategy:
    /// 1. First search STM for recent keyword matches (fast, recent context)
    /// 2. Then search LTM for deeper semantic/keyword matches
    /// 3. Combine and deduplicate results
    pub async fn retrieve(&self, query: &MemoryQuery, top_k: usize) -> Result<Vec<MemoryEntry>> {
        // Search STM first (recent context)
        let stm_results = self.stm.retrieve(query, top_k).await?;

        // Search LTM for deeper context
        let ltm_results = self.ltm.retrieve(query, top_k).await?;

        // Combine results, deduplicating by ID
        let mut combined = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        // STM results first (most recent, most relevant for current context)
        for entry in stm_results {
            if !seen_ids.contains(&entry.id) {
                seen_ids.insert(entry.id.clone());
                combined.push(entry);
            }
        }

        // Then LTM results (deeper context)
        for entry in ltm_results {
            if !seen_ids.contains(&entry.id) {
                seen_ids.insert(entry.id.clone());
                combined.push(entry);
            }
        }

        // Truncate to top_k
        combined.truncate(top_k);

        Ok(combined)
    }

    /// Get the assembled context string from STM.
    ///
    /// Returns a formatted string suitable for injection into an LLM context window.
    pub async fn get_context(&self) -> String {
        self.stm.assemble_context().await
    }

    /// Check if context compression is needed.
    ///
    /// Returns true if the estimated token count in STM exceeds the threshold.
    pub async fn needs_compression(&self) -> bool {
        let tokens = self.stm.estimated_tokens().await;
        tokens > self.config.compression_threshold_tokens
    }

    /// Compress the STM context using the LLM summarizer.
    ///
    /// This method requires a compressor to be configured (via `with_compressor`).
    /// If no compressor is available, falls back to simple eviction.
    ///
    /// Returns the compressed result including:
    /// - The compressed conversation
    /// - The summary generated by the LLM
    /// - The entries that were removed (for LTM storage)
    pub async fn compress(&self, conversation: &Conversation) -> Result<Option<CompressedResult>> {
        if let Some(compressor) = &self.compressor {
            let result = compressor.compress(conversation).await?;

            // Store removed entries in LTM for long-term access
            for entry in result.removed_entries.iter() {
                self.ltm.store(entry.clone()).await?;
            }

            Ok(Some(result))
        } else {
            // No compressor — fall back to simple STM eviction
            let evicted = self.stm.compress(self.config.compression_threshold_tokens).await?;

            // Store evicted entries in LTM
            for entry in evicted.iter() {
                self.ltm.store(entry.clone()).await?;
            }

            Ok(None)
        }
    }

    /// Get the estimated token count in STM.
    pub async fn estimated_tokens(&self) -> usize {
        self.stm.estimated_tokens().await
    }

    /// Get the number of entries in STM.
    pub async fn stm_len(&self) -> usize {
        self.stm.entries().await.len()
    }

    /// Clear STM (evicted entries go to LTM if evict_to_ltm is enabled).
    pub async fn clear_stm(&self) -> Result<()> {
        // Move all STM entries to LTM first
        if self.config.evict_to_ltm {
            let entries = self.stm.entries().await;
            for entry in entries {
                self.ltm.store(entry).await?;
            }
        }
        self.stm.clear().await?;
        Ok(())
    }

    /// Clear LTM.
    pub async fn clear_ltm(&self) -> Result<()> {
        self.ltm.clear().await
    }

    /// Get the STM reference.
    pub fn stm(&self) -> &ShortTermMemorySync {
        &self.stm
    }

    /// Get the LTM reference.
    pub fn ltm(&self) -> &Arc<LongTermMemory> {
        &self.ltm
    }

    /// Get the configuration.
    pub fn config(&self) -> &MemoryManagerConfig {
        &self.config
    }
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use std::collections::HashMap;
    use oneai_core::MemoryEntry;

    #[tokio::test]
    async fn test_memory_manager_add_and_retrieve() {
        let manager = MemoryManager::new();

        // Add several entries
        manager.add(MemoryEntry {
            id: "1".to_string(),
            content: "Rust programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        }).await.unwrap();

        manager.add(MemoryEntry {
            id: "2".to_string(),
            content: "Python programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "assistant".to_string())]),
        }).await.unwrap();

        manager.add(MemoryEntry {
            id: "3".to_string(),
            content: "The weather is sunny".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        }).await.unwrap();

        // Retrieve by keyword
        let query = MemoryQuery {
            text: "programming".to_string(),
            embedding: None,
            time_range: None,
            metadata_filters: HashMap::new(),
        };
        let results = manager.retrieve(&query, 10).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_memory_manager_eviction_to_ltm() {
        let config = MemoryManagerConfig {
            stm_window_size: 3,
            ..Default::default()
        };
        let manager = MemoryManager::with_config(config);

        // Add 5 entries (window size is 3, so 2 will be evicted to LTM)
        for i in 1..=5 {
            manager.add(MemoryEntry {
                id: format!("{}", i),
                content: format!("Entry {} about topic {}", i, i),
                timestamp: chrono::Utc::now(),
                embedding: None,
                metadata: HashMap::new(),
            }).await.unwrap();
        }

        // STM should have only 3 entries
        assert_eq!(manager.stm_len().await, 3);

        // Retrieve should find all 5 entries (STM + LTM combined)
        let query = MemoryQuery {
            text: "topic".to_string(),
            embedding: None,
            time_range: None,
            metadata_filters: HashMap::new(),
        };
        let results = manager.retrieve(&query, 10).await.unwrap();
        assert_eq!(results.len(), 5);
    }

    #[tokio::test]
    async fn test_memory_manager_context_string() {
        let manager = MemoryManager::new();

        manager.add(MemoryEntry {
            id: "1".to_string(),
            content: "Hello world".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "user".to_string())]),
        }).await.unwrap();

        manager.add(MemoryEntry {
            id: "2".to_string(),
            content: "Hi there".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::from([("role".to_string(), "assistant".to_string())]),
        }).await.unwrap();

        let context = manager.get_context().await;
        assert!(context.contains("[user] Hello world"));
        assert!(context.contains("[assistant] Hi there"));
    }

    #[tokio::test]
    async fn test_memory_manager_dedup() {
        let config = MemoryManagerConfig {
            stm_window_size: 2,
            ..Default::default()
        };
        let manager = MemoryManager::with_config(config);

        // Add an entry that will be evicted to LTM
        manager.add(MemoryEntry {
            id: "unique".to_string(),
            content: "Important information about Rust".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        }).await.unwrap();

        // Fill STM to evict
        for i in 0..3 {
            manager.add(MemoryEntry {
                id: format!("filler_{}", i),
                content: format!("Filler entry {}", i),
                timestamp: chrono::Utc::now(),
                embedding: None,
                metadata: HashMap::new(),
            }).await.unwrap();
        }

        // Search for "Rust" — should find "unique" in LTM only, not duplicated
        let query = MemoryQuery {
            text: "Rust".to_string(),
            embedding: None,
            time_range: None,
            metadata_filters: HashMap::new(),
        };
        let results = manager.retrieve(&query, 10).await.unwrap();
        let unique_count = results.iter().filter(|e| e.id == "unique").count();
        assert_eq!(unique_count, 1);
    }
}

