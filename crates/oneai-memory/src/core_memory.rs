//! Core memory — the always-in-context tier (Letta-style "core memory").
//!
//! `CoreMemory` wraps a [`MemoryFactStore`] with a token budget. It holds the
//! small set of curated facts the agent always sees, injected each turn by
//! `CoreMemorySource` (P4) and protected from compression. The agent curates
//! it directly via self-managed memory tools (`core_memory_append/replace`,
//! P5) — the "越用越好用" engine.
//!
//! When the budget is exceeded, the oldest-updated non-essential facts are
//! evicted to archival (the caller receives evicted facts to archive).

use oneai_core::MemoryFact;

use crate::fact_store::{MemoryFactStore, UpsertOutcome};

/// The always-in-context memory tier.
pub struct CoreMemory {
    store: MemoryFactStore,
    budget_tokens: usize,
}

impl CoreMemory {
    /// Create a core memory with the given token budget.
    pub fn new(budget_tokens: usize) -> Self {
        Self {
            store: MemoryFactStore::new(),
            budget_tokens,
        }
    }

    /// The configured token budget.
    pub fn budget_tokens(&self) -> usize {
        self.budget_tokens
    }

    /// Conflict-resolved upsert (delegates to the underlying store).
    pub async fn upsert(&self, fact: MemoryFact) -> UpsertOutcome {
        self.store.upsert(fact).await
    }

    /// Pin a fact's conflict key so it survives budget eviction. The pin
    /// flag lives on the fact itself (`MemoryFact.pinned`), so it travels
    /// with the fact through serialization and SQLite round-trips — not in a
    /// process-local set that's lost on restart.
    pub async fn pin(&self, user_id: &str, subject: &str, predicate: &str) {
        self.store
            .set_pinned(user_id, subject, predicate, true)
            .await;
    }

    /// Remove a fact by conflict key.
    pub async fn remove(&self, user_id: &str, subject: &str, predicate: &str) -> bool {
        self.store.remove(user_id, subject, predicate).await
    }

    /// Snapshot of all core facts.
    pub async fn facts(&self) -> Vec<MemoryFact> {
        self.store.all().await
    }

    /// Estimated token usage of the current core block (rough: ~1 token / 4 chars).
    pub async fn estimated_tokens(&self) -> usize {
        self.facts()
            .await
            .iter()
            .map(|f| f.content.len() / 4 + 40)
            .sum()
    }

    /// Enforce the token budget, evicting oldest-updated non-pinned facts.
    ///
    /// Returns the evicted facts so the caller can archive them (closing the
    /// core→archival paging loop). Pinned facts are never evicted.
    pub async fn enforce_budget(&self) -> Vec<MemoryFact> {
        let mut evicted = Vec::new();

        while self.estimated_tokens().await > self.budget_tokens {
            let mut facts = self.facts().await;
            // Evict the least core-worthy non-pinned fact first: lowest
            // `importance`, tiebroken by oldest `updated_at`. This keeps the
            // highest-salience facts (identity, constraints, decisions)
            // resident within the budget, rather than the LRU victim which
            // could be an old-but-critical identity fact.
            facts.retain(|f| !f.pinned);
            if facts.is_empty() {
                break; // only pinned facts left and still over budget — keep them.
            }
            facts.sort_by(core_rank_cmp);
            let victim = facts.pop().unwrap();
            self.store
                .remove(&victim.user_id, &victim.subject, &victim.predicate)
                .await;
            evicted.push(victim);
        }
        evicted
    }

