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

use std::time::Duration;

use oneai_core::{FactType, RecallConfig, RecallStrategy};

// ─── WorkingStatePolicy ──────────────────────────────────────────────────────

/// Where the working-state event log lives. Mirrors the plan's
/// `storage_root` axis: in-repo (git-trackable, free durability + the diff
/// *is* the reconciliation source) vs. the user's home dir (no repo, for
/// assistant/conversational domains with no external ground truth).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageRoot {
    /// `<project_dir>/.oneai/` — in-repo, git-committable (coding domains).
    InRepo,
    /// `~/.oneai/` — user home, no repo association (assistant/conversational).
    HomeDir,
}

/// When to checkpoint (append a working-state event). Per reference doc §8.1
/// crash-safety: `EveryStep` (the default for coding) bounds loss to the last
/// action; `OnTaskBoundary` writes less often (assistant domains where the
/// whole-task summary matters more than per-step audit).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckpointGranularity {
    /// Append an event after every significant action (step / decision /
    /// blocker). Most durable; the only sensible choice for coding domains.
    EveryStep,
    /// Checkpoint at task boundaries (created / paused / resumed / completed).
    /// Less audit detail; suits assistant domains with no per-step substrate.
    OnTaskBoundary,
    /// Only at structurally critical nodes (decisions + blockers + completion).
    CriticalNodes,
}

/// Whether the resume/continue path reconciles the pinned working state
/// against an external ground truth (reference doc §8.2 stale-checkpoint).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroundTruthReconciliation {
    /// No external ground truth (assistant / conversational) — skip the
    /// reconciliation pass. Memory-conflict resolution still runs via the
    /// memory layer's soft-fail path.
    None,
    /// Coding domains: run `git status` / `git log` / `git diff .oneai/` at
    /// resume and flag drift vs the pinned working state.
    Git,
}

/// Whether unfinished work from prior sessions is auto-surfaced.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CrossSessionSurface {
    /// Inject `[Unfinished Work]` on the first turn of a fresh session.
    AutoInject,
    /// Only surface when the user explicitly asks (`tasks list`).
    OnDemand,
}

/// What happens to a task's event log once it completes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Retention {
    /// Gzip the log → `.archive.jsonl.gz` and drop it from the open index.
    ArchiveOnComplete,
    /// Keep the full log in place (assistant domains — thicker audit trail).
    Keep,
}

/// How much working state to carry. Coding domains are `Thin` — much of the
/// state is re-derivable from the code substrate (git). Assistant domains are
/// `Thick` — no external ground truth, so the working state must be richer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkingStateThickness {
    Thin,
    Thick,
}

/// Compaction thresholds for the per-task event log (reference doc §7.3 /
/// §8.4 — bounded growth via in-log snapshot events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Append events beyond this count triggers a fold into a `Snapshot`.
    pub event_threshold: usize,
    /// Number of recent events kept verbatim after compaction.
    pub keep_recent: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            event_threshold: 200,
            keep_recent: 50,
        }
    }
}

/// Working-state persistence + reconciliation policy — the persistence
/// dimension of the memory profile (reference doc §9.1 "persistence" +
/// "ground_truth_reconciliation" axes). Folded into `MemoryProfile` rather
/// than adding an 8th DomainPack layer (per the working-state rework plan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingStatePolicy {
    /// Where the event log is rooted.
    pub storage_root: StorageRoot,
    /// Only `EveryStep` is honored today (the agent appends per action
    /// regardless); the other variants are declarative for future gating.
    pub checkpoint_granularity: CheckpointGranularity,
    /// Resume-time ground-truth reconciliation strategy.
    pub ground_truth_reconciliation: GroundTruthReconciliation,
    /// Cross-session unfinished-work surfacing.
    pub cross_session_surface: CrossSessionSurface,
    /// Completed-task retention.
    pub retention: Retention,
    /// Thin (re-derivable from substrate) vs. Thick (no external GT).
    pub thickness: WorkingStateThickness,
    /// Event-log compaction thresholds.
    pub compaction: CompactionConfig,
    /// Max age before an archived log is fully purged (index summary kept).
    pub max_age_before_archive: Duration,
}

impl Default for WorkingStatePolicy {
    fn default() -> Self {
        Self {
            storage_root: StorageRoot::InRepo,
            checkpoint_granularity: CheckpointGranularity::EveryStep,
            ground_truth_reconciliation: GroundTruthReconciliation::Git,
            cross_session_surface: CrossSessionSurface::AutoInject,
            retention: Retention::ArchiveOnComplete,
            thickness: WorkingStateThickness::Thin,
            compaction: CompactionConfig::default(),
            max_age_before_archive: Duration::from_secs(30 * 24 * 3600),
        }
    }
}

impl WorkingStatePolicy {
    /// Coding-domain default: in-repo, every-step, git reconciliation,
    /// auto-inject, archive on complete, thin, compact at 200/50.
    pub fn coding() -> Self {
        Self {
            storage_root: StorageRoot::InRepo,
            checkpoint_granularity: CheckpointGranularity::EveryStep,
            ground_truth_reconciliation: GroundTruthReconciliation::Git,
            cross_session_surface: CrossSessionSurface::AutoInject,
            retention: Retention::ArchiveOnComplete,
            thickness: WorkingStateThickness::Thin,
            compaction: CompactionConfig {
                event_threshold: 200,
                keep_recent: 50,
            },
            max_age_before_archive: Duration::from_secs(30 * 24 * 3600),
        }
    }

