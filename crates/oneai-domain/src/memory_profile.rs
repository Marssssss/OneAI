//! MemoryProfile — domain-specific memory policy (DomainPack layer 7).
//!
//! A `MemoryProfile` makes the agent's *memory behavior* declarative and
//! composable, just like the other DomainPack layers (`CompressionTemplate`,
//! `ContextSource`, `PermissionProfile`, …). It answers, per domain:
//!
//! - **What to remember** — `extraction_schema`: which categories of atomic
//!   facts to extract from the conversation (coding: tooling preferences,
//!   decisions, open tasks, critical files; research: sources, claims, open
//!   questions). This drives the compression-coupled `FactExtractor`.
//! - **How to recall** — `recall`: strategy (keyword/semantic/hybrid), top_k,
//!   time-decay. Backs the `CoreMemorySource` injection each turn.
//! - **How much stays on** — `core_budget_tokens`: the always-in-context core
//!   memory ceiling (Letta-style core tier).
//! - **Who manages it** — `enable_memory_tools`: whether the agent may curate
//!   its own core memory via self-managed tools (the "越用越好用" engine).
//! - **What persists across sessions** — `habit_fact_types`: fact types
//!   persisted under the **user** namespace and recalled across sessions
//!   (preferences, habits, long-term profile).
//!
//! Design rationale: without this layer, memory behavior is hardcoded in
//! `oneai-memory` (a one-size-fits-all episodic reflection). With it, the same
//! agent switches memory policy in one line via `AppBuilder::domain_pack(...)`,
//! and multi-domain agents merge policies sensibly.

use oneai_core::{FactType, RecallConfig, RecallStrategy};

// ─── MemoryProfile ───────────────────────────────────────────────────────────

/// Domain-specific memory policy — the 7th DomainPack layer.
///
/// See the module docs for the full rationale. All fields have sensible
/// defaults so a domain that doesn't care about memory can omit the layer
/// entirely and inherit generic behavior.
#[derive(Debug, Clone)]
pub struct MemoryProfile {
    /// Human-readable name (e.g. "coding", "research").
    pub name: String,

    /// Fact categories this domain extracts from conversation as durable
    /// memory. Drives the `FactExtractor` prompt schema.
    pub extraction_schema: Vec<FactType>,

    /// How facts are recalled into context each turn.
    pub recall: RecallConfig,

    /// Token budget for the always-in-context core memory tier.
    pub core_budget_tokens: usize,

    /// Whether to expose self-managed memory tools (`memory_search`,
    /// `core_memory_append/replace`, `archival_memory_insert`) to the agent
    /// in this domain.
    pub enable_memory_tools: bool,

    /// Fact types persisted under the **user** namespace and recalled across
    /// sessions. These are the "user habits" that make the agent improve with
    /// use. A subset of (or extending) `extraction_schema`.
    pub habit_fact_types: Vec<FactType>,
}

impl MemoryProfile {
    /// Create a new profile with the given name and sensible defaults.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            extraction_schema: Vec::new(),
            recall: RecallConfig::default(),
            core_budget_tokens: 2048,
            enable_memory_tools: false,
            habit_fact_types: Vec::new(),
        }
    }

    /// Set the extraction schema.
    pub fn extraction_schema(mut self, schema: Vec<FactType>) -> Self {
        self.extraction_schema = schema;
        self
    }

    /// Set the recall configuration.
    pub fn recall(mut self, recall: RecallConfig) -> Self {
        self.recall = recall;
        self
    }

    /// Set the core memory token budget.
    pub fn core_budget_tokens(mut self, tokens: usize) -> Self {
        self.core_budget_tokens = tokens;
        self
    }

    /// Enable/disable self-managed memory tools for this domain.
    pub fn enable_memory_tools(mut self, enabled: bool) -> Self {
        self.enable_memory_tools = enabled;
        self
    }

    /// Set the habit (cross-session, user-namespace) fact types.
    pub fn habit_fact_types(mut self, types: Vec<FactType>) -> Self {
        self.habit_fact_types = types;
        self
    }

    /// The coding-domain default memory profile.
    ///
    /// Mirrors `CODING_COMPRESSION_TEMPLATE`'s preservation priorities: the
    /// facts worth remembering for continuing coding work are tooling
    /// preferences, decisions, open tasks, and critical files. Tooling
    /// preferences are habits (cross-session); the rest are session-scoped.
    pub fn coding() -> Self {
        Self::new("coding")
            .extraction_schema(vec![
                FactType::new("user_tooling_pref"),
                FactType::new("decision"),
                FactType::new("open_task"),
                FactType::new("critical_file"),
            ])
            .recall(RecallConfig {
                strategy: RecallStrategy::Hybrid,
                top_k: 5,
                time_decay: true,
            })
            .core_budget_tokens(2048)
            .enable_memory_tools(true)
            .habit_fact_types(vec![FactType::new("user_tooling_pref")])
    }

    /// The research-domain default memory profile.
    pub fn research() -> Self {
        Self::new("research")
            .extraction_schema(vec![
                FactType::new("source"),
                FactType::new("claim"),
                FactType::new("open_question"),
                FactType::new("user_interest"),
            ])
            .recall(RecallConfig::default())
            .core_budget_tokens(1536)
            .enable_memory_tools(true)
            .habit_fact_types(vec![
                FactType::new("user_interest"),
                FactType::new("source"),
            ])
    }
}