    /// Evict non-pinned facts whose **effective salience**
    /// (`importance * temporal_score(updated_at, now, half_life)`) falls below
    /// `min_salience` — the Phase 2.4 importance-threshold decay pass (gap
    /// P2 #16). Unlike `enforce_budget` (LRU on token overflow), this evicts
    /// by salience even when under budget, so low-salience noise doesn't squat
    /// in the always-in-context block. Pinned facts are never evicted.
    /// Returns the evicted facts so the caller can page them to the archive.
    pub async fn evict_below_salience(
        &self,
        min_salience: f32,
        half_life_secs: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<MemoryFact> {
        let facts = self.facts().await;
        let mut victims: Vec<MemoryFact> = facts
            .into_iter()
            .filter(|f| {
                !f.pinned && !f.superseded && {
                    let score = f.importance
                        * crate::fact_store::temporal_score_fact(
                            &f.updated_at,
                            &now,
                            half_life_secs,
                        );
                    score < min_salience
                }
            })
            .collect();
        for v in &victims {
            self.store
                .remove(&v.user_id, &v.subject, &v.predicate)
                .await;
        }
        victims.sort_by(|a, b| {
            // Evict lowest-salience first (stable-ish order for the report).
            let sa = a.importance
                * crate::fact_store::temporal_score_fact(&a.updated_at, &now, half_life_secs);
            let sb = b.importance
                * crate::fact_store::temporal_score_fact(&b.updated_at, &now, half_life_secs);
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        });
        victims
    }

    /// Render the core memory as a labeled, injection-ready block.
    ///
    /// Format:
    /// ```text
    /// [Core Memory]
    /// - <subject> <predicate>: <content>
    /// ...
    /// ```
    pub async fn render(&self) -> String {
        let mut facts = self.facts().await;
        if facts.is_empty() {
            return String::new();
        }
        // Deterministic, most-core-worthy-first ordering (see `core_rank_cmp`)
        // so the block is byte-stable across turns (prompt-prefix caching) and
        // the model sees the highest-salience facts first.
        facts.sort_by(core_rank_cmp);
        let mut out = String::from("[Core Memory]\n");
        for f in &facts {
            out.push_str(&format!("- {} {}: {}\n", f.subject, f.predicate, f.content));
        }
        out
    }
}

/// Rank facts for core residency, most core-worthy first: higher `importance`
/// (the explicit salience signal), then fresher `updated_at`, then a
/// deterministic `subject`/`predicate` tiebreak so the rendered `[Core Memory]`
/// block is byte-stable for prompt-prefix caching.
fn core_rank_cmp(a: &MemoryFact, b: &MemoryFact) -> std::cmp::Ordering {
    b.importance
        .partial_cmp(&a.importance)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| b.updated_at.cmp(&a.updated_at))
        .then_with(|| a.subject.cmp(&b.subject))
        .then_with(|| a.predicate.cmp(&b.predicate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::FactType;
    use std::collections::HashMap;

    fn fact(subject: &str, content: &str, updated: chrono::DateTime<chrono::Utc>) -> MemoryFact {
        MemoryFact {
            id: format!("a_{}_{}", subject, content),
            user_id: "alice".to_string(),
            session_id: "s1".to_string(),
            fact_type: FactType::new("user_tooling_pref"),
            subject: subject.to_string(),
            predicate: "prefers".to_string(),
            content: content.to_string(),
            embedding: None,
            metadata: HashMap::new(),
            importance: 0.5,
            created_at: updated,
            updated_at: updated,
            version: 1,
            superseded: false,
            superseded_at: None,
            pinned: false,
        }
    }

    /// Like `fact` but with a set importance + a far-past timestamp so the
    /// temporal decay makes its effective salience near `importance * ~0`.
    fn fact_with_importance(
        subject: &str,
        content: &str,
        importance: f32,
        updated: chrono::DateTime<chrono::Utc>,
    ) -> MemoryFact {
        let mut f = fact(subject, content, updated);
        f.importance = importance;
        f
    }

    #[tokio::test]
    async fn render_empty_when_no_facts() {
        let cm = CoreMemory::new(2048);
        assert_eq!(cm.render().await, "");
    }

    #[tokio::test]
    async fn render_lists_facts() {
        let cm = CoreMemory::new(2048);
        cm.upsert(fact("user.pm", "pnpm", chrono::Utc::now())).await;
        let rendered = cm.render().await;
        assert!(rendered.contains("[Core Memory]"));
        assert!(rendered.contains("user.pm prefers: pnpm"));
    }

    #[tokio::test]
    async fn enforce_budget_evicts_oldest_and_returns_them() {
        // Tiny budget so a couple facts overflow it.
        let cm = CoreMemory::new(30);
        let old = chrono::Utc::now() - chrono::Duration::seconds(60);
        let newer = chrono::Utc::now();
        cm.upsert(fact("user.pm", "pnpm", old)).await;
        cm.upsert(fact("user.runner", "vitest", newer)).await;

        let evicted = cm.enforce_budget().await;
        assert!(!evicted.is_empty());
        // Equal importance (both default 0.5) → tiebreak by oldest updated_at.
        assert!(evicted.iter().any(|f| f.subject == "user.pm"));
        // Core is now within budget (or only pinned facts remain).
        assert!(cm.estimated_tokens().await <= cm.budget_tokens() || cm.facts().await.is_empty());
    }

    #[tokio::test]
    async fn enforce_budget_evicts_lowest_importance_first() {
        // Core residency is importance-primary, not LRU: a high-importance old
        // fact survives over a low-importance recent fact.
        let cm = CoreMemory::new(60);
        let old = chrono::Utc::now() - chrono::Duration::seconds(60);
        let recent = chrono::Utc::now();
        let mut high_old = fact("user.name", "Alice", old);
        high_old.importance = 0.9;
        let mut low_recent = fact("trivia", "noise", recent);
        low_recent.importance = 0.1;
        cm.upsert(high_old).await;
        cm.upsert(low_recent).await;

        let evicted = cm.enforce_budget().await;
        assert!(!evicted.is_empty());
        assert!(evicted.iter().any(|f| f.subject == "trivia"));
        let remaining: Vec<_> = cm.facts().await.into_iter().map(|f| f.subject).collect();
        assert!(
            remaining.contains(&"user.name".to_string()),
            "high-importance old fact must survive over low-importance recent fact: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn pinned_facts_survive_eviction() {
        let cm = CoreMemory::new(30);
        let old = chrono::Utc::now() - chrono::Duration::seconds(60);
        cm.upsert(fact("user.pm", "pnpm", old)).await;
        cm.pin("alice", "user.pm", "prefers").await;
        cm.upsert(fact("user.runner", "vitest", chrono::Utc::now()))
            .await;

        let evicted = cm.enforce_budget().await;
        // Pinned user.pm must not be evicted even though it's oldest.
        assert!(!evicted.iter().any(|f| f.subject == "user.pm"));
        let remaining: Vec<_> = cm.facts().await.into_iter().map(|f| f.subject).collect();
        assert!(remaining.contains(&"user.pm".to_string()));
    }

    #[tokio::test]
    async fn evict_below_salience_drops_low_keeps_pinned_high() {
        // Large budget — this test is about salience, not token overflow.
        let cm = CoreMemory::new(8192);
        let now = chrono::Utc::now();
        // 30 days ago → with 7-day half-life, temporal_score ≈ 0.5^(30/7) ≈ 0.052.
        let stale = now - chrono::Duration::days(30);
        // A low-importance stale fact → effective ≈ 0.1 * 0.052 ≈ 0.005 (below 0.05).
        cm.upsert(fact_with_importance("low", "noise", 0.1, stale))
            .await;
        // A high-importance stale fact → effective ≈ 0.9 * 0.052 ≈ 0.047 (still
        // below 0.05, but we pin it to prove pinning overrides salience).
        let high = fact_with_importance("high", "core", 0.9, stale);
        cm.upsert(high.clone()).await;
        cm.pin("alice", "high", "prefers").await;
        // A fresh high-importance fact → effective ≈ 0.9 * 1.0 = 0.9 (kept).
        cm.upsert(fact_with_importance("fresh", "recent", 0.9, now))
            .await;

        let evicted = cm.evict_below_salience(0.05, 7 * 24 * 3600, now).await;
        // Only the low-salience non-pinned fact is evicted.
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].subject, "low");
        let remaining: Vec<_> = cm.facts().await.into_iter().map(|f| f.subject).collect();
        assert!(remaining.contains(&"high".to_string()), "pinned survives");
        assert!(
            remaining.contains(&"fresh".to_string()),
            "fresh high survives"
        );
    }

    #[tokio::test]
    async fn upsert_conflict_updates_in_place() {
        let cm = CoreMemory::new(2048);
        cm.upsert(fact("user.pm", "npm", chrono::Utc::now())).await;
        let out = cm.upsert(fact("user.pm", "pnpm", chrono::Utc::now())).await;
        assert_eq!(
            out,
            UpsertOutcome::Updated {
                previous_version: 1
            }
        );
        assert_eq!(cm.facts().await.len(), 1);
        assert_eq!(cm.facts().await[0].content, "pnpm");
    }
}
