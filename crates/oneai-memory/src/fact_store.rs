//! MemoryFactStore — the canonical container for atomic `MemoryFact`s.
//!
//! Both the **core** tier (always-in-context, budgeted) and the **archival**
//! tier (full fact base, recalled on demand) are instances of this store.
//! Distinct from the legacy `LongTermMemory` (which is `MemoryEntry`-based):
//! `MemoryFactStore` holds conflict-resolved atomic facts and is what the
//! unified `memories` persistence table (P5) serializes.
//!
//! Conflict resolution follows the Mem0 invariant: two facts sharing the
//! `(user_id, subject, predicate)` key are the *same fact*; a new value
//! **updates** the existing one (bumping `version` and `updated_at`) rather
//! than appending a duplicate. This keeps long-term memory from drifting into
//! contradiction as the agent accumulates facts across sessions.

use std::collections::HashMap;

use oneai_core::MemoryFact;
use tokio::sync::RwLock;

/// Outcome of a conflict-resolved upsert.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum UpsertOutcome {
    /// A new fact was inserted (no prior fact with the same key).
    Inserted,
    /// An existing fact was updated. Carries the previous version number.
    Updated { previous_version: u32 },
}

/// An in-memory store of `MemoryFact`s with Mem0-style conflict resolution.
///
/// Thread-safe via a `tokio::sync::RwLock`. Search is brute-force cosine
/// similarity (over embeddings) plus keyword matching — acceptable for the
/// <10K-entry scale OneAI targets; P5's SQLite backend may later accelerate it.
pub struct MemoryFactStore {
    facts: RwLock<HashMap<String, MemoryFact>>,
    /// Index: (user_id, subject, predicate) -> fact id, for O(1) conflict lookup.
    key_index: RwLock<HashMap<(String, String, String), String>>,
}

impl MemoryFactStore {
    /// Create an empty fact store.
    pub fn new() -> Self {
        Self {
            facts: RwLock::new(HashMap::new()),
            key_index: RwLock::new(HashMap::new()),
        }
    }

    /// Number of stored facts.
    pub async fn len(&self) -> usize {
        self.facts.read().await.len()
    }

    /// Whether the store is empty.
    pub async fn is_empty(&self) -> bool {
        self.facts.read().await.is_empty()
    }

    /// Conflict-resolved upsert of a fact.
    ///
    /// If a fact with the same `(user_id, subject, predicate)` key exists, its
    /// `content`/`embedding`/`metadata` are replaced, `version` is bumped, and
    /// `updated_at` is refreshed — returning `Updated`. Otherwise the fact is
    /// inserted — returning `Inserted`. The fact's `id`/`created_at`/`version`
    /// are normalized on insert.
    pub async fn upsert(&self, mut fact: MemoryFact) -> UpsertOutcome {
        let key = (
            fact.user_id.clone(),
            fact.subject.clone(),
            fact.predicate.clone(),
        );

        // Check for an existing fact with the same conflict key.
        let existing_id = self.key_index.read().await.get(&key).cloned();
        if let Some(id) = existing_id {
            let mut facts = self.facts.write().await;
            if let Some(prev) = facts.get_mut(&id) {
                let previous_version = prev.version;
                // Preserve identity and origin timestamps; update mutable fields.
                prev.content = std::mem::take(&mut fact.content);
                prev.embedding = fact.embedding.take();
                prev.metadata = fact.metadata;
                prev.fact_type = fact.fact_type;
                prev.updated_at = fact.updated_at;
                prev.version = previous_version.saturating_add(1);
                return UpsertOutcome::Updated { previous_version };
            }
        }

        // Fresh insert: normalize version if unset-equivalent.
        if fact.version == 0 {
            fact.version = 1;
        }
        let id = fact.id.clone();
        self.facts.write().await.insert(id.clone(), fact);
        self.key_index.write().await.insert(key, id);
        UpsertOutcome::Inserted
    }

    /// Remove a fact by its conflict key. Returns true if a fact was removed.
    pub async fn remove(&self, user_id: &str, subject: &str, predicate: &str) -> bool {
        let key = (user_id.to_string(), subject.to_string(), predicate.to_string());
        let id = self.key_index.write().await.remove(&key);
        if let Some(id) = id {
            self.facts.write().await.remove(&id).is_some()
        } else {
            false
        }
    }

