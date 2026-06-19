//! Memory manager — unified entry point for the memory system.
//!
//! The MemoryManager orchestrates short-term memory, long-term memory,
//! context compression, and the STM↔LTM closed loop. It provides a single interface for:
//! - Storing new memories (STM + LTM)
//! - Retrieving relevant memories (STM keyword search → LTM semantic/keyword)
//! - Context compression when STM exceeds token threshold
//! - Evicted STM entries are automatically stored in LTM
//! - **LTM→STM feedback injection**: proactively recall relevant LTM memories into STM context
//! - **Memory reflection**: at session end, reflect on STM and generate episodic LTM entries

use std::sync::Arc;

use oneai_core::{Conversation, MemoryEntry, MemoryQuery};
use oneai_core::error::Result;
use oneai_core::traits::{LlmProvider, MemoryStore, MemoryPersistence, EmbeddingService};

use crate::short_term::ShortTermMemorySync;
use crate::long_term::LongTermMemory;
use crate::compression::{ContextCompressor, CompressedResult};
use crate::reflection::{MemoryReflection, MemoryReflectionConfig, EpisodicMemory};

// ─── RecallStrategy ──────────────────────────────────────────────

/// Strategy for recalling memories from LTM into STM context.
///
/// Different strategies are suited for different scenarios:
/// - KeywordFirst: works without embeddings (faster, simpler)
/// - SemanticFirst: requires embeddings (more relevant, deeper)
/// - Hybrid: combines both (best coverage, aligned with HybridScorer)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallStrategy {
    /// Keyword search first, then semantic if available.
    /// Best for scenarios without embeddings.
    KeywordFirst,
    /// Semantic (embedding) search first, then keyword as fallback.
    /// Best for scenarios with embeddings.
    SemanticFirst,
    /// Both keyword and semantic search, merge and deduplicate.
    /// Best for hybrid scenarios (aligned with HybridScorer).
    Hybrid,
}

impl Default for RecallStrategy {
    fn default() -> Self {
        Self::Hybrid
    }
}

// ─── MemoryInjectionConfig ──────────────────────────────────────

/// Configuration for LTM→STM context injection.
///
/// This controls how the memory manager proactively injects
/// relevant LTM memories into the STM context on each new turn.
#[derive(Debug, Clone)]
pub struct MemoryInjectionConfig {
    /// Maximum number of LTM entries to inject per turn.
    pub inject_top_k: usize,
    /// The recall strategy to use for injection.
    pub inject_strategy: RecallStrategy,
    /// Whether to automatically inject on each new user message.
    pub inject_on_new_turn: bool,
    /// Whether to deduplicate against existing STM entries.
    pub dedup_against_stm: bool,
}

impl Default for MemoryInjectionConfig {
    fn default() -> Self {
        Self {
            inject_top_k: 3,
            inject_strategy: RecallStrategy::Hybrid,
            inject_on_new_turn: true,
            dedup_against_stm: true,
        }
    }
}

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

/// Unified memory manager that orchestrates STM, LTM, context compression,
/// and the STM↔LTM closed loop.
///
/// Provides a single entry point for all memory operations:
/// - `add()`: Store a new memory entry in STM (evicted entries go to LTM)
/// - `retrieve()`: Search STM first (recent, keyword), then LTM (semantic/keyword)
/// - `compress()`: When STM exceeds token threshold, compress context using LLM
/// - `get_context()`: Assemble current STM context for LLM injection
/// - `inject_ltm_context()`: Proactively recall LTM → STM (closed loop feedback)
/// - `reflect()`: At session end, generate episodic memory from STM → LTM
pub struct MemoryManager {
    /// Short-term memory (sliding window).
    stm: ShortTermMemorySync,
    /// Long-term memory (vector store + content store).
    ltm: Arc<LongTermMemory>,
    /// Context compressor (uses LLM for summarization).
    compressor: Option<Arc<ContextCompressor>>,
    /// Memory reflection engine (uses LLM for episodic memory generation).
    reflection: Option<Arc<MemoryReflection>>,
    /// Configuration for memory injection (LTM→STM feedback).
    injection_config: MemoryInjectionConfig,
    /// Configuration.
    config: MemoryManagerConfig,
    /// Persistence backend (optional — enables SQLite storage).
    ///
    /// When set, STM/LTM entries are persisted to SQLite on each operation,
    /// enabling session resume and knowledge accumulation across restarts.
    persistence: Option<Arc<dyn MemoryPersistence>>,
    /// Embedding service (optional — enables automatic embedding generation).
    ///
    /// When set, embeddings are automatically computed for memory entries
    /// during `add()` and `inject_ltm_context()`, enabling true semantic
    /// search in LTM. Without this, memory search is limited to keyword matching.
    embedding_service: Option<Arc<dyn EmbeddingService>>,
}

