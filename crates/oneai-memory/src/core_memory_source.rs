//! CoreMemorySource — surface core memory + recalled facts as an anti-compression
//! context source.
//!
//! Implements `oneai_domain::ContextSource` so the existing `ContextAssembler`
//! epoch machinery injects core memory every iteration — no new injection
//! plumbing. Refresh policy is `EveryIteration`: this is what makes the block
//! **anti-compression**. The `ContextCompressor` discards older messages on
//! compression (keeping only `keep_recent_turns`), but the next iteration's
//! `assemble()` re-injects this source, so core memory survives compression —
//! unlike the old one-shot "Previous conversation context" system message which
//! was buried in history and could be summarized away.
//!
//! Two sections:
//! - `[Core Memory]` — curated facts the agent always sees (self-managed via
//!   tools in P5).
//! - `[Recalled Context]` — per-turn archival recall, set by `AppSession`
//!   before each run (replaces the old `retrieve()` one-shot message).

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::MemoryFact;
use oneai_domain::context_source::{ContextSource, RefreshPolicy};
use tokio::sync::RwLock;

use crate::core_memory::CoreMemory;

/// Context source that injects the core memory block (+ recalled context) each
/// iteration, protected from compression by `EveryIteration` re-injection.
pub struct CoreMemorySource {
    core: Arc<CoreMemory>,
    /// Rendered "[Recalled Context]" section, set per-turn by the session.
    recall: RwLock<String>,
}

impl CoreMemorySource {
    /// Create a source backed by the given core memory.
    pub fn new(core: Arc<CoreMemory>) -> Self {
        Self { core, recall: RwLock::new(String::new()) }
    }

    /// Set the per-turn recalled context (from archival `retrieve`).
    ///
    /// Replaces the old one-shot "Previous conversation context" system message:
    /// recall now lives in the protected core block instead of being a
    /// compressible history message.
    pub async fn set_recall(&self, facts: Vec<MemoryFact>) {
        let rendered = if facts.is_empty() {
            String::new()
        } else {
            let mut out = String::from("\n[Recalled Context]\n");
            for f in &facts {
                out.push_str(&format!("- {} {}: {}\n", f.subject, f.predicate, f.content));
            }
            out
        };
        *self.recall.write().await = rendered;
    }

    /// Set the per-turn recalled context from a pre-rendered string (e.g. the
    /// legacy `MemoryEntry`-based `retrieve` path, which isn't fact-typed).
    pub async fn set_recall_text(&self, text: impl Into<String>) {
        let t = text.into();
        let rendered = if t.trim().is_empty() {
            String::new()
        } else {
            format!("\n[Recalled Context]\n{}", t)
        };
        *self.recall.write().await = rendered;
    }
}

#[async_trait]
impl ContextSource for CoreMemorySource {
    fn key(&self) -> &str {
        "core_memory"
    }

    async fn load(&self) -> Result<String> {
        let mut out = self.core.render().await;
        out.push_str(&self.recall.read().await);
        Ok(out)
    }

    /// Every iteration — guarantees re-injection after compression, making the
    /// core block anti-compression. The block is small (bounded by core token
    /// budget), so the per-iteration cost is bounded.
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::EveryIteration
    }

    /// High priority (low number) — injected early, before domain env sources.
    fn priority(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::FactType;
    use std::collections::HashMap;

    fn fact(subject: &str, content: &str) -> MemoryFact {
        MemoryFact {
            id: format!("f_{}", subject),
            user_id: "alice".to_string(),
            session_id: "s1".to_string(),
            fact_type: FactType::new("user_tooling_pref"),
            subject: subject.to_string(),
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
        }
    }

    #[tokio::test]
    async fn load_renders_core_block() {
        let cm = Arc::new(CoreMemory::new(2048));
        cm.upsert(fact("user.pm", "pnpm")).await;
        let src = CoreMemorySource::new(cm);
        let loaded = src.load().await.unwrap();
        assert!(loaded.contains("[Core Memory]"));
        assert!(loaded.contains("user.pm prefers: pnpm"));
    }

    #[tokio::test]
    async fn set_recall_appends_recalled_section() {
        let cm = Arc::new(CoreMemory::new(2048));
        let src = CoreMemorySource::new(cm);
        src.set_recall(vec![fact("user.runner", "vitest")]).await;
        let loaded = src.load().await.unwrap();
        assert!(loaded.contains("[Recalled Context]"));
        assert!(loaded.contains("user.runner prefers: vitest"));
    }

    #[tokio::test]
    async fn empty_recall_omits_section() {
        let cm = Arc::new(CoreMemory::new(2048));
        let src = CoreMemorySource::new(cm);
        src.set_recall(Vec::new()).await;
        let loaded = src.load().await.unwrap();
        assert!(!loaded.contains("[Recalled Context]"));
    }

    #[test]
    fn policy_is_every_iteration_and_high_priority() {
        // Anti-compression guarantee: EveryIteration re-injects after compress.
        let cm = Arc::new(CoreMemory::new(2048));
        let src = CoreMemorySource::new(cm);
        assert_eq!(src.refresh_policy(), RefreshPolicy::EveryIteration);
        assert!(src.priority() < 100);
    }
}