    /// Snapshot all facts (cloned).
    pub async fn all(&self) -> Vec<MemoryFact> {
        self.facts.read().await.values().cloned().collect()
    }

    /// Get a fact by its conflict key.
    pub async fn get(&self, user_id: &str, subject: &str, predicate: &str) -> Option<MemoryFact> {
        let key = (user_id.to_string(), subject.to_string(), predicate.to_string());
        let id = self.key_index.read().await.get(&key).cloned()?;
        self.facts.read().await.get(&id).cloned()
    }

    /// Semantic search (cosine similarity over embeddings), top_k results.
    pub async fn search_semantic(&self, query_embedding: &[f32], top_k: usize) -> Vec<MemoryFact> {
        let facts = self.facts.read().await;
        let mut scored: Vec<(f32, MemoryFact)> = facts
            .values()
            .filter_map(|f| f.embedding.as_ref().map(|emb| (cosine(query_embedding, emb), f.clone())))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(_, f)| f).take(top_k).collect()
    }

    /// Keyword search over fact content/subject/predicate, top_k results.
    pub async fn search_keyword(&self, query: &str, top_k: usize) -> Vec<MemoryFact> {
        let facts = self.facts.read().await;
        let mut results: Vec<MemoryFact> = facts
            .values()
            .filter(|f| {
                oneai_core::keyword_matches(&f.content, query)
                    || oneai_core::keyword_matches(&f.subject, query)
                    || oneai_core::keyword_matches(&f.predicate, query)
            })
            .cloned()
            .collect();
        // Most-recently-updated first.
        results.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        results.truncate(top_k);
        results
    }
}

impl Default for MemoryFactStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Cosine similarity between two vectors (returns 0.0 if lengths mismatch).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|y| y * y).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::FactType;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    fn make_fact(user: &str, subject: &str, predicate: &str, content: &str) -> MemoryFact {
        MemoryFact {
            id: format!("{}_{}_{}", user, subject, predicate),
            user_id: user.to_string(),
            session_id: "s1".to_string(),
            fact_type: FactType::new("user_tooling_pref"),
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            content: content.to_string(),
            embedding: None,
            metadata: HashMap::new(),
            created_at: now(),
            updated_at: now(),
            version: 1,
        }
    }

    #[tokio::test]
    async fn upsert_inserts_new_fact() {
        let store = MemoryFactStore::new();
        let out = store.upsert(make_fact("alice", "user.package_manager", "prefers", "npm")).await;
        assert_eq!(out, UpsertOutcome::Inserted);
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn upsert_updates_on_conflict_not_append() {
        // The Mem0 invariant: a contradicting fact updates rather than duplicates.
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "user.package_manager", "prefers", "npm")).await;
        let out = store.upsert(make_fact("alice", "user.package_manager", "prefers", "pnpm")).await;
        assert_eq!(out, UpsertOutcome::Updated { previous_version: 1 });

        // Still one fact — not two.
        assert_eq!(store.len().await, 1);
        // And its content is the new value, version bumped.
        let f = store.get("alice", "user.package_manager", "prefers").await.unwrap();
        assert_eq!(f.content, "pnpm");
        assert_eq!(f.version, 2);
    }

    #[tokio::test]
    async fn different_subjects_are_distinct_facts() {
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "user.package_manager", "prefers", "pnpm")).await;
        store.upsert(make_fact("alice", "user.test_runner", "prefers", "vitest")).await;
        assert_eq!(store.len().await, 2);
    }

    #[tokio::test]
    async fn different_users_are_distinct() {
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "user.package_manager", "prefers", "pnpm")).await;
        store.upsert(make_fact("bob", "user.package_manager", "prefers", "npm")).await;
        assert_eq!(store.len().await, 2);
    }

    #[tokio::test]
    async fn remove_by_conflict_key() {
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "user.package_manager", "prefers", "pnpm")).await;
        assert!(store.remove("alice", "user.package_manager", "prefers").await);
        assert_eq!(store.len().await, 0);
        assert!(!store.remove("alice", "user.package_manager", "prefers").await);
    }

    #[tokio::test]
    async fn keyword_search_matches_content() {
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "user.package_manager", "prefers", "pnpm")).await;
        store.upsert(make_fact("alice", "user.test_runner", "prefers", "vitest")).await;
        let results = store.search_keyword("pnpm", 5).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "pnpm");
    }
}
