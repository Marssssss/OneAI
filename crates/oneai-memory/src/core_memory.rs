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
use tokio::sync::RwLock;

use crate::fact_store::{MemoryFactStore, UpsertOutcome};

/// The always-in-context memory tier.
pub struct CoreMemory {
    store: MemoryFactStore,
    budget_tokens: usize,
    /// Facts explicitly pinned by the agent; never auto-evicted.
    pinned: RwLock<Vec<String>>, // conflict keys "user|subject|predicate"
}

impl CoreMemory {
    /// Create a core memory with the given token budget.
    pub fn new(budget_tokens: usize) -> Self {
        Self {
            store: MemoryFactStore::new(),
            budget_tokens,
            pinned: RwLock::new(Vec::new()),
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

    /// Pin a fact's conflict key so it survives budget eviction.
    pub async fn pin(&self, user_id: &str, subject: &str, predicate: &str) {
        self.pinned.write().await.push(conflict_key(user_id, subject, predicate));
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
        self.facts().await.iter().map(|f| f.content.len() / 4 + 40).sum()
    }

    /// Enforce the token budget, evicting oldest-updated non-pinned facts.
    ///
    /// Returns the evicted facts so the caller can archive them (closing the
    /// core→archival paging loop). Pinned facts are never evicted.
    pub async fn enforce_budget(&self) -> Vec<MemoryFact> {
        let mut evicted = Vec::new();
        let pinned = self.pinned.read().await.clone();

        while self.estimated_tokens().await > self.budget_tokens {
            let mut facts = self.facts().await;
            // Evict the least-recently-updated non-pinned fact.
            facts.retain(|f| !pinned.contains(&conflict_key(&f.user_id, &f.subject, &f.predicate)));
            if facts.is_empty() {
                break; // only pinned facts left and still over budget — keep them.
            }
            facts.sort_by_key(|f| f.updated_at);
            let victim = facts.into_iter().next().unwrap();
            self.store.remove(&victim.user_id, &victim.subject, &victim.predicate).await;
            evicted.push(victim);
        }
        evicted
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
        let facts = self.facts().await;
        if facts.is_empty() {
            return String::new();
        }
        let mut out = String::from("[Core Memory]\n");
        for f in &facts {
            out.push_str(&format!("- {} {}: {}\n", f.subject, f.predicate, f.content));
        }
        out
    }
}

fn conflict_key(user_id: &str, subject: &str, predicate: &str) -> String {
    format!("{}|{}|{}", user_id, subject, predicate)
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
            created_at: updated,
            updated_at: updated,
            version: 1,
        }
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
        // Oldest (user.pm) should be the first evicted.
        assert!(evicted.iter().any(|f| f.subject == "user.pm"));
        // Core is now within budget (or only pinned facts remain).
        assert!(cm.estimated_tokens().await <= cm.budget_tokens() || cm.facts().await.is_empty());
    }

    #[tokio::test]
    async fn pinned_facts_survive_eviction() {
        let cm = CoreMemory::new(30);
        let old = chrono::Utc::now() - chrono::Duration::seconds(60);
        cm.upsert(fact("user.pm", "pnpm", old)).await;
        cm.pin("alice", "user.pm", "prefers").await;
        cm.upsert(fact("user.runner", "vitest", chrono::Utc::now())).await;

        let evicted = cm.enforce_budget().await;
        // Pinned user.pm must not be evicted even though it's oldest.
        assert!(!evicted.iter().any(|f| f.subject == "user.pm"));
        let remaining: Vec<_> = cm.facts().await.into_iter().map(|f| f.subject).collect();
        assert!(remaining.contains(&"user.pm".to_string()));
    }

    #[tokio::test]
    async fn upsert_conflict_updates_in_place() {
        let cm = CoreMemory::new(2048);
        cm.upsert(fact("user.pm", "npm", chrono::Utc::now())).await;
        let out = cm.upsert(fact("user.pm", "pnpm", chrono::Utc::now())).await;
        assert_eq!(out, UpsertOutcome::Updated { previous_version: 1 });
        assert_eq!(cm.facts().await.len(), 1);
        assert_eq!(cm.facts().await[0].content, "pnpm");
    }
}
