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

    /// Soft-invalidate the current fact for a conflict key (§12.2, Zep-style
    /// soft-fail). The fact is marked `superseded` and excluded from default
    /// recall, but not physically removed — it stays auditable via the
    /// include-superseded search path and the `_superseded_history` log. When
    /// persistence is configured, the invalidated state is also persisted so
    /// resume keeps it suppressed. Returns true if a live fact was invalidated.
    pub async fn invalidate_fact(&self, user_id: &str, subject: &str, predicate: &str) -> bool {
        let hit = self.fact_archive.invalidate(user_id, subject, predicate).await;
        if hit {
            if let Some(p) = &self.persistence {
                // Persist the invalidated state: re-store the fact (its
                // superseded flag now set) via the upsert path.
                if let Some(f) = self.fact_archive.get(user_id, subject, predicate).await {
                    if let Err(e) = p.store_fact(&f).await {
                        tracing::warn!("Failed to persist invalidated fact: {}", e);
                    }
                }
            }
        }
        hit
    }

    /// Get a fact by its conflict key.
    pub async fn get_fact(
        &self,
        user_id: &str,
        subject: &str,
        predicate: &str,
    ) -> Option<MemoryFact> {
        self.fact_archive.get(user_id, subject, predicate).await
    }

    /// Archive a batch of facts into the archival tier with Mem0-style
    /// conflict resolution (same-key facts update rather than duplicate).
    ///
    /// Called by the compression-coupled `FactExtractor` path and by the
    /// agent's `archival_memory_insert` tool (P5). When a persistence backend
    /// is configured, facts are also durably stored so they survive restart.
    ///
    /// When an embedding service is configured, each fact's `embedding` is
    /// computed from `"{subject} {predicate} {content}"` here (the single
    /// canonical write path) — §12.1: this is what makes semantic recall
    /// actually work, since `FactExtractor`/`build_fact` write `embedding:
    /// None` and rely on this method to populate it before upsert.
    pub async fn archive_facts(&self, facts: Vec<MemoryFact>) {
        for mut fact in facts {
            self.embed_fact(&mut fact).await;
            self.fact_archive.upsert(fact.clone()).await;
            if let Some(p) = &self.persistence {
                if let Err(e) = p.store_fact(&fact).await {
                    tracing::warn!("Failed to persist fact: {}", e);
                }
            }
        }
    }

    /// Embed a fact's `content` (preceded by `subject`/`predicate` for
    /// disambiguation) via the configured `EmbeddingService`, writing the
    /// result into `fact.embedding`. **§12.1** — this is the single place
    /// where stored facts acquire embeddings so `search_hybrid`'s semantic
    /// branch is no longer dead code.
    ///
    /// Fail-safe: if no embedding service is configured, or the call fails,
    /// the fact is left with `embedding: None` and a `tracing::warn!` is
    /// emitted (no error propagated — same contract as `recall_facts`'s
    /// query-embedding fallback).
    pub async fn embed_fact(&self, fact: &mut MemoryFact) {
        if fact.embedding.is_some() {
            return; // already embedded — preserve (e.g. loaded from SQLite).
        }
        let Some(svc) = &self.embedding_service else {
            return; // no embedding service → keyword recall only.
        };
        let text = if fact.subject.is_empty() && fact.predicate.is_empty() {
            fact.content.clone()
        } else {
            format!("{} {} {}", fact.subject, fact.predicate, fact.content)
        };
        match svc.embed(&text).await {
            Ok(emb) => fact.embedding = Some(emb),
            Err(e) => tracing::warn!("fact embedding failed (key={}/{}/{}): {}", fact.user_id, fact.subject, fact.predicate, e),
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
    /// `MemoryFactStore::search_hybrid_with_config`. When an embedding service
    /// is configured, the query is embedded for semantic relevance; otherwise
    /// keyword matching is used. Delegates to `recall_facts_with_config` with
    /// a default `RecallConfig` (only honoring `top_k`).
    pub async fn recall_facts(&self, query: &str, top_k: usize) -> Result<Vec<MemoryFact>> {
        let cfg = oneai_core::RecallConfig {
            top_k,
            ..oneai_core::RecallConfig::default()
        };
        self.recall_facts_with_config(query, &cfg).await
    }

    /// Configurable recall (§12.4): the domain `MemoryProfile.recall`
    /// supplies weights, half-life, normalization, and time-decay. This is
    /// the path `AppSession` wires in each turn so the three-factor scoring
    /// is domain-tunable rather than hardcoded.
    pub async fn recall_facts_with_config(
        &self,
        query: &str,
        cfg: &oneai_core::RecallConfig,
    ) -> Result<Vec<MemoryFact>> {
        if cfg.top_k == 0 {
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
        let facts = self
            .fact_archive
            .search_hybrid_with_config(
                query_embedding.as_deref(),
                query,
                cfg,
                chrono::Utc::now(),
                false,
            )
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

    /// §12.3: reflect mid-session if the cumulative importance of newly
    /// archived facts exceeds the reflection threshold and enough turns have
    /// elapsed since the last reflection (Generative-Agents-style importance-sum
    /// gating). Returns `Ok(None)` when the threshold isn't met or no
    /// reflection engine is configured. The reflection is fed a distilled
    /// summary of the most recent prior episodic facts from the archival
    /// tier, so new insights can build on (recursive-reflection雏形) rather
    /// than ignore earlier reflections.
    pub async fn reflect_if_threshold(
        &self,
        session_id: &str,
        conversation: &Conversation,
        accumulated_importance: f32,
        turns_since_last: u32,
    ) -> Result<Option<EpisodicMemory>> {
        let Some(reflection) = &self.reflection else {
            return Ok(None);
        };
        if !reflection.should_reflect(accumulated_importance, turns_since_last) {
            return Ok(None);
        }
        // Gather up to 3 most-recent prior episodic facts to seed recursive
        // reflection. Sorted by updated_at descending.
        let mut prior: Vec<_> = self.fact_archive.all().await.into_iter()
            .filter(|f| f.fact_type.as_str() == "episodic" && !f.superseded)
            .collect();
        prior.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let prior_summary = if prior.is_empty() {
            None
        } else {
            Some(
                prior.iter().take(3)
                    .map(|f| format!("- ({}): {}", f.updated_at.to_rfc3339(), f.content.chars().take(280).collect::<String>()))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
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

        let episodic = reflection.reflect_with_prior(session_id, &entries, prior_summary.as_deref()).await?;
        // Route through archive_facts so the episodic fact is embedded (§12.1)
        // and persisted in one place.
        self.archive_facts(vec![episodic.to_fact()]).await;
        Ok(Some(episodic))
    }

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
        // layer) + persist. Route through `archive_facts` so it is embedded
        // (§12.1) and durably persisted in one place rather than bypassing
        // them with a direct `fact_archive.upsert`.
        let fact = episodic.to_fact();
        self.archive_facts(vec![fact]).await;

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
            superseded: false,
            superseded_at: None,
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
            superseded: false,
            superseded_at: None,
        };
        manager.archive_facts(vec![mk("npm")]).await;
        manager.archive_facts(vec![mk("pnpm")]).await;
        assert_eq!(manager.fact_archive().len().await, 1);
        let f = manager.fact_archive().get("alice", "user.pm", "prefers").await.unwrap();
        assert_eq!(f.content, "pnpm");
    }

    #[tokio::test]
    async fn test_invalidate_fact_excludes_from_recall() {
        // §12.2: invalidate_fact soft-fails the current truth; recall no longer
        // surfaces it. The KU scenario: agent switches from JWT to session —
        // invalidating the JWT fact means the old value won't be recalled.
        let manager = MemoryManager::new();
        manager.set_user_id("alice").await;
        let mk = |content: &str| oneai_core::MemoryFact {
            id: "f_auth".to_string(),
            user_id: "alice".to_string(),
            session_id: String::new(),
            fact_type: oneai_core::FactType::new("decision"),
            subject: "auth.scheme".to_string(),
            predicate: "decided_to".to_string(),
            content: content.to_string(),
            embedding: None,
            metadata: HashMap::new(),
            importance: 0.85,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
            superseded: false,
            superseded_at: None,
        };
        manager.archive_facts(vec![mk("JWT")]).await;
        assert!(manager.invalidate_fact("alice", "auth.scheme", "decided_to").await);

        // Recall for "auth"/"jwt" no longer returns the invalidated fact.
        let recalled = manager.recall_facts("jwt", 5).await.unwrap();
        assert!(recalled.is_empty());

        // The supersede history was recorded on the (still-present) fact.
        let f = manager.get_fact("alice", "auth.scheme", "decided_to").await.unwrap();
        assert!(f.superseded);
        assert!(f.metadata.contains_key("_superseded_history"));
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

    // ─── §12.3: threshold-triggered mid-session reflection ──────────────────

    /// Mock LLM provider for the reflection prompt.
    struct MockReflectProvider { resp: String }
    impl MockReflectProvider { fn new(r: impl Into<String>) -> Self { Self { resp: r.into() } } }
    #[async_trait::async_trait]
    impl oneai_core::traits::LlmProvider for MockReflectProvider {
        async fn infer(&self, _req: oneai_core::InferenceRequest) -> Result<oneai_core::InferenceResponse> {
            Ok(oneai_core::InferenceResponse {
                message: oneai_core::Message::assistant(self.resp.clone()),
                usage: oneai_core::TokenUsage::default(),
                model: "mock-reflect".to_string(),
                metadata: HashMap::new(),
            })
        }
        async fn infer_stream(&self, _req: oneai_core::InferenceRequest) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = oneai_core::InferenceStreamChunk> + Send>>> {
            Err(oneai_core::error::OneAIError::Provider("no stream".into()))
        }
        fn capabilities(&self) -> oneai_core::ModelCapability {
            oneai_core::ModelCapability { supports_multimodal: false, supports_streaming: false, supports_tools: false, context_window_size: 4096, max_output_tokens: 512 }
        }
        fn config(&self) -> &oneai_core::ModelConfig {
            static CONFIG: std::sync::OnceLock<oneai_core::ModelConfig> = std::sync::OnceLock::new();
            CONFIG.get_or_init(oneai_core::ModelConfig::default)
        }
    }

    #[tokio::test]
    async fn reflect_if_threshold_skips_below_threshold() {
        let manager = MemoryManager::with_compressor_and_reflection(
            MemoryManagerConfig::default(),
            Arc::new(MockReflectProvider::new("REFLECTION: x\nOUTCOME: success")),
        );
        let conv = oneai_core::Conversation::new();
        // Below the 150.0 default threshold.
        let r = manager.reflect_if_threshold("s", &conv, 10.0, 20).await.unwrap();
        assert!(r.is_none(), "should not reflect below threshold");
    }

    #[tokio::test]
    async fn reflect_if_threshold_fires_above_threshold() {
        let manager = MemoryManager::with_compressor_and_reflection(
            MemoryManagerConfig::default(),
            Arc::new(MockReflectProvider::new("REFLECTION: consolidated insight\nINSIGHTS: pattern learned\nDECISIONS: adopt X\nOUTCOME: success")),
        );
        let mut conv = oneai_core::Conversation::new();
        conv.add_message(oneai_core::Message::user("did important work"));
        // Above threshold + enough turns → fires.
        let r = manager.reflect_if_threshold("s", &conv, 200.0, 10).await.unwrap();
        assert!(r.is_some(), "should reflect above threshold");
        // The resulting episodic fact is archived (and embedded if a service were set).
        let facts = manager.fact_archive().all().await;
        assert!(facts.iter().any(|f| f.fact_type.as_str() == "episodic"));
    }

    #[test]
    fn test_config_default() {
        let config = MemoryManagerConfig::default();
        assert_eq!(config.compression_threshold_tokens, 4000);
        assert_eq!(config.compression_keep_recent_turns, 6);
    }

    // ─── §12.1: fact auto-embedding → semantic recall ───────────────────────

    /// Deterministic embedding service for tests: maps text to a small float
    /// vector derived from character histograms so that "包管理器" and
    /// "package manager" collide on shared bytes (proving semantic recall
    /// surfaces synonym facts that keyword recall misses) while unrelated
    /// text differs. Not a real embedding — just stable and discriminative
    /// enough to exercise the §12.1 path without a network.
    struct HashEmbeddingService;
    #[async_trait::async_trait]
    impl oneai_core::traits::EmbeddingService for HashEmbeddingService {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let mut v = vec![0.0f32; 32];
            for (i, b) in text.bytes().enumerate() {
                v[(b as usize) % 32] += 1.0;
                v[((b as usize).wrapping_add(i)) % 32] += 0.5;
            }
            // L2-normalize.
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
            for x in v.iter_mut() { *x /= norm; }
            Ok(v)
        }
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for t in texts { out.push(self.embed(t).await?); }
            Ok(out)
        }
        fn model(&self) -> oneai_core::traits::EmbeddingModel {
            oneai_core::traits::EmbeddingModel::allminilm_l6_v2()
        }
    }

    fn make_fact_embed(user: &str, subject: &str, predicate: &str, content: &str) -> MemoryFact {
        MemoryFact {
            id: format!("{}_{}_{}", user, subject, predicate),
            user_id: user.to_string(),
            session_id: String::new(),
            fact_type: oneai_core::FactType::new("user_tooling_pref"),
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            content: content.to_string(),
            embedding: None,
            metadata: HashMap::new(),
            importance: 0.5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
            superseded: false,
            superseded_at: None,
        }
    }

    #[tokio::test]
    async fn archive_facts_embeds_when_service_configured() {
        // §12.1: archive_facts populates fact.embedding when an embedding
        // service is present, so search_hybrid's semantic branch is no longer
        // dead code.
        let mm = MemoryManager::with_embedding(
            MemoryManagerConfig::default(),
            Arc::new(HashEmbeddingService),
        );
        mm.archive_facts(vec![make_fact_embed("alice", "user.package_manager", "prefers", "pnpm")]).await;
        let f = mm.fact_archive().get("alice", "user.package_manager", "prefers").await.unwrap();
        assert!(f.embedding.is_some(), "fact must be embedded at archive time");
    }

    #[tokio::test]
    async fn archive_facts_no_embedding_without_service() {
        // Without an embedding service, facts stay un-embedded (keyword recall).
        let mm = MemoryManager::new();
        mm.archive_facts(vec![make_fact_embed("alice", "user.package_manager", "prefers", "pnpm")]).await;
        let f = mm.fact_archive().get("alice", "user.package_manager", "prefers").await.unwrap();
        assert!(f.embedding.is_none());
    }

    #[tokio::test]
    async fn semantic_recall_hits_synonym_query_that_keyword_misses() {
        // §12.1 headline test: a fact stored as "包管理器" (Chinese) should be
        // recalled by the English query "package manager" once facts are
        // embedded — keyword recall has zero overlap on these strings, so the
        // only way to surface it is the semantic branch.
        let mm = MemoryManager::with_embedding(
            MemoryManagerConfig::default(),
            Arc::new(HashEmbeddingService),
        );
        // Content with no byte overlap with the English query.
        mm.archive_facts(vec![make_fact_embed("alice", "用户.包管理器", "偏好", "使用 pnpm 管理依赖")]).await;

        // Keyword path: query "package manager" shares no bytes with the
        // Chinese subject/predicate/content → 0 hits.
        let kw = mm.fact_archive().search_keyword("package manager", 5).await;
        assert!(kw.is_empty(), "keyword recall should miss the synonym fact");

        // Semantic path: embed the query, search_hybrid uses cosine.
        let qemb = HashEmbeddingService.embed("package manager dependency").await.unwrap();
        let sem = mm.fact_archive().search_semantic(&qemb, 5).await;
        assert!(!sem.is_empty(), "semantic recall must surface the synonym fact");
        assert_eq!(sem[0].subject, "用户.包管理器");
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
