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

use oneai_core::{DecayPolicy, FactType, RecallConfig, RecallStrategy};

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
                keep_recent: primary
                    .compaction
                    .keep_recent
                    .min(other.compaction.keep_recent),
            },
            max_age_before_archive: primary
                .max_age_before_archive
                .min(other.max_age_before_archive),
        }
    }
}

// ─── SkillLifecyclePolicy ───────────────────────────────────────────────────

/// Skill lifecycle policy — the "grow-with-you" dimension of the memory
/// profile (Phase 2.1 Stage B). Folded into `MemoryProfile` rather than adding
/// an 8th DomainPack layer, mirroring the `WorkingStatePolicy` precedent.
///
/// This is the *declarative* DomainPack-side config; `oneai-skill`'s
/// `SkillMetadataStore` consumes its primitives (the store stays decoupled
/// from `oneai-domain`). Answers, per domain:
///
/// - **When a skill goes stale / gets archived** — `stale_after` / `archive_after`.
/// - **Whether the curator runs automatic transitions at all** —
///   `auto_transitions` (off = the curator records usage only; retirement is
///   manual). A domain that wants full manual control (e.g. a strict coding
///   domain where every skill is curated) turns this off.
/// - **How many restorable backup snapshots to keep** — `backup_count`.
/// - **Where the metadata + backups live** — `storage_root` (skill *usage* is
///   a user habit, so both presets default to `HomeDir` — putting personal
///   use-counts in-repo would pollute the repo and leak usage patterns).
/// - **Grace window for never-used fresh skills** — `grace_unused` (give a
///   freshly-authored skill time to be discovered before aging it out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillLifecyclePolicy {
    /// Idle duration that ages a skill `Active → Stale`.
    pub stale_after: Duration,
    /// Idle duration that ages a skill `Stale → Archived`.
    pub archive_after: Duration,
    /// Whether the curator applies automatic transitions (`run`).
    pub auto_transitions: bool,
    /// How many rotating backup snapshots to keep on disk.
    pub backup_count: usize,
    /// Where the metadata index + backups live.
    pub storage_root: StorageRoot,
    /// Grace window for never-used skills since `created_at`.
    pub grace_unused: Duration,
}