    /// Assistant/conversational default: home dir, task-boundary, no external
    /// ground truth, auto-inject, keep (thick), compact at 500/100.
    pub fn assistant() -> Self {
        Self {
            storage_root: StorageRoot::HomeDir,
            checkpoint_granularity: CheckpointGranularity::OnTaskBoundary,
            ground_truth_reconciliation: GroundTruthReconciliation::None,
            cross_session_surface: CrossSessionSurface::AutoInject,
            retention: Retention::Keep,
            thickness: WorkingStateThickness::Thick,
            compaction: CompactionConfig {
                event_threshold: 500,
                keep_recent: 100,
            },
            max_age_before_archive: Duration::from_secs(90 * 24 * 3600),
        }
    }

    /// Merge two policies for multi-domain agents.
    ///
    /// Rules (aligned with the rest of `merge.rs` — primary wins ties,
    /// strictest ceiling wins on bounds):
    /// - enums: take the **primary** (left) — a domain that cares about its
    ///   ground-truth reconciliation keeps its strategy.
    /// - `compaction`: take the **minimum** event_threshold and keep_recent
    ///   (strictest compaction — keeps logs smallest across domains).
    /// - `max_age_before_archive`: take the **minimum** (purge earliest).
    pub fn merge(primary: &Self, other: &Self) -> Self {
        Self {
            storage_root: primary.storage_root.clone(),
            checkpoint_granularity: primary.checkpoint_granularity.clone(),
            ground_truth_reconciliation: primary.ground_truth_reconciliation.clone(),
            cross_session_surface: primary.cross_session_surface.clone(),
            retention: primary.retention.clone(),
            thickness: primary.thickness.clone(),
            compaction: CompactionConfig {
                event_threshold: primary
                    .compaction
                    .event_threshold
                    .min(other.compaction.event_threshold),
                keep_recent: primary.compaction.keep_recent.min(other.compaction.keep_recent),
            },
            max_age_before_archive: primary
                .max_age_before_archive
                .min(other.max_age_before_archive),
        }
    }
}

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

    /// Working-state persistence + reconciliation policy (the persistence
    /// dimension of this profile — reference doc §9.1). Folded in rather than
    /// adding an 8th DomainPack layer.
    pub working_state: WorkingStatePolicy,
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
            working_state: WorkingStatePolicy::coding(),
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

    /// Set the working-state policy (persistence + reconciliation).
    pub fn working_state(mut self, policy: WorkingStatePolicy) -> Self {
        self.working_state = policy;
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
                ..Default::default()
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
            .working_state(WorkingStatePolicy::assistant())
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
            working_state: WorkingStatePolicy::merge(&primary.working_state, &other.working_state),
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
            ..Default::default()
        });
        let b = MemoryProfile::new("b").recall(RecallConfig::default());
        let m = MemoryProfile::merge(&a, &b);
        assert_eq!(m.recall.strategy, RecallStrategy::KeywordFirst);
        assert_eq!(m.recall.top_k, 3);
    }

    #[test]
    fn test_working_state_policy_presets() {
        let c = WorkingStatePolicy::coding();
        assert_eq!(c.storage_root, StorageRoot::InRepo);
        assert_eq!(c.ground_truth_reconciliation, GroundTruthReconciliation::Git);
        assert_eq!(c.retention, Retention::ArchiveOnComplete);
        assert_eq!(c.thickness, WorkingStateThickness::Thin);
        assert_eq!(c.compaction.event_threshold, 200);
        assert_eq!(c.compaction.keep_recent, 50);

        let a = WorkingStatePolicy::assistant();
        assert_eq!(a.storage_root, StorageRoot::HomeDir);
        assert_eq!(a.ground_truth_reconciliation, GroundTruthReconciliation::None);
        assert_eq!(a.retention, Retention::Keep);
        assert_eq!(a.thickness, WorkingStateThickness::Thick);
        assert_eq!(a.compaction.event_threshold, 500);
    }

    #[test]
    fn test_coding_profile_carries_coding_working_state_policy() {
        let p = MemoryProfile::coding();
        assert_eq!(p.working_state.ground_truth_reconciliation, GroundTruthReconciliation::Git);
        assert_eq!(p.working_state.storage_root, StorageRoot::InRepo);
    }

    #[test]
    fn test_research_profile_carries_assistant_working_state_policy() {
        let p = MemoryProfile::research();
        assert_eq!(p.working_state.ground_truth_reconciliation, GroundTruthReconciliation::None);
        assert_eq!(p.working_state.storage_root, StorageRoot::HomeDir);
    }

    #[test]
    fn test_working_state_merge_takes_min_compaction() {
        // Coding (200/50) merged with assistant (500/100) → min = 200/50.
        let m = WorkingStatePolicy::merge(&WorkingStatePolicy::coding(), &WorkingStatePolicy::assistant());
        assert_eq!(m.compaction.event_threshold, 200);
        assert_eq!(m.compaction.keep_recent, 50);
        // Primary's storage_root wins (coding → InRepo) even though assistant is HomeDir.
        assert_eq!(m.storage_root, StorageRoot::InRepo);
    }
}