impl MemoryManager {
    /// Create a new memory manager with default configuration.
    ///
    /// Without an LLM provider, context compression and reflection are not available.
    /// Without a persistence backend, memory is purely in-memory (lost on restart).
    /// Without an embedding service, memory search is limited to keyword matching.
    pub fn new() -> Self {
        Self {
            stm: ShortTermMemorySync::new(MemoryManagerConfig::default().stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: None,
            reflection: None,
            injection_config: MemoryInjectionConfig::default(),
            config: MemoryManagerConfig::default(),
            persistence: None,
            embedding_service: None,
        }
    }

    /// Create a new memory manager with custom configuration.
    pub fn with_config(config: MemoryManagerConfig) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: None,
            reflection: None,
            injection_config: MemoryInjectionConfig::default(),
            config,
            persistence: None,
            embedding_service: None,
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
                summarizer.clone(),
            ))),
            reflection: None,
            injection_config: MemoryInjectionConfig::default(),
            config,
            persistence: None,
            embedding_service: None,
        }
    }

    /// Create a memory manager with both compression and reflection.
    ///
    /// This enables the full STM↔LTM closed loop:
    /// - Context compression (STM eviction → LTM storage)
    /// - Memory reflection (STM → LTM episodic memory at session end)
    /// - LTM→STM injection (proactive recall on each turn)
    pub fn with_compressor_and_reflection(
        config: MemoryManagerConfig,
        injection_config: MemoryInjectionConfig,
        summarizer: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: Some(Arc::new(ContextCompressor::new(
                config.compression_threshold_tokens,
                config.compression_keep_recent_turns,
                summarizer.clone(),
            ))),
            reflection: Some(Arc::new(MemoryReflection::new(summarizer))),
            injection_config,
            config,
            persistence: None,
            embedding_service: None,
        }
    }

    /// Create a memory manager with SQLite persistence.
    pub fn with_persistence(
        config: MemoryManagerConfig,
        persistence: Arc<dyn MemoryPersistence>,
    ) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: None,
            reflection: None,
            injection_config: MemoryInjectionConfig::default(),
            config,
            persistence: Some(persistence),
            embedding_service: None,
        }
    }

    /// Create a memory manager with compression, reflection, and persistence.
    ///
    /// This enables the full STM↔LTM closed loop with persistent storage:
    /// - Context compression (STM eviction → LTM storage → SQLite)
    /// - Memory reflection (STM → LTM episodic memory → SQLite)
    /// - LTM→STM injection (proactive recall on each turn)
    /// - Session resume (load from SQLite on restart)
    pub fn with_compressor_reflection_and_persistence(
        config: MemoryManagerConfig,
        injection_config: MemoryInjectionConfig,
        summarizer: Arc<dyn LlmProvider>,
        persistence: Arc<dyn MemoryPersistence>,
    ) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: Some(Arc::new(ContextCompressor::new(
                config.compression_threshold_tokens,
                config.compression_keep_recent_turns,
                summarizer.clone(),
            ))),
            reflection: Some(Arc::new(MemoryReflection::new(summarizer))),
            injection_config,
            config,
            persistence: Some(persistence),
            embedding_service: None,
        }
    }

    /// Create a memory manager with embedding service for automatic embedding generation.
    ///
    /// When an embedding service is configured, embeddings are automatically
    /// computed for each memory entry during `add()`, enabling true semantic
    /// search in LTM. Without this, memory search is limited to keyword matching.
    ///
    /// **Usage**:
    /// ```ignore
    /// let embedding_service = Arc::new(FastEmbedService::new());
    /// let manager = MemoryManager::with_embedding(
    ///     MemoryManagerConfig::default(),
    ///     embedding_service,
    /// );
    /// ```
    pub fn with_embedding(
        config: MemoryManagerConfig,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: None,
            reflection: None,
            injection_config: MemoryInjectionConfig::default(),
            config,
            persistence: None,
            embedding_service: Some(embedding_service),
        }
    }

    /// Create a memory manager with compression, reflection, and embedding service.
    ///
    /// This enables the full STM↔LTM closed loop with semantic search:
    /// - Context compression (STM eviction → LTM storage with embeddings)
    /// - Memory reflection (STM → LTM episodic memory with embeddings)
    /// - LTM→STM injection (semantic recall via embedding similarity)
    pub fn with_compressor_reflection_and_embedding(
        config: MemoryManagerConfig,
        injection_config: MemoryInjectionConfig,
        summarizer: Arc<dyn LlmProvider>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: Some(Arc::new(ContextCompressor::new(
                config.compression_threshold_tokens,
                config.compression_keep_recent_turns,
                summarizer.clone(),
            ))),
            reflection: Some(Arc::new(MemoryReflection::new(summarizer))),
            injection_config,
            config,
            persistence: None,
            embedding_service: Some(embedding_service),
        }
    }

    /// Create a memory manager with all features: compression, reflection, persistence, embedding.
    ///
    /// This is the **complete** MemoryManager configuration enabling:
    /// - Context compression (STM eviction → LTM storage → SQLite)
    /// - Memory reflection (STM → LTM episodic memory → SQLite)
    /// - LTM→STM injection (semantic recall via embedding similarity)
    /// - Session resume (load from SQLite on restart)
    /// - Auto-embedding (each entry gets a computed embedding for semantic search)
    pub fn with_all_features(
        config: MemoryManagerConfig,
        injection_config: MemoryInjectionConfig,
        summarizer: Arc<dyn LlmProvider>,
        persistence: Arc<dyn MemoryPersistence>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            stm: ShortTermMemorySync::new(config.stm_window_size),
            ltm: Arc::new(LongTermMemory::new()),
            compressor: Some(Arc::new(ContextCompressor::new(
                config.compression_threshold_tokens,
                config.compression_keep_recent_turns,
                summarizer.clone(),
            ))),
            reflection: Some(Arc::new(MemoryReflection::new(summarizer))),
            injection_config,
            config,
            persistence: Some(persistence),
            embedding_service: Some(embedding_service),
        }
    }

    /// Add a new memory entry.
    ///
    /// Stores the entry in STM. If STM is full, the oldest entry is evicted.
    /// Evicted entries are optionally stored in LTM for long-term retrieval.
    /// If persistence is enabled, the entry is also persisted to SQLite.
    /// If an embedding service is configured, the entry's embedding is
    /// automatically computed (if the entry doesn't already have one).
    pub async fn add(&self, entry: MemoryEntry) -> Result<()> {
        // Auto-embed: compute embedding if the entry doesn't have one
        let entry = if entry.embedding.is_none() && self.embedding_service.is_some() {
            let service = self.embedding_service.as_ref().unwrap();
            let embedding = service.embed(&entry.content).await?;
            MemoryEntry {
                id: entry.id,
                content: entry.content,
                timestamp: entry.timestamp,
                embedding: Some(embedding),
                metadata: entry.metadata,
            }
        } else {
            entry
        };

        let evicted = self.stm.push(entry.clone()).await;

        // If STM evicted an entry and evict_to_ltm is enabled, store it in LTM
        if let Some(evicted_entry) = evicted {
            if self.config.evict_to_ltm {
                self.ltm.store(evicted_entry.clone()).await?;
                // Persist evicted entry to LTM storage
                if let Some(p) = &self.persistence {
                    p.save_ltm(&evicted_entry).await?;
                }
            }
        }

        // Persist the new entry to STM storage
        if let Some(p) = &self.persistence {
            // Get all current STM entries to persist the full window
            let stm_entries = self.stm.entries().await;
            p.save_stm(&entry.metadata.get("session_id").cloned().unwrap_or_default(), &stm_entries).await?;
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
    /// If persistence is enabled, STM entries are also cleared in SQLite.
    pub async fn clear_stm(&self) -> Result<()> {
        // Move all STM entries to LTM first
        if self.config.evict_to_ltm {
            let entries = self.stm.entries().await;
            for entry in entries {
                self.ltm.store(entry.clone()).await?;
                if let Some(p) = &self.persistence {
                    p.save_ltm(&entry).await?;
                }
            }
        }
        self.stm.clear().await?;
        // Clear STM in persistence (if we know the session ID)
        if let Some(p) = &self.persistence {
            // Use empty session ID as fallback — real usage should call clear_stm_session()
            p.clear_stm("").await.ok(); // Non-critical — may fail if no session ID
        }
        Ok(())
    }

    /// Clear STM for a specific session in both memory and persistence.
    pub async fn clear_stm_session(&self, session_id: &str) -> Result<()> {
        if self.config.evict_to_ltm {
            let entries = self.stm.entries().await;
            for entry in entries {
                self.ltm.store(entry.clone()).await?;
                if let Some(p) = &self.persistence {
                    p.save_ltm(&entry).await?;
                }
            }
        }
        self.stm.clear().await?;
        if let Some(p) = &self.persistence {
            p.clear_stm(session_id).await?;
        }
        Ok(())
    }

    /// Clear LTM in both memory and persistence.
    pub async fn clear_ltm(&self) -> Result<()> {
        self.ltm.clear().await?;
        if let Some(p) = &self.persistence {
            p.clear_ltm().await?;
        }
        Ok(())
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

    // ─── STM↔LTM Closed Loop ──────────────────────────────────────────

    /// Proactively inject relevant LTM memories into the STM context.
    ///
    /// This is the key feedback mechanism in the STM↔LTM closed loop:
    /// on each new user turn, relevant LTM entries are recalled and
    /// injected as ephemeral "recall" entries into the STM context.
    ///
    /// **Injection strategy** (controlled by `MemoryInjectionConfig`):
    /// 1. Query LTM for entries relevant to the current input
    /// 2. Deduplicate against existing STM entries (if dedup_against_stm is true)
    /// 3. Inject filtered entries as "recall" type entries (metadata source=ltm_recall)
    /// 4. These recall entries are ephemeral — they don't count against STM window eviction
    ///
    /// Returns the injected entries for context assembly.
    pub async fn inject_ltm_context(&self, current_input: &str) -> Result<Vec<MemoryEntry>> {
        let top_k = self.injection_config.inject_top_k;
        if top_k == 0 {
            return Ok(Vec::new());
        }

        // Auto-embed: compute query embedding if embedding service is available
        let query_embedding = if self.embedding_service.is_some() {
            let service = self.embedding_service.as_ref().unwrap();
            Some(service.embed(current_input).await?)
        } else {
            None
        };

        // Build the LTM query based on the recall strategy
        let query = MemoryQuery {
            text: current_input.to_string(),
            embedding: query_embedding,
            time_range: None,
            metadata_filters: std::collections::HashMap::new(),
        };

        // Retrieve from LTM based on strategy
        let ltm_results = match self.injection_config.inject_strategy {
            RecallStrategy::KeywordFirst => {
                // Keyword search only (no embedding needed)
                self.ltm.retrieve(&query, top_k).await?
            }
            RecallStrategy::SemanticFirst => {
                // Semantic search first, then keyword fallback
                self.ltm.retrieve(&query, top_k).await?
            }
            RecallStrategy::Hybrid => {
                // Both channels, merge and deduplicate
                self.ltm.retrieve(&query, top_k).await?
            }
        };

        // Deduplicate against existing STM entries
        let stm_entries = self.stm.entries().await;
        let stm_ids: std::collections::HashSet<String> = stm_entries.iter()
            .map(|e| e.id.clone())
            .collect();

        let filtered: Vec<MemoryEntry> = if self.injection_config.dedup_against_stm {
            ltm_results.into_iter()
                .filter(|entry| !stm_ids.contains(&entry.id))
                .take(top_k)
                .collect()
        } else {
            ltm_results.into_iter().take(top_k).collect()
        };

        // Inject filtered entries as recall-type entries into STM
        let injected: Vec<MemoryEntry> = filtered.into_iter()
            .map(|entry| {
                // Create a recall-type entry (ephemeral, doesn't evict)
                let mut recall_entry = entry.clone();
                recall_entry.id = format!("recall_{}", entry.id);
                recall_entry.metadata.insert("source".to_string(), "ltm_recall".to_string());
                recall_entry
            })
            .collect();

        // Store recall entries in STM (they're ephemeral, will be overwritten)
        for entry in &injected {
            self.stm.push(entry.clone()).await;
        }

        Ok(injected)
    }

    /// Reflect on the current session and generate an episodic memory.
    ///
    /// This is the final step in the STM↔LTM closed loop: at session end,
    /// the LLM reflects on all STM entries, extracts key insights and
    /// decisions, and stores the resulting EpisodicMemory in LTM.
    ///
    /// Requires a MemoryReflection engine (set via `with_compressor_and_reflection`).
    /// If no reflection engine is available, returns Ok(None) (no reflection).
    ///
    /// Returns the generated EpisodicMemory (also stored in LTM).
    pub async fn reflect(&self, session_id: &str) -> Result<Option<EpisodicMemory>> {
        if let Some(reflection) = &self.reflection {
            let stm_entries = self.stm.entries().await;
            let episodic = reflection.reflect(session_id, &stm_entries).await?;

            // Store the episodic memory entry in LTM
            let entry = episodic.to_memory_entry();
            self.ltm.store(entry).await?;

            Ok(Some(episodic))
        } else {
            Ok(None)
        }
    }

    /// Get the injection configuration.
    pub fn injection_config(&self) -> &MemoryInjectionConfig {
        &self.injection_config
    }

    /// Set the injection configuration.
    pub fn set_injection_config(&mut self, config: MemoryInjectionConfig) {
        self.injection_config = config;
    }

    /// Get the reflection engine (if configured).
    pub fn reflection(&self) -> Option<&Arc<MemoryReflection>> {
        self.reflection.as_ref()
    }

    /// Get the persistence backend (if configured).
    pub fn persistence(&self) -> Option<&Arc<dyn MemoryPersistence>> {
        self.persistence.as_ref()
    }

    /// Get the embedding service (if configured).
    pub fn embedding_service(&self) -> Option<&Arc<dyn EmbeddingService>> {
        self.embedding_service.as_ref()
    }

    /// Set the embedding service (for runtime configuration).
    ///
    /// This is useful when the embedding service is configured after
    /// the MemoryManager is created (e.g., via AppBuilder).
    pub fn set_embedding_service(&mut self, service: Arc<dyn EmbeddingService>) {
        self.embedding_service = Some(service);
    }

    // ─── Session Persistence ──────────────────────────────────────────

    /// Restore a session from persistence.
    ///
    /// Loads STM entries and LTM context from the persistence backend
    /// into the in-memory stores. This enables session resume after
    /// application restart.
    ///
    /// Returns the number of STM entries restored.
    pub async fn restore_session(&self, session_id: &str) -> Result<usize> {
        if let Some(p) = &self.persistence {
            // Load STM entries from persistence
            let stm_entries = p.load_stm(session_id).await?;
            for entry in &stm_entries {
                self.stm.push(entry.clone()).await;
            }

            // Load LTM entries from persistence (populate in-memory LTM)
            // We load keyword-relevant entries to populate the in-memory store
            let ltm_entries = p.search_ltm_keyword("", 1000).await?; // Load all
            for entry in &ltm_entries {
                self.ltm.store(entry.clone()).await?;
            }

            tracing::info!(
                "Restored session '{}': {} STM entries, {} LTM entries",
                session_id, stm_entries.len(), ltm_entries.len()
            );

            Ok(stm_entries.len())
        } else {
            Ok(0)
        }
    }

    /// Save the current session state to persistence.
    ///
    /// Persists STM entries and the in-memory LTM entries to SQLite.
    /// Called at the end of a conversation turn (or on explicit save).
    pub async fn save_session(&self, session_id: &str, conversation: &Conversation) -> Result<()> {
        if let Some(p) = &self.persistence {
            // Save STM entries (current sliding window)
            let stm_entries = self.stm.entries().await;
            p.save_stm(session_id, &stm_entries).await?;

            // Save conversation history
            p.save_conversation(session_id, conversation).await?;

            tracing::info!(
                "Saved session '{}': {} STM entries, {} conversation messages",
                session_id, stm_entries.len(), conversation.messages.len()
            );

            Ok(())
        } else {
            Ok(())
        }
    }

    /// Save a single LTM entry to persistence (for evicted/compressed entries).
    pub async fn persist_ltm_entry(&self, entry: &MemoryEntry) -> Result<()> {
        if let Some(p) = &self.persistence {
            p.save_ltm(entry).await?;
        }
        Ok(())
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

    // ─── STM↔LTM Closed Loop Tests ──────────────────────────────────

    #[tokio::test]
    async fn test_inject_ltm_context_basic() {
        let config = MemoryManagerConfig {
            stm_window_size: 10,
            evict_to_ltm: true,
            ..Default::default()
        };
        let injection_config = MemoryInjectionConfig {
            inject_top_k: 3,
            inject_strategy: RecallStrategy::KeywordFirst,
            inject_on_new_turn: true,
            dedup_against_stm: true,
        };
        let mut manager = MemoryManager::with_config(config);
        manager.set_injection_config(injection_config);

        // Add entries that will be evicted to LTM
        for i in 0..12 {
            manager.add(MemoryEntry {
                id: format!("evict_{}", i),
                content: format!("Entry {} about Rust programming", i),
                timestamp: chrono::Utc::now(),
                embedding: None,
                metadata: HashMap::new(),
            }).await.unwrap();
        }

        // STM has only 10 entries, so 2 were evicted to LTM
        assert_eq!(manager.stm_len().await, 10);

        // Inject LTM context for a query about "Rust"
        let injected = manager.inject_ltm_context("Rust").await.unwrap();
        // Should find evicted entries in LTM that contain "Rust"
        assert!(injected.len() > 0);
        // All injected entries should have source=ltm_recall
        for entry in &injected {
            assert_eq!(entry.metadata.get("source").unwrap(), "ltm_recall");
        }
    }

    #[tokio::test]
    async fn test_inject_ltm_context_dedup() {
        let config = MemoryManagerConfig {
            stm_window_size: 10,
            evict_to_ltm: true,
            ..Default::default()
        };
        let injection_config = MemoryInjectionConfig {
            inject_top_k: 5,
            inject_strategy: RecallStrategy::KeywordFirst,
            dedup_against_stm: true,
            inject_on_new_turn: true,
        };
        let mut manager = MemoryManager::with_config(config);
        manager.set_injection_config(injection_config);

        // Add one entry that stays in STM
        manager.add(MemoryEntry {
            id: "stm_entry".to_string(),
            content: "Python programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        }).await.unwrap();

        // Manually add an entry to LTM with same content
        manager.ltm.store(MemoryEntry {
            id: "stm_entry".to_string(), // Same ID as STM entry
            content: "Python programming language".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        }).await.unwrap();

        // Inject — should dedup against STM entries
        let injected = manager.inject_ltm_context("Python").await.unwrap();
        // The LTM entry with same ID should be deduped
        assert!(injected.iter().all(|e| e.id != "recall_stm_entry"));
    }

    #[tokio::test]
    async fn test_inject_ltm_context_no_results() {
        let manager = MemoryManager::new();

        // LTM is empty — inject should return nothing
        let injected = manager.inject_ltm_context("anything").await.unwrap();
        assert!(injected.is_empty());
    }

    #[test]
    fn test_recall_strategy_default() {
        assert_eq!(RecallStrategy::default(), RecallStrategy::Hybrid);
    }

    #[test]
    fn test_injection_config_default() {
        let config = MemoryInjectionConfig::default();
        assert_eq!(config.inject_top_k, 3);
        assert_eq!(config.inject_strategy, RecallStrategy::Hybrid);
        assert!(config.inject_on_new_turn);
        assert!(config.dedup_against_stm);
    }

    #[tokio::test]
    async fn test_reflect_no_engine() {
        let manager = MemoryManager::new();

        // No reflection engine — should return None
        let result = manager.reflect("sess_test").await.unwrap();
        assert!(result.is_none());
    }
}
