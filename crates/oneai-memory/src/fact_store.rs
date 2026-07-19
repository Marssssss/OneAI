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

use oneai_core::{MemoryFact, RecallConfig};
use tokio::sync::RwLock;

/// Metadata key under which the chain of superseded (Mem0/Zep-style) fact
/// revisions is recorded. Each conflict-resolved update appends the previous
/// `{content, embedding, fact_type, updated_at, version}` here as a JSON
/// array element, so the decision evolution is auditable even though the
/// "current truth" is the single live row (the Mem0 invariant).
pub const SUPERSEDED_HISTORY_KEY: &str = "_superseded_history";

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
    /// If a fact with the same `(user_id, subject, predicate)` key exists, the
    /// previous revision is first appended to `metadata["_superseded_history"]`
    /// (§12.2 — decision evolution stays auditable), then its
    /// `content`/`embedding`/`metadata`/`fact_type` are replaced, `version`
    /// is bumped, and `updated_at` is refreshed — returning `Updated`.
    /// Otherwise the fact is inserted — returning `Inserted`. The fact's
    /// `id`/`created_at`/`version` are normalized on insert. `superseded` is
    /// reset to `false` on update (a fresh write re-establishes the fact as
    /// the current truth).
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
                // §12.2: capture the outgoing revision BEFORE overwriting
                // mutable fields, so the decision evolution is auditable.
                let history_entry = serde_json::json!({
                    "content": prev.content.clone(),
                    "embedding": prev.embedding.clone(),
                    "fact_type": prev.fact_type.as_str().to_string(),
                    "updated_at": prev.updated_at.to_rfc3339(),
                    "version": prev.version,
                });
                // Merge the new fact's metadata in (the incoming metadata may
                // carry fresh provenance), then append the captured history
                // entry so the supersede chain survives the update.
                for (k, v) in fact.metadata.drain() {
                    if k == SUPERSEDED_HISTORY_KEY { continue; } // never clobber history
                    prev.metadata.insert(k, v);
                }
                append_history(prev, history_entry);
                // Preserve identity and origin timestamps; update mutable fields.
                prev.content = std::mem::take(&mut fact.content);
                prev.embedding = fact.embedding.take();
                prev.fact_type = fact.fact_type;
                prev.updated_at = fact.updated_at;
                prev.importance = fact.importance;
                prev.superseded = false;
                prev.superseded_at = None;
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

    /// Soft-invalidate the current fact for a conflict key (Zep-style
    /// soft-fail). The fact is NOT removed: it is marked `superseded=true`
    /// with a timestamp, excluded from default recall, but remains auditable
    /// via the include-superseded search variants and the history log.
    /// Returns true if a live (non-superseded) fact was invalidated.
    pub async fn invalidate(&self, user_id: &str, subject: &str, predicate: &str) -> bool {
        let key = (user_id.to_string(), subject.to_string(), predicate.to_string());
        let Some(id) = self.key_index.read().await.get(&key).cloned() else {
            return false;
        };
        let now = chrono::Utc::now();
        let mut facts = self.facts.write().await;
        if let Some(prev) = facts.get_mut(&id) {
            if prev.superseded {
                return false; // already invalidated
            }
            let history_entry = serde_json::json!({
                "content": prev.content.clone(),
                "embedding": prev.embedding.clone(),
                "fact_type": prev.fact_type.as_str().to_string(),
                "updated_at": prev.updated_at.to_rfc3339(),
                "version": prev.version,
                "superseded": true,
            });
            append_history(prev, history_entry);
            prev.superseded = true;
            prev.superseded_at = Some(now);
            prev.updated_at = now;
            prev.version = prev.version.saturating_add(1);
            return true;
        }
        false
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

    /// Toggle a fact's `pinned` flag in place (no version bump, no
    /// superseded-history entry — pinning is a curation signal, not a content
    /// revision). Returns `true` if a fact matched the conflict key.
    pub async fn set_pinned(
        &self,
        user_id: &str,
        subject: &str,
        predicate: &str,
        pinned: bool,
    ) -> bool {
        let key = (user_id.to_string(), subject.to_string(), predicate.to_string());
        let id = match self.key_index.read().await.get(&key).cloned() {
            Some(id) => id,
            None => return false,
        };
        if let Some(fact) = self.facts.write().await.get_mut(&id) {
            fact.pinned = pinned;
            true
        } else {
            false
        }
    }

    /// Semantic search (cosine similarity over embeddings), top_k results.
    /// Superseded facts are excluded by default.
    pub async fn search_semantic(&self, query_embedding: &[f32], top_k: usize) -> Vec<MemoryFact> {
        self.search_semantic_with(query_embedding, top_k, false).await
    }

    async fn search_semantic_with(
        &self,
        query_embedding: &[f32],
        top_k: usize,
        include_superseded: bool,
    ) -> Vec<MemoryFact> {
        let facts = self.facts.read().await;
        let mut scored: Vec<(f32, MemoryFact)> = facts
            .values()
            .filter(|f| include_superseded || !f.superseded)
            .filter_map(|f| f.embedding.as_ref().map(|emb| (cosine(query_embedding, emb), f.clone())))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(_, f)| f).take(top_k).collect()
    }

    /// Keyword search over fact content/subject/predicate, top_k results.
    /// Superseded facts are excluded by default.
    pub async fn search_keyword(&self, query: &str, top_k: usize) -> Vec<MemoryFact> {
        self.search_keyword_with(query, top_k, false).await
    }

    async fn search_keyword_with(
        &self,
        query: &str,
        top_k: usize,
        include_superseded: bool,
    ) -> Vec<MemoryFact> {
        let facts = self.facts.read().await;
        let mut results: Vec<MemoryFact> = facts
            .values()
            .filter(|f| include_superseded || !f.superseded)
            .filter(|f| {
                // Token-level match (§: no-embedding recall upgrade): a
                // natural-language query like "which package manager does the
                // user prefer" matches fact text via its "package"/"manager"
                // tokens, even though the whole sentence is never a substring
                // of short fact content.
                oneai_core::keyword_matches_any_token(&f.content, query)
                    || oneai_core::keyword_matches_any_token(&f.subject, query)
                    || oneai_core::keyword_matches_any_token(&f.predicate, query)
            })
            .cloned()
            .collect();
        // Most-recently-updated first.
        results.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        results.truncate(top_k);
        results
    }

    /// Three-factor hybrid search (Generative Agents): relevance + recency +
    /// importance, with the legacy 4-arg signature delegating to
    /// `search_hybrid_with_config` using a default `RecallConfig` and the
    /// current time. Superseded facts are excluded.
    pub async fn search_hybrid(
        &self,
        query_embedding: Option<&[f32]>,
        query_text: &str,
        top_k: usize,
        time_decay: bool,
    ) -> Vec<MemoryFact> {
        let cfg = RecallConfig {
            time_decay,
            top_k,
            ..RecallConfig::default()
        };
        self.search_hybrid_with_config(query_embedding, query_text, &cfg, chrono::Utc::now(), false)
            .await
    }

    /// Configurable three-factor hybrid search (§12.4): weights and the
    /// recency half-life come from `RecallConfig`, and (by default) the three
    /// factors are min-max normalized across the candidate set before
    /// weighting — Generative Agents requires this, otherwise cosine ∈
    /// [-1,1], importance ∈ [0,1], recency ∈ (0,1] are not comparable.
    ///
    /// `relevance` is cosine similarity when both query and fact have
    /// embeddings, else a fixed keyword-match score. `recency` is exponential
    /// decay over `updated_at` (half-life = `cfg.recency_half_life_secs`,
    /// disabled when `time_decay` is false). `importance` is the fact's
    /// salience. Candidates with `relevance <= 0` are dropped. When
    /// `include_superseded` is false, superseded facts are excluded.
    pub async fn search_hybrid_with_config(
        &self,
        query_embedding: Option<&[f32]>,
        query_text: &str,
        cfg: &RecallConfig,
        now: chrono::DateTime<chrono::Utc>,
        include_superseded: bool,
    ) -> Vec<MemoryFact> {
        let facts = self.facts.read().await;

        // Pass 1: compute raw (relevance, recency, importance) per candidate,
        // dropping zero-relevance and (optionally) superseded facts.
        let mut candidates: Vec<(f32, f32, f32, MemoryFact)> = facts
            .values()
            .filter(|f| include_superseded || !f.superseded)
            .filter_map(|f| {
                let relevance = match query_embedding {
                    Some(emb) => f
                        .embedding
                        .as_ref()
                        .map(|fe| cosine(emb, fe))
                        .unwrap_or(0.0),
                    None => {
                        // Token-level keyword match (no-embedding recall
                        // upgrade): see `search_keyword_with` for rationale.
                        if oneai_core::keyword_matches_any_token(&f.content, query_text)
                            || oneai_core::keyword_matches_any_token(&f.subject, query_text)
                            || oneai_core::keyword_matches_any_token(&f.predicate, query_text)
                        {
                            0.6
                        } else {
                            0.0
                        }
                    }
                };
                if relevance <= 0.0 {
                    return None;
                }
                let recency = if cfg.time_decay {
                    temporal_score_fact(&f.updated_at, &now, cfg.recency_half_life_secs)
                } else {
                    0.5
                };
                let importance = f.importance;
                Some((relevance, recency, importance, f.clone()))
            })
            .collect();
        drop(facts);

        // Pass 2: optional min-max normalization across the candidate set.
        let (rmin, rmax) = minmax(candidates.iter().map(|(r, _, _, _)| *r));
        let (cmin, cmax) = minmax(candidates.iter().map(|(_, c, _, _)| *c));
        let (imin, imax) = minmax(candidates.iter().map(|(_, _, i, _)| *i));
        for (r, c, i, _) in candidates.iter_mut() {
            if cfg.normalize_factors {
                *r = rescale(*r, rmin, rmax);
                *c = rescale(*c, cmin, cmax);
                *i = rescale(*i, imin, imax);
            }
        }

        // Pass 3: weighted sum, rank, top_k.
        for (r, c, i, _) in candidates.iter_mut() {
            let score = cfg.relevance_weight * *r
                + cfg.recency_weight * *c
                + cfg.importance_weight * *i;
            // Stash the final score in `relevance` (already consumed).
            *r = score;
        }
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        candidates.into_iter().map(|(_, _, _, f)| f).take(cfg.top_k).collect()
    }
}

