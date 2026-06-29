//! Memory manager — unified entry point for the memory system.
//!
//! The MemoryManager orchestrates the canonical long-term memory
//! (`fact_archive` + `core_memory`), per-turn recall, compression-coupled
//! fact extraction, and session-end reflection. Working memory is
//! single-sourced on the `Conversation` (M1); the legacy STM/LTM
//! `MemoryEntry` stores have been removed.
//!
//! - `recall_facts()`: per-turn three-factor recall from the archival tier
//! - `archive_facts()` / `archive_discarded_snapshot()`: write canonical facts
//!   and raw-transcript snapshots
//! - `reflect()`: at session end, reflect on the conversation → episodic fact
//! - `save_session()`: persist the conversation snapshot for resume

use std::sync::Arc;

use oneai_core::{Conversation, MemoryEntry, MemoryFact, Message};
use oneai_core::error::Result;
use oneai_core::traits::{LlmProvider, MemoryPersistence, EmbeddingService, DiscardedSink};

use crate::reflection::{MemoryReflection, EpisodicMemory};

// `RecallStrategy` is defined canonically in `oneai-core` so that the
// domain-level `MemoryProfile` and this runtime manager share one type.
// Re-export it here to preserve the historical `oneai_memory::RecallStrategy`
// path.
pub use oneai_core::RecallStrategy;

/// Configuration for the MemoryManager.
#[derive(Debug, Clone)]
pub struct MemoryManagerConfig {
    /// Token threshold for triggering context compression.
    pub compression_threshold_tokens: usize,
    /// Number of recent turns to keep intact during compression.
    pub compression_keep_recent_turns: usize,
}

impl Default for MemoryManagerConfig {
    fn default() -> Self {
        Self {
            compression_threshold_tokens: 4000,
            compression_keep_recent_turns: 6,
        }
    }
}

/// Unified memory manager that orchestrates the canonical fact tiers,
/// per-turn recall, and session-end reflection.
///
/// Provides a single entry point for all memory operations:
/// - `recall_facts()`: three-factor recall (relevance + recency + importance)
///   from the archival tier, surfaced each turn via `CoreMemorySource`.
/// - `archive_facts()` / `archive_discarded_snapshot()`: write canonical facts
///   (Mem0-style conflict resolution) and raw-transcript conversation snapshots.
/// - `reflect()`: at session end, reflect on the conversation and store an
///   episodic fact in the archival tier (+ persist).
/// - `save_session()`: persist the conversation snapshot for resume.
pub struct MemoryManager {
    /// Memory reflection engine (uses LLM for episodic memory generation).
    reflection: Option<Arc<MemoryReflection>>,
    /// Configuration.
    config: MemoryManagerConfig,
    /// Persistence backend (optional — enables SQLite storage).
    ///
    /// When set, facts and conversation snapshots are persisted to SQLite,
    /// enabling session resume and knowledge accumulation across restarts.
    persistence: Option<Arc<dyn MemoryPersistence>>,
    /// Embedding service (optional — enables semantic recall).
    ///
    /// When set, query embeddings are computed in `recall_facts()`, enabling
    /// true semantic relevance scoring. Without this, recall is keyword-based.
    embedding_service: Option<Arc<dyn EmbeddingService>>,

    /// Core memory tier — always-in-context curated facts (Letta-style core).
    /// Surfaced each turn via `CoreMemorySource` (P4) and self-managed via
    /// memory tools (P5). Owned here so the compressor's extraction sink and
    /// the injection source share one instance.
    core_memory: Arc<crate::core_memory::CoreMemory>,

    /// Archival fact tier — the full `MemoryFact` base recalled on demand.
    /// Fed by `FactExtractor` on compression (P3, the "压缩即丢失" closure)
    /// and by the agent's `archival_memory_insert` tool (P5).
    fact_archive: Arc<crate::fact_store::MemoryFactStore>,