impl Default for MemoryProfile {
    fn default() -> Self {
        Self::new("default")
    }
}

// ─── Merge ───────────────────────────────────────────────────────────────────

impl MemoryProfile {
    /// Merge two memory profiles for multi-domain agents.
    ///
    /// Rules (aligned with the rest of `merge.rs`):
    /// - `extraction_schema` / `habit_fact_types`: union, deduplicated.
    /// - `recall`: take the **primary** (left) profile's config, like
    ///   `CompressionTemplate` takes the primary pack's template.
    /// - `core_budget_tokens`: take the **minimum** (strictest ceiling).
    /// - `enable_memory_tools`: OR (any domain opting in enables the tools).
    pub fn merge(primary: &Self, other: &Self) -> Self {
        let mut schema: Vec<FactType> = primary.extraction_schema.clone();
        for ft in &other.extraction_schema {
            if !schema.contains(ft) {
                schema.push(ft.clone());
            }
        }
        let mut habits: Vec<FactType> = primary.habit_fact_types.clone();
        for ft in &other.habit_fact_types {
            if !habits.contains(ft) {
                habits.push(ft.clone());
            }
        }
        Self {
            name: format!("{}+{}", primary.name, other.name),
            extraction_schema: schema,
            recall: primary.recall.clone(),
            core_budget_tokens: primary.core_budget_tokens.min(other.core_budget_tokens),
            enable_memory_tools: primary.enable_memory_tools || other.enable_memory_tools,
            habit_fact_types: habits,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_profile() {
        let p = MemoryProfile::default();
        assert_eq!(p.name, "default");
        assert!(p.extraction_schema.is_empty());
        assert!(!p.enable_memory_tools);
        assert_eq!(p.core_budget_tokens, 2048);
    }

    #[test]
    fn test_coding_profile() {
        let p = MemoryProfile::coding();
        assert_eq!(p.name, "coding");
        assert!(p.enable_memory_tools);
        assert!(p.extraction_schema.contains(&FactType::new("user_tooling_pref")));
        assert!(p.habit_fact_types.contains(&FactType::new("user_tooling_pref")));
        assert!(p.extraction_schema.contains(&FactType::new("decision")));
        // decisions are session-scoped, not habits
        assert!(!p.habit_fact_types.contains(&FactType::new("decision")));
    }

    #[test]
    fn test_merge_unions_schema_and_habits() {
        let a = MemoryProfile::coding();
        let b = MemoryProfile::research();
        let m = MemoryProfile::merge(&a, &b);
        assert!(m.extraction_schema.contains(&FactType::new("decision")));
        assert!(m.extraction_schema.contains(&FactType::new("claim")));
        assert!(m.habit_fact_types.contains(&FactType::new("user_tooling_pref")));
        assert!(m.habit_fact_types.contains(&FactType::new("source")));
    }

    #[test]
    fn test_merge_takes_min_budget_and_or_tools() {
        let a = MemoryProfile::new("a").core_budget_tokens(2000).enable_memory_tools(true);
        let b = MemoryProfile::new("b").core_budget_tokens(1000).enable_memory_tools(false);
        let m = MemoryProfile::merge(&a, &b);
        assert_eq!(m.core_budget_tokens, 1000); // min
        assert!(m.enable_memory_tools); // OR
    }

    #[test]
    fn test_merge_takes_primary_recall() {
        let a = MemoryProfile::new("a").recall(RecallConfig {
            strategy: RecallStrategy::KeywordFirst,
            top_k: 3,
            time_decay: false,
        });
        let b = MemoryProfile::new("b").recall(RecallConfig::default());
        let m = MemoryProfile::merge(&a, &b);
        assert_eq!(m.recall.strategy, RecallStrategy::KeywordFirst);
        assert_eq!(m.recall.top_k, 3);
    }
}