/// Append a captured revision (a `serde_json::Value` object) to the fact's
/// `_superseded_history` metadata (§12.2). The history is a JSON array of
/// `{content, embedding, fact_type, updated_at, version}` snapshots, so the
/// decision evolution is auditable even though the "current truth" is the
/// single live row (the Mem0 invariant). Best-effort: malformed history is
/// reset to a fresh array containing just the new entry.
fn append_history(fact: &mut MemoryFact, entry: serde_json::Value) {
    let existing = fact.metadata.get(SUPERSEDED_HISTORY_KEY).cloned().unwrap_or_default();
    let mut arr: serde_json::Value = if existing.is_empty() {
        serde_json::Value::Array(Vec::new())
    } else {
        serde_json::from_str(&existing).unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
    };
    if let Some(a) = arr.as_array_mut() {
        a.push(entry);
    } else {
        // Corrupt non-array value → reset to a fresh array.
        arr = serde_json::Value::Array(vec![entry]);
    }
    fact.metadata.insert(SUPERSEDED_HISTORY_KEY.to_string(), arr.to_string());
}

/// Min and max of an iterator of f32 (empty → (0,0)).
fn minmax(it: impl Iterator<Item = f32>) -> (f32, f32) {
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for v in it {
        if v < lo { lo = v; }
        if v > hi { hi = v; }
    }
    if !lo.is_finite() { lo = 0.0; }
    if !hi.is_finite() { hi = 0.0; }
    (lo, hi)
}