    /// Owning user id (cross-session habit namespace). Interior-mutable so it
    /// can be set after Arc construction (via AppBuilder).
    user_id: tokio::sync::RwLock<String>,
    /// Per-session id (episodic namespace). Interior-mutable so it can be
    /// updated each run through the shared `Arc<MemoryManager>`.
    session_id: tokio::sync::RwLock<String>,
}

impl MemoryManager {
    /// Create a new memory manager with default configuration.
    ///
    /// Without an LLM provider, reflection is not available. Without a
    /// persistence backend, memory is purely in-memory (lost on restart).
    /// Without an embedding service, recall is keyword-based.
    pub fn new() -> Self {
        Self {
            reflection: None,
            config: MemoryManagerConfig::default(),
            persistence: None,
            embedding_service: None,
            core_memory: Arc::new(crate::core_memory::CoreMemory::new(2048)),
            fact_archive: Arc::new(crate::fact_store::MemoryFactStore::new()),
            user_id: tokio::sync::RwLock::new(String::new()),
            session_id: tokio::sync::RwLock::new(String::new()),
        }
    }

    /// Create a new memory manager with custom configuration.
    pub fn with_config(config: MemoryManagerConfig) -> Self {
        Self {
            reflection: None,
            config,
            persistence: None,
            embedding_service: None,
            core_memory: Arc::new(crate::core_memory::CoreMemory::new(2048)),
            fact_archive: Arc::new(crate::fact_store::MemoryFactStore::new()),
            user_id: tokio::sync::RwLock::new(String::new()),
            session_id: tokio::sync::RwLock::new(String::new()),
        }
    }