impl Default for SkillLifecyclePolicy {
    fn default() -> Self {
        Self {
            stale_after: Duration::from_secs(30 * 24 * 3600),
            archive_after: Duration::from_secs(90 * 24 * 3600),
            auto_transitions: true,
            backup_count: 5,
            storage_root: StorageRoot::HomeDir,
            grace_unused: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

impl SkillLifecyclePolicy {
    /// Coding-domain default. Skill *usage* is a personal habit, so metadata
    /// lives under the user's home dir (not in-repo — putting personal
    /// use-counts in the repo would pollute it and leak usage patterns across
    /// teammates). Aggressive retirement (30d/90d) keeps the schema footprint
    /// tight for the heavily-tooled coding domain.
    pub fn coding() -> Self {
        Self {
            stale_after: Duration::from_secs(30 * 24 * 3600),
            archive_after: Duration::from_secs(90 * 24 * 3600),
            auto_transitions: true,
            backup_count: 5,
            storage_root: StorageRoot::HomeDir,
            grace_unused: Duration::from_secs(7 * 24 * 3600),
        }
    }

    /// Assistant/conversational default. Gentler retirement (60d/180d) —
    /// assistant domains accumulate more skills and a skill that goes unused
    /// for a few weeks may still be relevant to a recurring conversation.
    pub fn assistant() -> Self {
        Self {
            stale_after: Duration::from_secs(60 * 24 * 3600),
            archive_after: Duration::from_secs(180 * 24 * 3600),
            auto_transitions: true,
            backup_count: 5,
            storage_root: StorageRoot::HomeDir,
            grace_unused: Duration::from_secs(14 * 24 * 3600),
        }
    }

    /// Merge two skill-lifecycle policies for multi-domain agents.
    ///
    /// Rules (aligned with the rest of `merge.rs`):
    /// - **min** `stale_after` / `archive_after` / `grace_unused` (strictest
    ///   retirement wins — a coding domain's 30d beats an assistant's 60d).
    /// - **min** `backup_count` (keep fewest snapshots — strictest disk bound).
    /// - `auto_transitions`: **OR** (any domain opting into auto transitions
    ///   enables them — a domain that wants manual control can still pin).
    /// - `storage_root`: take the **primary** (left), like
    ///   `WorkingStatePolicy::merge` does for its enums.
    pub fn merge(primary: &Self, other: &Self) -> Self {
        Self {
            stale_after: primary.stale_after.min(other.stale_after),
            archive_after: primary.archive_after.min(other.archive_after),
            auto_transitions: primary.auto_transitions || other.auto_transitions,
            backup_count: primary.backup_count.min(other.backup_count),
            storage_root: primary.storage_root.clone(),
            grace_unused: primary.grace_unused.min(other.grace_unused),
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

    /// Skill lifecycle policy (Phase 2.1 Stage B) — the "grow-with-you"
    /// dimension: when skills age to Stale/Archived, whether the curator runs
    /// automatically, how many backups to keep. Folded in, mirroring
    /// `working_state`.
    pub skill_lifecycle: SkillLifecyclePolicy,

    /// Memory decay / forgetting policy (Phase 2.4, gap P2 #16) — the
    /// "don't-accumulate-forever" dimension: importance-threshold eviction
    /// (core→archive) + soft-invalidation of stale low-salience archival
    /// facts. Default-off / opt-in (coding keeps facts forever; the
    /// "grow-with-you" domains enable it). Folded in, mirroring
    /// `working_state` / `skill_lifecycle`.
    pub decay: DecayPolicy,
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
            skill_lifecycle: SkillLifecyclePolicy::coding(),
            decay: DecayPolicy::default(),
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

    /// Set the decay / forgetting policy (Phase 2.4).
    pub fn decay(mut self, decay: DecayPolicy) -> Self {
        self.decay = decay;
        self
    }

    /// Set the working-state policy (persistence + reconciliation).
    pub fn working_state(mut self, policy: WorkingStatePolicy) -> Self {
        self.working_state = policy;
        self
    }

    /// Set the skill-lifecycle policy (retirement + backups).
    pub fn skill_lifecycle(mut self, policy: SkillLifecyclePolicy) -> Self {
        self.skill_lifecycle = policy;
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
            .skill_lifecycle(SkillLifecyclePolicy::assistant())
            // Research is a "grow-with-you" domain — enable decay so the
            // fact base doesn't accumulate stale low-salience noise forever.
            .decay(DecayPolicy {
                enabled: true,
                ..DecayPolicy::default()
            })
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
            skill_lifecycle: SkillLifecyclePolicy::merge(
                &primary.skill_lifecycle,
                &other.skill_lifecycle,
            ),
            // Decay: strictest-wins. enabled=OR (any domain opting in
            // enables decay); thresholds/half-life take the min (more
            // aggressive eviction wins, mirroring permission strictest-wins);
            // ttl takes the min (earliest-eligible forget wins); sweep=AND
            // (both must opt into sync-sweep — conservative).
            decay: DecayPolicy {
                enabled: primary.decay.enabled || other.decay.enabled,
                min_salience: primary.decay.min_salience.min(other.decay.min_salience),
                archive_forget_salience: primary
                    .decay
                    .archive_forget_salience
                    .min(other.decay.archive_forget_salience),
                archive_ttl_secs: primary
                    .decay
                    .archive_ttl_secs
                    .min(other.decay.archive_ttl_secs),
                recency_half_life_secs: primary
                    .decay
                    .recency_half_life_secs
                    .min(other.decay.recency_half_life_secs),
                sweep_on_reflect: primary.decay.sweep_on_reflect && other.decay.sweep_on_reflect,
            },
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
        assert!(p
            .extraction_schema
            .contains(&FactType::new("user_tooling_pref")));
        assert!(p
            .habit_fact_types
            .contains(&FactType::new("user_tooling_pref")));
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
        assert!(m
            .habit_fact_types
            .contains(&FactType::new("user_tooling_pref")));
        assert!(m.habit_fact_types.contains(&FactType::new("source")));
    }

    #[test]
    fn test_merge_takes_min_budget_and_or_tools() {
        let a = MemoryProfile::new("a")
            .core_budget_tokens(2000)
            .enable_memory_tools(true);
        let b = MemoryProfile::new("b")
            .core_budget_tokens(1000)
            .enable_memory_tools(false);
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
    fn test_merge_decay_strictest_wins() {
        // a: enabled, aggressive (low threshold, short ttl, sweep on)
        let a = MemoryProfile::new("a").decay(DecayPolicy {
            enabled: true,
            min_salience: 0.05,
            archive_forget_salience: 0.02,
            archive_ttl_secs: 90 * 24 * 3600,
            recency_half_life_secs: 7 * 24 * 3600,
            sweep_on_reflect: true,
        });
        // b: disabled, lenient (high threshold, long ttl, sweep off)
        let b = MemoryProfile::new("b").decay(DecayPolicy {
            enabled: false,
            min_salience: 0.2,
            archive_forget_salience: 0.1,
            archive_ttl_secs: 365 * 24 * 3600,
            recency_half_life_secs: 30 * 24 * 3600,
            sweep_on_reflect: false,
        });
        let m = MemoryProfile::merge(&a, &b);
        // enabled = OR (a opts in)
        assert!(m.decay.enabled);
        // thresholds/half-life = min (more aggressive wins)
        assert!((m.decay.min_salience - 0.05).abs() < 1e-6);
        assert!((m.decay.archive_forget_salience - 0.02).abs() < 1e-6);
        assert_eq!(m.decay.archive_ttl_secs, 90 * 24 * 3600);
        assert_eq!(m.decay.recency_half_life_secs, 7 * 24 * 3600);
        // sweep = AND (b opted out → off)
        assert!(!m.decay.sweep_on_reflect);
    }

    #[test]
    fn test_coding_decay_off_research_on() {
        // Coding keeps facts forever (backward-compat); research enables decay.
        assert!(!MemoryProfile::coding().decay.enabled);
        assert!(MemoryProfile::research().decay.enabled);
    }

    #[test]
    fn test_working_state_policy_presets() {
        let c = WorkingStatePolicy::coding();
        assert_eq!(c.storage_root, StorageRoot::InRepo);
        assert_eq!(
            c.ground_truth_reconciliation,
            GroundTruthReconciliation::Git
        );
        assert_eq!(c.retention, Retention::ArchiveOnComplete);
        assert_eq!(c.thickness, WorkingStateThickness::Thin);
        assert_eq!(c.compaction.event_threshold, 200);
        assert_eq!(c.compaction.keep_recent, 50);

        let a = WorkingStatePolicy::assistant();
        assert_eq!(a.storage_root, StorageRoot::HomeDir);
        assert_eq!(
            a.ground_truth_reconciliation,
            GroundTruthReconciliation::None
        );
        assert_eq!(a.retention, Retention::Keep);
        assert_eq!(a.thickness, WorkingStateThickness::Thick);
        assert_eq!(a.compaction.event_threshold, 500);
    }

    #[test]
    fn test_coding_profile_carries_coding_working_state_policy() {
        let p = MemoryProfile::coding();
        assert_eq!(
            p.working_state.ground_truth_reconciliation,
            GroundTruthReconciliation::Git
        );
        assert_eq!(p.working_state.storage_root, StorageRoot::InRepo);
    }

    #[test]
    fn test_research_profile_carries_assistant_working_state_policy() {
        let p = MemoryProfile::research();
        assert_eq!(
            p.working_state.ground_truth_reconciliation,
            GroundTruthReconciliation::None
        );
        assert_eq!(p.working_state.storage_root, StorageRoot::HomeDir);
    }

    #[test]
    fn test_working_state_merge_takes_min_compaction() {
        // Coding (200/50) merged with assistant (500/100) → min = 200/50.
        let m = WorkingStatePolicy::merge(
            &WorkingStatePolicy::coding(),
            &WorkingStatePolicy::assistant(),
        );
        assert_eq!(m.compaction.event_threshold, 200);
        assert_eq!(m.compaction.keep_recent, 50);
        // Primary's storage_root wins (coding → InRepo) even though assistant is HomeDir.
        assert_eq!(m.storage_root, StorageRoot::InRepo);
    }

    #[test]
    fn test_skill_lifecycle_presets() {
        let c = SkillLifecyclePolicy::coding();
        assert_eq!(c.stale_after, Duration::from_secs(30 * 24 * 3600));
        assert_eq!(c.archive_after, Duration::from_secs(90 * 24 * 3600));
        assert!(c.auto_transitions);
        assert_eq!(c.backup_count, 5);
        assert_eq!(c.storage_root, StorageRoot::HomeDir);

        let a = SkillLifecyclePolicy::assistant();
        // Assistant is gentler (longer retirement thresholds).
        assert!(a.stale_after > c.stale_after);
        assert!(a.archive_after > c.archive_after);
        assert!(a.grace_unused > c.grace_unused);
    }

    #[test]
    fn test_coding_profile_carries_coding_skill_lifecycle() {
        let p = MemoryProfile::coding();
        assert_eq!(
            p.skill_lifecycle.stale_after,
            Duration::from_secs(30 * 24 * 3600)
        );
        assert_eq!(p.skill_lifecycle.storage_root, StorageRoot::HomeDir);
    }

    #[test]
    fn test_research_profile_carries_assistant_skill_lifecycle() {
        let p = MemoryProfile::research();
        assert_eq!(
            p.skill_lifecycle.archive_after,
            Duration::from_secs(180 * 24 * 3600)
        );
    }

    #[test]
    fn test_skill_lifecycle_merge_takes_min_thresholds() {
        // Coding (30d/90d) merged with assistant (60d/180d) → min = 30d/90d.
        let m = SkillLifecyclePolicy::merge(
            &SkillLifecyclePolicy::coding(),
            &SkillLifecyclePolicy::assistant(),
        );
        assert_eq!(m.stale_after, Duration::from_secs(30 * 24 * 3600));
        assert_eq!(m.archive_after, Duration::from_secs(90 * 24 * 3600));
        // min backup_count (both 5 → 5).
        assert_eq!(m.backup_count, 5);
        // OR auto_transitions (both true → true).
        assert!(m.auto_transitions);
        // Primary's storage_root wins.
        assert_eq!(m.storage_root, StorageRoot::HomeDir);
    }

    #[test]
    fn test_skill_lifecycle_merge_auto_or_and_backup_min() {
        let a = SkillLifecyclePolicy {
            auto_transitions: false,
            backup_count: 8,
            ..SkillLifecyclePolicy::coding()
        };
        let b = SkillLifecyclePolicy {
            auto_transitions: true,
            backup_count: 3,
            ..SkillLifecyclePolicy::assistant()
        };
        let m = SkillLifecyclePolicy::merge(&a, &b);
        assert!(m.auto_transitions); // OR
        assert_eq!(m.backup_count, 3); // min
    }
}