/// Rescale `v` from [min,max] to [0,1]; degenerate range (min==max) → 1.0
/// (a single candidate or constant factor shouldn't be zeroed out).
fn rescale(v: f32, min: f32, max: f32) -> f32 {
    if (max - min).abs() < 1e-9 { 1.0 } else { (v - min) / (max - min) }
}

/// Exponential recency decay over a fact's `updated_at`, in `[0.0, 1.0]`.
///
/// §12.4: half-life is now configurable (was hardcoded 1 hour). Mirrors
/// `long_term::EmbeddedVectorStore::temporal_score` but operates on fact
/// timestamps (the canonical layer), so the three-factor scorer in
/// `search_hybrid_with_config` can apply Generative-Agents-style recency
/// weighting with a domain-tunable half-life.
fn temporal_score_fact(
    entry_time: &chrono::DateTime<chrono::Utc>,
    reference_time: &chrono::DateTime<chrono::Utc>,
    half_life_secs: u64,
) -> f32 {
    let diff = reference_time.timestamp() - entry_time.timestamp();
    if diff <= 0 {
        return 1.0;
    }
    let half_life = half_life_secs.max(1) as f64;
    let decay = std::cmp::min(diff, 365 * 24 * 3600) as f64;
    0.5_f64.powf(decay / half_life) as f32
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
            importance: 0.5,
            created_at: now(),
            updated_at: now(),
            version: 1,
            superseded: false,
            superseded_at: None,
            pinned: false,
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

    #[tokio::test]
    async fn search_hybrid_keyword_matches() {
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "user.package_manager", "prefers", "pnpm")).await;
        store.upsert(make_fact("alice", "user.test_runner", "prefers", "vitest")).await;
        // No query embedding → keyword relevance path.
        let results = store.search_hybrid(None, "pnpm", 5, true).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "pnpm");
    }

    #[tokio::test]
    async fn search_hybrid_ranks_higher_importance_first() {
        // Three-factor scorer: a higher-importance fact outranks a lower one
        // when relevance is comparable (both keyword-match the query).
        let store = MemoryFactStore::new();
        let mut low = make_fact("alice", "auth.module", "decided_to", "jwt");
        low.importance = 0.2;
        let mut high = make_fact("alice", "auth.scheme", "decided_to", "jwt");
        high.importance = 0.95;
        store.upsert(low).await;
        store.upsert(high).await;

        let results = store.search_hybrid(None, "jwt", 5, false).await;
        assert_eq!(results.len(), 2);
        // time_decay disabled → importance is the differentiator; high first.
        assert_eq!(results[0].subject, "auth.scheme");
        assert_eq!(results[1].subject, "auth.module");
    }

    // ─── §12.2: supersede history + soft-invalidate ─────────────────────────

    #[tokio::test]
    async fn upsert_records_superseded_history() {
        // A contradicting update must preserve the prior revision in the
        // _superseded_history metadata (auditable decision evolution).
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "auth.scheme", "decided_to", "JWT")).await;
        store.upsert(make_fact("alice", "auth.scheme", "decided_to", "session")).await;

        let f = store.get("alice", "auth.scheme", "decided_to").await.unwrap();
        assert_eq!(f.content, "session"); // current truth = new value
        assert_eq!(f.version, 2);
        let history = f.metadata.get(super::SUPERSEDED_HISTORY_KEY).expect("history recorded");
        let arr: serde_json::Value = serde_json::from_str(history).unwrap();
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["content"], "JWT");
        assert_eq!(arr[0]["version"], 1);
    }

    #[tokio::test]
    async fn invalidate_soft_fails_and_excludes_from_recall() {
        // Soft-invalidate marks superseded=true; default search excludes it.
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "auth.scheme", "decided_to", "JWT")).await;
        assert!(store.invalidate("alice", "auth.scheme", "decided_to").await);

        let f = store.get("alice", "auth.scheme", "decided_to").await.unwrap();
        assert!(f.superseded);
        assert!(f.superseded_at.is_some());

        // Default hybrid/keyword search no longer surfaces it.
        assert!(store.search_hybrid(None, "jwt", 5, true).await.is_empty());
        assert!(store.search_keyword("jwt", 5).await.is_empty());

        // But a re-upsert with a new value re-establishes the current truth.
        store.upsert(make_fact("alice", "auth.scheme", "decided_to", "OAuth")).await;
        let f = store.get("alice", "auth.scheme", "decided_to").await.unwrap();
        assert!(!f.superseded);
        assert_eq!(f.content, "OAuth");
        assert_eq!(f.version, 3); // insert(1) → invalidate(2) → update(3)
    }

    // ─── §12.4: configurable weights + normalization ───────────────────────

    #[tokio::test]
    async fn search_hybrid_with_config_respects_weights() {
        // With recency weight cranked to 1.0 and others to 0, the
        // most-recently-updated fact wins among two keyword-matching facts
        // regardless of importance.
        let store = MemoryFactStore::new();
        let mut a = make_fact("alice", "a.mod", "decided_to", "jwt");
        a.importance = 0.99;
        a.updated_at = chrono::Utc::now();
        let mut b = make_fact("alice", "b.mod", "decided_to", "jwt");
        b.importance = 0.05;
        b.updated_at = chrono::Utc::now() + chrono::Duration::seconds(60);
        store.upsert(a).await;
        store.upsert(b).await;

        let cfg = oneai_core::RecallConfig {
            strategy: oneai_core::RecallStrategy::Hybrid,
            top_k: 2,
            time_decay: true,
            relevance_weight: 0.0,
            recency_weight: 1.0,
            importance_weight: 0.0,
            recency_half_life_secs: 3600,
            normalize_factors: true,
        };
        let results = store
            .search_hybrid_with_config(None, "jwt", &cfg, chrono::Utc::now() + chrono::Duration::seconds(120), false)
            .await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].subject, "b.mod"); // more recent wins
    }

    #[tokio::test]
    async fn search_hybrid_with_config_normalization_keeps_single_candidate() {
        // A single candidate factor range is degenerate → rescaled to 1.0
        // (not zeroed out), so it still surfaces.
        let store = MemoryFactStore::new();
        store.upsert(make_fact("alice", "x.mod", "decided_to", "jwt")).await;
        let cfg = oneai_core::RecallConfig::default();
        let results = store
            .search_hybrid_with_config(None, "jwt", &cfg, chrono::Utc::now(), false)
            .await;
        assert_eq!(results.len(), 1);
    }
}