    /// Create a memory manager with reflection enabled.
    ///
    /// The same provider is used for the reflection prompt. This enables the
    /// session-end reflection closed loop: conversation → episodic fact →
    /// archival tier (+ persist).
    pub fn with_compressor_and_reflection(
        config: MemoryManagerConfig,
        summarizer: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            reflection: Some(Arc::new(MemoryReflection::new(summarizer))),
            config,
            persistence: None,
            embedding_service: None,
            core_memory: Arc::new(crate::core_memory::CoreMemory::new(2048)),
            fact_archive: Arc::new(crate::fact_store::MemoryFactStore::new()),
            user_id: tokio::sync::RwLock::new(String::new()),
            session_id: tokio::sync::RwLock::new(String::new()),
        }
    }

    /// Create a memory manager with SQLite persistence.
    pub fn with_persistence(
        config: MemoryManagerConfig,
        persistence: Arc<dyn MemoryPersistence>,
    ) -> Self {
        Self {
            reflection: None,
            config,
            persistence: Some(persistence),
            embedding_service: None,
            core_memory: Arc::new(crate::core_memory::CoreMemory::new(2048)),
            fact_archive: Arc::new(crate::fact_store::MemoryFactStore::new()),
            user_id: tokio::sync::RwLock::new(String::new()),
            session_id: tokio::sync::RwLock::new(String::new()),
        }
    }

    /// Create a memory manager with reflection and persistence.
    ///
    /// This enables the full closed loop with persistent storage:
    /// - Reflection (conversation → episodic fact → SQLite)
    /// - Session resume (load facts + conversation snapshot on restart)
    pub fn with_compressor_reflection_and_persistence(
        config: MemoryManagerConfig,
        summarizer: Arc<dyn LlmProvider>,
        persistence: Arc<dyn MemoryPersistence>,
    ) -> Self {
        Self {
            reflection: Some(Arc::new(MemoryReflection::new(summarizer))),
            config,
            persistence: Some(persistence),
            embedding_service: None,
            core_memory: Arc::new(crate::core_memory::CoreMemory::new(2048)),
            fact_archive: Arc::new(crate::fact_store::MemoryFactStore::new()),
            user_id: tokio::sync::RwLock::new(String::new()),
            session_id: tokio::sync::RwLock::new(String::new()),
        }
    }

    /// Create a memory manager with embedding service for semantic recall.
    ///
    /// When an embedding service is configured, `recall_facts()` computes a
    /// query embedding for semantic relevance scoring. Without this, recall is
    /// keyword-based.
    pub fn with_embedding(
        config: MemoryManagerConfig,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            reflection: None,
            config,
            persistence: None,
            embedding_service: Some(embedding_service),
            core_memory: Arc::new(crate::core_memory::CoreMemory::new(2048)),
            fact_archive: Arc::new(crate::fact_store::MemoryFactStore::new()),
            user_id: tokio::sync::RwLock::new(String::new()),
            session_id: tokio::sync::RwLock::new(String::new()),
        }
    }

    /// Create a memory manager with reflection and embedding service.
    pub fn with_compressor_reflection_and_embedding(
        config: MemoryManagerConfig,
        summarizer: Arc<dyn LlmProvider>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            reflection: Some(Arc::new(MemoryReflection::new(summarizer))),
            config,
            persistence: None,
            embedding_service: Some(embedding_service),
            core_memory: Arc::new(crate::core_memory::CoreMemory::new(2048)),
            fact_archive: Arc::new(crate::fact_store::MemoryFactStore::new()),
            user_id: tokio::sync::RwLock::new(String::new()),
            session_id: tokio::sync::RwLock::new(String::new()),
        }
    }

    /// Create a memory manager with all features: reflection, persistence, embedding.
    ///
    /// This is the **complete** MemoryManager configuration enabling:
    /// - Reflection (conversation → episodic fact → SQLite)
    /// - Semantic recall (query embedding → three-factor scoring)
    /// - Session resume (load facts + conversation snapshot on restart)
    pub fn with_all_features(
        config: MemoryManagerConfig,
        summarizer: Arc<dyn LlmProvider>,
        persistence: Arc<dyn MemoryPersistence>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            reflection: Some(Arc::new(MemoryReflection::new(summarizer))),
            config,
            persistence: Some(persistence),
            embedding_service: Some(embedding_service),
            core_memory: Arc::new(crate::core_memory::CoreMemory::new(2048)),
            fact_archive: Arc::new(crate::fact_store::MemoryFactStore::new()),
            user_id: tokio::sync::RwLock::new(String::new()),
            session_id: tokio::sync::RwLock::new(String::new()),
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &MemoryManagerConfig {
        &self.config
    }

    // ─── Fact tiers (core / archival) ─────────────────────────────────

    /// The core memory tier (always-in-context curated facts).
    pub fn core_memory(&self) -> &Arc<crate::core_memory::CoreMemory> {
        &self.core_memory
    }

    /// The archival fact tier (full `MemoryFact` base, recalled on demand).
    ///
    /// This is the sink for `FactExtractor` output on compression — the
    /// "压缩即丢失" closure: discarded turns become conflict-resolved facts
    /// here rather than being lost.
    pub fn fact_archive(&self) -> &Arc<crate::fact_store::MemoryFactStore> {
        &self.fact_archive
    }

    /// The owning user id (cross-session habit namespace). Empty if unset.
    pub async fn user_id(&self) -> String {
        self.user_id.read().await.clone()
    }

    /// Set the owning user id (for runtime configuration via AppBuilder).
    /// Takes `&self` — interior mutability lets it work through the shared
    /// `Arc<MemoryManager>` after construction.
    pub async fn set_user_id(&self, user_id: impl Into<String>) {
        *self.user_id.write().await = user_id.into();
    }

    /// The current session id (episodic namespace). Set per run.
    pub async fn session_id(&self) -> String {
        self.session_id.read().await.clone()
    }

    /// Set the current session id (called by AppSession before each run).
    /// Takes `&self` — interior mutability lets it work through the shared
    /// `Arc<MemoryManager>`.
    pub async fn set_session_id(&self, session_id: impl Into<String>) {
        *self.session_id.write().await = session_id.into();
    }

    /// Archive a batch of facts into the archival tier with Mem0-style
    /// conflict resolution (same-key facts update rather than duplicate).
    ///
    /// Called by the compression-coupled `FactExtractor` path and by the
    /// agent's `archival_memory_insert` tool (P5). When a persistence backend
    /// is configured, facts are also durably stored so they survive restart.
    pub async fn archive_facts(&self, facts: Vec<MemoryFact>) {
        for fact in &facts {
            self.fact_archive.upsert(fact.clone()).await;
            if let Some(p) = &self.persistence {
                if let Err(e) = p.store_fact(fact).await {
                    tracing::warn!("Failed to persist fact: {}", e);
                }
            }
        }
    }

    /// Load durable facts from persistence into the archival tier on resume.
    ///
    /// Loads cross-session user facts (habits) plus this session's episodic
    /// facts, so the agent starts with its accumulated memory. No-op without a
    /// persistence backend.
    pub async fn load_persisted_facts(&self) {
        let p = match &self.persistence {
            Some(p) => p.clone(),
            None => return,
        };
        let user_id = self.user_id().await;
        let session_id = self.session_id().await;
        // Cross-session habits (all user facts) — empty session scope.
        if let Ok(habits) = p.load_facts(&user_id, "").await {
            for f in habits {
                self.fact_archive.upsert(f).await;
            }
        }
        // This session's episodic facts.
        if !session_id.is_empty() {
            if let Ok(episodic) = p.load_facts(&user_id, &session_id).await {
                for f in episodic {
                    self.fact_archive.upsert(f).await;
                }
            }
        }
    }

    /// Recall the top-k most salient facts from the archival tier for a query
    /// — the canonical per-turn recall path (R1).
    ///
    /// Uses the three-factor scorer (relevance + recency + importance) via
    /// `MemoryFactStore::search_hybrid`. When an embedding service is
    /// configured, the query is embedded for semantic relevance; otherwise
    /// keyword matching is used. `time_decay` follows the domain's
    /// `MemoryProfile.recall.time_decay`.
    pub async fn recall_facts(&self, query: &str, top_k: usize) -> Result<Vec<MemoryFact>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let query_embedding = if let Some(svc) = &self.embedding_service {
            match svc.embed(query).await {
                Ok(emb) => Some(emb),
                Err(e) => {
                    tracing::warn!("query embedding failed, falling back to keyword recall: {}", e);
                    None
                }
            }
        } else {
            None
        };
        // time_decay defaults on; the caller can pass a domain-specific config
        // via the recall call site when wired through MemoryProfile.
        let facts = self
            .fact_archive
            .search_hybrid(query_embedding.as_deref(), query, top_k, true)
            .await;
        Ok(facts)
    }

    /// Archive messages discarded during context compression as a turn-scoped
    /// conversation snapshot (the "压缩即不丢" raw-transcript closure, C2).
    ///
    /// The discarded `Message`s are persisted via `save_conversation` under a
    /// derived id (`"{session}::discarded::{uuid}"`) so they remain available
    /// for resume, audit, and on-demand `memory_search` fallback — raw
    /// transcript is not lost even though it leaves the working context.
    /// No-op without a persistence backend (in-memory runs have no durable
    /// snapshot to write). Fact extraction already ran inside the compressor;
    /// this is the complementary raw-transcript archive.
    pub async fn archive_discarded_snapshot(
        &self,
        session_id: &str,
        discarded: Vec<Message>,
    ) -> Result<()> {
        if discarded.is_empty() {
            return Ok(());
        }
        let Some(p) = &self.persistence else {
            // No persistence: nothing durable to write. The discarded segment
            // was already run through the FactExtractor inside the compressor,
            // so canonical facts survive even without a raw snapshot.
            return Ok(());
        };
        let mut conv = Conversation::new();
        for m in discarded {
            conv.add_message(m);
        }
        let id = format!("{}::discarded::{}", session_id, uuid::Uuid::new_v4());
        p.save_conversation(&id, &conv).await?;
        Ok(())
    }

    // ─── Reflection ───────────────────────────────────────────────────

    /// Reflect on the current session and generate an episodic memory.
    ///
    /// At session end, the LLM reflects on the conversation, extracts key
    /// insights and decisions, and stores the resulting `EpisodicMemory` as a
    /// canonical `MemoryFact` (fact_type "episodic") in the archival tier —
    /// the "提炼型 episodic 中间层" (M5). When persistence is configured the
    /// episodic fact is also durably stored, so it survives restart and is
    /// recalled by the three-factor scorer in later sessions.
    ///
    /// Requires a MemoryReflection engine (set via `with_compressor_and_reflection`).
    /// If no reflection engine is available, returns Ok(None) (no reflection).
    ///
    /// Reflects on `conversation` directly (working-memory single source).
    ///
    /// Returns the generated EpisodicMemory (also stored in fact_archive).
    pub async fn reflect(
        &self,
        session_id: &str,
        conversation: &Conversation,
    ) -> Result<Option<EpisodicMemory>> {
        let Some(reflection) = &self.reflection else {
            return Ok(None);
        };

        // Build the entry view the reflector expects from the live conversation.
        let entries: Vec<MemoryEntry> = conversation.messages.iter()
            .filter(|m| !matches!(m.role, oneai_core::Role::System))
            .map(|m| {
                let role = match m.role {
                    oneai_core::Role::User => "user",
                    oneai_core::Role::Assistant => "assistant",
                    oneai_core::Role::Tool => "tool",
                    _ => "user",
                };
                MemoryEntry {
                    id: format!("reflect_{}", uuid::Uuid::new_v4()),
                    content: m.text_content(),
                    timestamp: chrono::Utc::now(),
                    embedding: None,
                    metadata: std::collections::HashMap::from([
                        ("role".to_string(), role.to_string()),
                    ]),
                }
            })
            .collect();

        let episodic = reflection.reflect(session_id, &entries).await?;

        // Store the episodic memory as a canonical archival fact (M5 middle
        // layer) + persist.
        let fact = episodic.to_fact();
        self.fact_archive.upsert(fact.clone()).await;
        if let Some(p) = &self.persistence {
            if let Err(e) = p.store_fact(&fact).await {
                tracing::warn!("Failed to persist episodic fact: {}", e);
            }
        }

        Ok(Some(episodic))
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
    /// Useful when the embedding service is configured after the MemoryManager
    /// is created (e.g., via AppBuilder).
    pub fn set_embedding_service(&mut self, service: Arc<dyn EmbeddingService>) {
        self.embedding_service = Some(service);
    }

    // ─── Session Persistence ──────────────────────────────────────────

    /// Save the current session state to persistence.
    ///
    /// Persists the conversation snapshot to SQLite. Called at the end of a
    /// conversation turn (or on explicit save / `/compact`). Facts are
    /// persisted incrementally via `archive_facts` / `store_fact`; this method
    /// handles the raw conversation snapshot used for resume and on-demand
    /// `memory_search` fallback.
    pub async fn save_session(&self, session_id: &str, conversation: &Conversation) -> Result<()> {
        if let Some(p) = &self.persistence {
            p.save_conversation(session_id, conversation).await?;
            tracing::info!(
                "Saved session '{}': {} conversation messages",
                session_id, conversation.messages.len()
            );
            Ok(())
        } else {
            Ok(())
        }
    }
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ArchivalDiscardedSink ──────────────────────────────────────────────────

/// `DiscardedSink` backed by a `MemoryManager`.
///
/// Hands discarded compression messages to `MemoryManager::
/// archive_discarded_snapshot`, which persists them as a turn-scoped
/// conversation snapshot for resume / audit / on-demand `memory_search`.
/// Wired into `ContextBudgetManager` by `AppSession` so the agent loop's
/// auto-compression archives raw transcript instead of dropping it.
pub struct ArchivalDiscardedSink {
    mm: Arc<MemoryManager>,
}

impl ArchivalDiscardedSink {
    /// Create a sink backed by the given memory manager.
    pub fn new(mm: Arc<MemoryManager>) -> Self {
        Self { mm }
    }
}

#[async_trait::async_trait]
impl DiscardedSink for ArchivalDiscardedSink {
    async fn archive_discarded(&self, session_id: &str, discarded: Vec<Message>) -> Result<()> {
        self.mm.archive_discarded_snapshot(session_id, discarded).await
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use std::collections::HashMap;
    use oneai_core::MemoryEntry;

    #[tokio::test]
    async fn test_recall_facts_from_archive() {
        // A2: the canonical per-turn recall path returns facts from the
        // archival tier (keyword path, no embedding service configured).
        let manager = MemoryManager::new();
        let fact = oneai_core::MemoryFact {
            id: "f1".to_string(),
            user_id: String::new(),
            session_id: String::new(),
            fact_type: oneai_core::FactType::new("decision"),
            subject: "auth.module".to_string(),
            predicate: "decided_to".to_string(),
            content: "use pnpm as the package manager".to_string(),
            embedding: None,
            metadata: HashMap::new(),
            importance: 0.8,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        };
        manager.archive_facts(vec![fact]).await;

        let recalled = manager.recall_facts("pnpm", 5).await.unwrap();
        assert_eq!(recalled.len(), 1);
        assert!(recalled[0].content.contains("pnpm"));
    }

    #[tokio::test]
    async fn test_recall_facts_empty_archive() {
        let manager = MemoryManager::new();
        let recalled = manager.recall_facts("anything", 5).await.unwrap();
        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn test_archive_facts_conflict_resolves() {
        // Mem0 invariant: same-key facts update, not duplicate.
        let manager = MemoryManager::new();
        let mk = |content: &str| oneai_core::MemoryFact {
            id: format!("f_{}", content),
            user_id: "alice".to_string(),
            session_id: "s1".to_string(),
            fact_type: oneai_core::FactType::new("user_tooling_pref"),
            subject: "user.pm".to_string(),
            predicate: "prefers".to_string(),
            content: content.to_string(),
            embedding: None,
            metadata: HashMap::new(),
            importance: 0.5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        };
        manager.archive_facts(vec![mk("npm")]).await;
        manager.archive_facts(vec![mk("pnpm")]).await;
        assert_eq!(manager.fact_archive().len().await, 1);
        let f = manager.fact_archive().get("alice", "user.pm", "prefers").await.unwrap();
        assert_eq!(f.content, "pnpm");
    }

    #[tokio::test]
    async fn test_archive_discarded_snapshot_noop_without_persistence() {
        // A1: without a persistence backend, archiving discarded is a safe
        // no-op (facts were already extracted inside the compressor).
        let manager = MemoryManager::new();
        let discarded = vec![oneai_core::Message::user("old turn")];
        manager.archive_discarded_snapshot("sess", discarded).await.unwrap();
        // No panic, no error — that's the contract.
    }

    #[tokio::test]
    async fn test_reflect_no_engine() {
        let manager = MemoryManager::new();
        // No reflection engine — should return None.
        let conv = oneai_core::Conversation::new();
        let result = manager.reflect("sess_test", &conv).await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_config_default() {
        let config = MemoryManagerConfig::default();
        assert_eq!(config.compression_threshold_tokens, 4000);
        assert_eq!(config.compression_keep_recent_turns, 6);
    }

    // Suppress "unused" for the re-exported symbol carried for API compat.
    #[test]
    fn _recall_strategy_reexport() {
        let _ = RecallStrategy::default();
    }

    // Keep MemoryEntry in the test namespace's import graph exercised.
    #[test]
    fn _memory_entry_still_constructable() {
        let _ = MemoryEntry {
            id: "x".to_string(),
            content: "x".to_string(),
            timestamp: chrono::Utc::now(),
            embedding: None,
            metadata: HashMap::new(),
        };
    }
}
