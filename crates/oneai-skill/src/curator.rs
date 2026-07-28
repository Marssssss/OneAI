//! `SkillCurator` — the closed-loop skill steward (Phase 2.1 Stage B).
//!
//! Where [`SkillMetadataStore`] is the *storage* layer (per-skill metadata +
//! backup snapshots), the curator is the *policy* layer that ties the store
//! to the live [`SkillRegistry`] and runs the Hermes-style stewardship loop:
//!
//! - **run** — apply automatic `Active → Stale → Archived` transitions, then
//!   persist + emit a human-readable report. The reflect sub-agent (Stage A)
//!   persists *learnings*; the curator *retires* skills that no longer earn
//!   their schema footprint. Together they close the loop.
//! - **status** — a per-skill table joining the registry's descriptors with
//!   the store's metadata (state, use_count, last activity, pinned, author).
//! - **pin / archive / restore** — manual overrides the model reaches via
//!   the `skill_manage` tool and the user via `oneai curator`.
//! - **backup / rollback** — point-in-time restorable snapshots of every
//!   skill the agent sees; the curator writes one before each `run` that
//!   archives anything, and on explicit `backup`.
//!
//! The curator never *deletes* a skill — only archives. Archiving a skill
//! referenced by a DomainPack workflow / StateGraph / cron is logged loudly
//! (not silently) and, for `run`, the skill is added to the exempt set so the
//! automatic pass leaves it alone. Destructive rewrite of workflow skill
//! references is deliberately out of scope: the curator warns; the human (or
//! a future Stage) reconciles the pack.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use oneai_core::SkillDescriptor;

use crate::lifecycle::{
    now_unix, SkillAuthor, SkillLifecycleConfig, SkillMetadata, SkillMetadataStore, SkillState,
};
use crate::registry::SkillRegistry;

/// Error from [`SkillCurator::apply_merge`] — the only failure modes that
/// abort a merge. Per-member problems (a member that's referenced or already
/// archived) are *skipped with a warning*, not errors — a consolidation pass
/// should apply what it can and report what it skipped.
#[derive(Debug)]
pub enum MergeError {
    Io(String),
    /// The umbrella name is already a registered skill — refuse to overwrite
    /// via merge (the human reconciles, or picks a fresh umbrella name).
    UmbrellaExists(String),
    /// A requested member skill isn't registered.
    NotFound(String),
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Io(m) => write!(f, "I/O error writing umbrella skill: {m}"),
            MergeError::UmbrellaExists(n) => {
                write!(
                    f,
                    "umbrella skill '{n}' already registered (refuse merge overwrite)"
                )
            }
            MergeError::NotFound(n) => write!(f, "skill '{n}' not registered"),
        }
    }
}

impl std::error::Error for MergeError {}

/// Result of one umbrella merge applied by [`SkillCurator::apply_merge`].
#[derive(Debug, Clone, Default)]
pub struct MergeReport {
    /// The umbrella skill name written + registered.
    pub umbrella_name: String,
    /// Members successfully archived into the umbrella.
    pub members_archived: Vec<String>,
    /// Members skipped (referenced by the active pack — destructive rewrite
    /// is out of scope; the human reconciles).
    pub members_skipped: Vec<String>,
    /// The backup snapshot id written before any retirement (pass to
    /// [`SkillCurator::rollback`] to undo the whole merge).
    pub backup_id: u64,
}

/// A steward that runs the skill lifecycle against the live registry.
pub struct SkillCurator {
    registry: Arc<SkillRegistry>,
    store: Arc<SkillMetadataStore>,
    /// Skill names referenced by the active DomainPack's workflows /
    /// StateGraphs / cron jobs. Such skills are exempt from automatic
    /// retirement and warn when manually archived. Refreshed by
    /// `oneai-agent`/`oneai-app` when the pack reloads.
    referenced: std::sync::RwLock<HashSet<String>>,
}

/// Result of a `curator run` — surfaced to the CLI and `on_reflection`-style
/// observers as a short report.
#[derive(Debug, Clone, Default)]
pub struct CuratorReport {
    /// Skills aged `Active → Stale` this run.
    pub gone_stale: Vec<String>,
    /// Skills retired `* → Archived` this run.
    pub archived: Vec<String>,
    /// Whether a backup snapshot was written (always before an archiving run).
    pub backup_written: bool,
}

impl SkillCurator {
    /// Create a curator over the given registry + store. `referenced` lists
    /// skill names the active pack references (workflows/StateGraphs/cron).
    pub fn new(
        registry: Arc<SkillRegistry>,
        store: Arc<SkillMetadataStore>,
        referenced: HashSet<String>,
    ) -> Self {
        Self {
            registry,
            store,
            referenced: std::sync::RwLock::new(referenced),
        }
    }

    /// Replace the referenced-skill set (called by the agent when the pack
    /// reloads — see evolution-plan §3.4 data-layer hot reload).
    pub fn set_referenced(&self, referenced: HashSet<String>) {
        *self.referenced.write().unwrap() = referenced;
    }

    /// Access the underlying metadata store (e.g. for `skill_manage`/SkillTool).
    pub fn store(&self) -> &Arc<SkillMetadataStore> {
        &self.store
    }

    /// Access the underlying skill registry.
    pub fn registry(&self) -> &Arc<SkillRegistry> {
        &self.registry
    }

    /// The active lifecycle config.
    pub fn config(&self) -> SkillLifecycleConfig {
        self.store.config()
    }

    /// The exempt set for automatic transitions: pinned + Bundled + referenced.
    async fn exempt_set(&self) -> HashSet<String> {
        let referenced = self.referenced.read().unwrap().clone();
        let mut exempt = referenced;
        for (name, m) in self.store.list().await {
            if m.pinned || m.created_by == SkillAuthor::Bundled {
                exempt.insert(name);
            }
        }
        exempt
    }

    /// Run the automatic transitions pass. Writes a backup snapshot *before*
    /// archiving anything (so every retirement is one-step reversible), then
    /// applies [`SkillMetadataStore::apply_automatic_transitions`].
    pub async fn run(&self, now: SystemTime) -> CuratorReport {
        // Seed metadata for every registered skill so fresh skills get a
        // `created_at` (and a grace window) before their first transition.
        let live = self.registry.list().await;
        let now_s = now_unix();
        for s in &live {
            self.store.ensure(&s.name, SkillAuthor::User, now_s).await;
        }
        let exempt = self.exempt_set().await;

        // Snapshot before any archiving — restorable retirement.
        let will_archive_maybe = self.has_archive_candidates(&live, &exempt, now_s).await;
        let mut backup_written = false;
        if will_archive_maybe {
            let path = self.store.write_backup(&live, now).await;
            backup_written = !path.as_os_str().is_empty();
        }

        let changed = self.store.apply_automatic_transitions(now, &exempt).await;
        let mut gone_stale = Vec::new();
        let mut archived = Vec::new();
        for name in changed {
            match self
                .store
                .get(&name)
                .await
                .map(|m| m.state)
                .unwrap_or(SkillState::Active)
            {
                SkillState::Archived => archived.push(name),
                SkillState::Stale => gone_stale.push(name),
                SkillState::Active => {}
            }
        }
        CuratorReport {
            gone_stale,
            archived,
            backup_written,
        }
    }

    // ─── LLM consolidation (Phase 2.1 Stage C) ────────────────────────────
    //
    // `apply_automatic_transitions` (Stage B) is the *pure-function* half of
    // the Hermes curator — age-based Active→Stale→Archived. The *LLM* half
    // merges narrow one-session skills into class-level umbrella skills
    // ("hundreds of narrow skills each capturing one session is a library
    // failure, not a feature"). The LLM orchestration lives in `oneai-agent`
    // (it needs a provider); these are the data-layer primitives the runner
    // calls. Default-off / opt-in — never invoked from `run` or any
    // cadence-fired path; only `oneai curator consolidate` triggers it.

    /// Narrow Active skills that are safe consolidation candidates: not
    /// pinned, not Bundled, not referenced by the active pack's workflows /
    /// StateGraphs / cron. The LLM decides *which* of these to merge; this
    /// just scopes the input so the proposer can't suggest retiring an
    /// exempt skill.
    pub async fn consolidation_candidates(&self) -> Vec<(SkillDescriptor, SkillMetadata)> {
        let referenced = self.referenced.read().unwrap().clone();
        let live = self.registry.list().await;
        let meta = self.store.list().await;
        let now_s = now_unix();
        let mut out = Vec::new();
        for s in live {
            if referenced.contains(&s.name) {
                continue;
            }
            let m = meta
                .get(&s.name)
                .cloned()
                .unwrap_or_else(|| SkillMetadata::fresh(SkillAuthor::User, now_s));
            if m.state != SkillState::Active || m.pinned || m.created_by == SkillAuthor::Bundled {
                continue;
            }
            out.push((s, m));
        }
        out
    }

    /// Apply one LLM-proposed umbrella merge: write the umbrella `SKILL.md`
    /// into `skills_dir`, register it, seed its metadata as `Agent`-authored,
    /// then archive each member (one shared backup first → the whole merge
    /// is one-step reversible via [`Self::rollback`]). Members referenced by
    /// the active pack are skipped with a warning, not an error.
    pub async fn apply_merge(
        &self,
        umbrella: SkillDescriptor,
        members: &[String],
        skills_dir: &Path,
    ) -> Result<MergeReport, MergeError> {
        // Refuse to clobber an existing skill via merge.
        if self.registry.find_by_name(&umbrella.name).await.is_some() {
            return Err(MergeError::UmbrellaExists(umbrella.name));
        }
        // Validate every member is registered before touching anything.
        for m in members {
            if self.registry.find_by_name(m).await.is_none() {
                return Err(MergeError::NotFound(m.clone()));
            }
        }

        // Write the umbrella SKILL.md (frontmatter + body), reusing the same
        // materialization format as `rollback` so discovery re-finds it
        // identically next session.
        let skill_dir: PathBuf = skills_dir.join(&umbrella.name);
        std::fs::create_dir_all(&skill_dir).map_err(|e| MergeError::Io(e.to_string()))?;
        let body = if umbrella.prompt_template.is_empty() {
            String::new()
        } else {
            umbrella.prompt_template.clone()
        };
        let frontmatter = format!(
            "---\nname: {name}\ndescription: {desc}\n---\n",
            name = umbrella.name,
            desc = umbrella.description.replace('\n', " ")
        );
        std::fs::write(skill_dir.join("SKILL.md"), format!("{frontmatter}{body}"))
            .map_err(|e| MergeError::Io(e.to_string()))?;

        // Register + seed metadata.
        self.registry.register(umbrella.clone()).await.ok();
        let now_s = now_unix();
        self.store
            .ensure(&umbrella.name, SkillAuthor::Agent, now_s)
            .await;

        // One shared backup before retiring any member → whole merge
        // reversible via a single rollback.
        let live = self.registry.list().await;
        let backup_path = self.store.write_backup(&live, SystemTime::now()).await;
        let backup_id = backup_path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.trim_end_matches(".json").parse::<u64>().ok())
            .unwrap_or_else(now_unix);

        let referenced = self.referenced.read().unwrap().clone();
        let mut members_archived = Vec::new();
        let mut members_skipped = Vec::new();
        for m in members {
            if referenced.contains(m) {
                tracing::warn!(
                    "consolidate: skipping member '{m}' — referenced by active pack's \
                     workflows/StateGraphs/cron; reconcile the pack or unreference first."
                );
                members_skipped.push(m.clone());
                continue;
            }
            self.store.archive(m, now_s).await;
            members_archived.push(m.clone());
        }

        Ok(MergeReport {
            umbrella_name: umbrella.name,
            members_archived,
            members_skipped,
            backup_id,
        })
    }

    /// Whether any skill would cross into Archived this run (cheap pre-check
    /// so we only snapshot when a restorable retirement is imminent).
    async fn has_archive_candidates(
        &self,
        live: &[SkillDescriptor],
        exempt: &HashSet<String>,
        now_s: u64,
    ) -> bool {
        let cfg = self.store.config();
        if !cfg.auto_transitions {
            return false;
        }
        let archive = cfg.archive_after.as_secs();
        let grace = cfg.grace_unused.as_secs();
        let meta = self.store.list().await;
        for s in live {
            if exempt.contains(&s.name) {
                continue;
            }
            let Some(m) = meta.get(&s.name) else {
                continue;
            };
            if m.state == SkillState::Archived || m.pinned || m.created_by == SkillAuthor::Bundled {
                continue;
            }
            let idle = if m.use_count > 0 {
                m.last_activity_at
                    .map(|t| now_s.saturating_sub(t))
                    .unwrap_or(0)
            } else {
                let age = now_s.saturating_sub(m.created_at);
                if age < grace {
                    continue;
                }
                age
            };
            if idle >= archive {
                return true;
            }
        }
        false
    }

    /// A per-skill status row joining the registry's descriptors with the
    /// store's metadata. Skills present in the registry but absent from the
    /// store are reported with a fresh `Active` metadata (not yet recorded).
    pub async fn status(&self) -> Vec<(SkillDescriptor, SkillMetadata)> {
        let now_s = now_unix();
        let mut live = self.registry.list().await;
        live.sort_by(|a, b| a.name.cmp(&b.name));
        let mut out = Vec::with_capacity(live.len());
        for s in live {
            let m = self
                .store
                .get(&s.name)
                .await
                .unwrap_or_else(|| SkillMetadata::fresh(SkillAuthor::User, now_s));
            out.push((s, m));
        }
        out
    }

    /// Pin a skill (exempt from automatic retirement).
    pub async fn pin(&self, name: &str) -> SkillMetadata {
        self.store.set_pinned(name, true, now_unix()).await
    }

    /// Unpin a skill.
    pub async fn unpin(&self, name: &str) -> SkillMetadata {
        self.store.set_pinned(name, false, now_unix()).await
    }

    /// Manually archive a skill. Warns (does NOT refuse) when the skill is
    /// referenced by the active pack — destructive workflow-rewrite is out of
    /// scope; the human reconciles. Writes a backup first so this is
    /// reversible.
    pub async fn archive(&self, name: &str) -> SkillMetadata {
        let referenced = self.referenced.read().unwrap().clone();
        if referenced.contains(name) {
            tracing::warn!(
                "curator: archiving skill '{}' which is referenced by the active pack's \
                 workflows/StateGraphs/cron — workflows may reference a now-hidden skill. \
                 Reconcile the pack or restore the skill. (Destructive rewrite is out of scope.)",
                name
            );
        }
        // Snapshot before the manual retirement.
        let live = self.registry.list().await;
        self.store.write_backup(&live, SystemTime::now()).await;
        self.store.archive(name, now_unix()).await
    }

    /// Restore an archived skill to Active.
    pub async fn restore(&self, name: &str) -> SkillMetadata {
        self.store.restore(name, now_unix()).await
    }

    /// Write a named backup snapshot of every registered skill. Returns the
    /// backup id (unix timestamp) — pass to [`Self::rollback`].
    pub async fn backup(&self) -> u64 {
        let live = self.registry.list().await;
        let path = self.store.write_backup(&live, SystemTime::now()).await;
        // The id is the timestamp embedded in the filename.
        path.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.trim_end_matches(".json").parse::<u64>().ok())
            .unwrap_or_else(now_unix)
    }

    /// List available backup snapshot ids (unix timestamps), newest first.
    pub fn list_backups(&self) -> Vec<u64> {
        self.store.list_backups()
    }

    /// Restore a backup snapshot: re-materializes each skill's `SKILL.md` into
    /// `restore_dir` and replaces the metadata index with the snapshot's.
    pub async fn rollback(&self, id: u64, restore_dir: &Path) -> Result<usize, RollbackError> {
        self.store.rollback(id, restore_dir).await
    }
}

// `RollbackError` is re-exported from `lifecycle`; alias here for ergonomics.
pub use crate::lifecycle::RollbackError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::SkillLifecycleConfig;
    use std::path::PathBuf;

    fn make_skill(name: &str) -> SkillDescriptor {
        SkillDescriptor {
            name: name.into(),
            description: format!("desc {name}"),
            prompt_template: "body".into(),
            trigger_keywords: vec!["k".into()],
            embedding: None,
        }
    }

    fn tmp_root() -> PathBuf {
        let name = std::thread::current().name().unwrap_or("test").to_string();
        let mut h: u64 = 1469598103934665603;
        for b in name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let p = std::env::temp_dir().join(format!("oneai-skill-cur-{h:x}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn days(secs: u64) -> std::time::Duration {
        std::time::Duration::from_secs(secs)
    }

    #[tokio::test]
    async fn run_archives_idle_skill_and_writes_backup() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(make_skill("idle")).await.unwrap();
        let cfg = SkillLifecycleConfig {
            stale_after: days(10),
            archive_after: days(20),
            grace_unused: days(0),
            ..Default::default()
        };
        let store = Arc::new(SkillMetadataStore::new(tmp_root(), cfg));
        store.bump_use("idle", 0).await; // last used at t=0
        let curator = SkillCurator::new(registry, Arc::clone(&store), HashSet::new());

        let report = curator.run(SystemTime::UNIX_EPOCH + days(365)).await;
        assert_eq!(report.archived, vec!["idle".to_string()]);
        assert!(report.backup_written);
        assert!(!curator.list_backups().is_empty());
        assert_eq!(store.get("idle").await.unwrap().state, SkillState::Archived);
    }

    #[tokio::test]
    async fn referenced_skill_exempt_from_run_and_warns_on_manual_archive() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(make_skill("ref")).await.unwrap();
        let cfg = SkillLifecycleConfig {
            stale_after: days(10),
            archive_after: days(20),
            grace_unused: days(0),
            ..Default::default()
        };
        let store = Arc::new(SkillMetadataStore::new(tmp_root(), cfg));
        store.bump_use("ref", 0).await;
        let mut referenced = HashSet::new();
        referenced.insert("ref".to_string());
        let curator = SkillCurator::new(registry, Arc::clone(&store), referenced);

        // Automatic run leaves it alone.
        let report = curator.run(SystemTime::UNIX_EPOCH + days(365)).await;
        assert!(report.archived.is_empty());
        assert_eq!(store.get("ref").await.unwrap().state, SkillState::Active);

        // Manual archive still works (warns) — referenced is a warning, not a block.
        let m = curator.archive("ref").await;
        assert_eq!(m.state, SkillState::Archived);
        // And it's reversible.
        let m = curator.restore("ref").await;
        assert_eq!(m.state, SkillState::Active);
    }

    #[tokio::test]
    async fn status_joins_descriptor_and_metadata() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(make_skill("a")).await.unwrap();
        registry.register(make_skill("b")).await.unwrap();
        let store = Arc::new(SkillMetadataStore::new(
            tmp_root(),
            SkillLifecycleConfig::default(),
        ));
        store.bump_use("a", 123).await;
        let curator = SkillCurator::new(registry, store, HashSet::new());
        let rows = curator.status().await;
        assert_eq!(rows.len(), 2);
        let a = rows.iter().find(|(s, _)| s.name == "a").unwrap();
        assert_eq!(a.1.use_count, 1);
        let b = rows.iter().find(|(s, _)| s.name == "b").unwrap();
        assert_eq!(b.1.use_count, 0); // never recorded → fresh
        assert_eq!(b.1.state, SkillState::Active);
    }

    #[tokio::test]
    async fn backup_and_rollback_round_trip() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(make_skill("foo")).await.unwrap();
        let store = Arc::new(SkillMetadataStore::new(
            tmp_root(),
            SkillLifecycleConfig::default(),
        ));
        store.bump_use("foo", 100).await;
        let curator = SkillCurator::new(registry, Arc::clone(&store), HashSet::new());

        let id = curator.backup().await;
        // Mutate then rollback.
        curator.archive("foo").await;
        assert_eq!(store.get("foo").await.unwrap().state, SkillState::Archived);
        let restore_dir = store.root().join("restore");
        let n = curator.rollback(id, &restore_dir).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.get("foo").await.unwrap().state, SkillState::Active);
        assert!(restore_dir.join("foo/SKILL.md").exists());
    }

    #[tokio::test]
    async fn apply_merge_writes_umbrella_and_archives_members() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(make_skill("narrow-a")).await.unwrap();
        registry.register(make_skill("narrow-b")).await.unwrap();
        let store = Arc::new(SkillMetadataStore::new(
            tmp_root(),
            SkillLifecycleConfig::default(),
        ));
        // Both members active + non-exempt → candidates.
        store.ensure("narrow-a", SkillAuthor::User, 100).await;
        store.ensure("narrow-b", SkillAuthor::User, 100).await;
        let curator = SkillCurator::new(registry, Arc::clone(&store), HashSet::new());

        let candidates = curator.consolidation_candidates().await;
        assert_eq!(candidates.len(), 2);

        let umbrella = SkillDescriptor {
            name: "umbrella".into(),
            description: "covers a+b".into(),
            prompt_template: "merged body".into(),
            trigger_keywords: vec!["k".into()],
            embedding: None,
        };
        let skills_dir = store.root().join("skills");
        let report = curator
            .apply_merge(
                umbrella,
                &["narrow-a".to_string(), "narrow-b".to_string()],
                &skills_dir,
            )
            .await
            .expect("merge applies");
        assert_eq!(report.umbrella_name, "umbrella");
        assert_eq!(report.members_archived.len(), 2);
        assert!(report.members_archived.contains(&"narrow-a".to_string()));
        assert!(report.members_archived.contains(&"narrow-b".to_string()));

        // Umbrella registered + SKILL.md written + authored by Agent.
        assert!(curator.registry().find_by_name("umbrella").await.is_some());
        assert!(skills_dir.join("umbrella/SKILL.md").exists());
        assert_eq!(
            store.get("umbrella").await.unwrap().created_by,
            SkillAuthor::Agent
        );
        // Members archived.
        assert_eq!(
            store.get("narrow-a").await.unwrap().state,
            SkillState::Archived
        );
        assert_eq!(
            store.get("narrow-b").await.unwrap().state,
            SkillState::Archived
        );

        // Whole merge reversible via the shared backup id.
        let restore_dir = store.root().join("restore");
        curator
            .rollback(report.backup_id, &restore_dir)
            .await
            .unwrap();
        assert_eq!(
            store.get("narrow-a").await.unwrap().state,
            SkillState::Active
        );
    }

    #[tokio::test]
    async fn apply_merge_refuses_to_clobber_existing_umbrella() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(make_skill("umbrella")).await.unwrap();
        registry.register(make_skill("m")).await.unwrap();
        let store = Arc::new(SkillMetadataStore::new(
            tmp_root(),
            SkillLifecycleConfig::default(),
        ));
        let curator = SkillCurator::new(registry, store, HashSet::new());
        let err = curator
            .apply_merge(
                make_skill("umbrella"),
                &["m".to_string()],
                Path::new("/tmp"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MergeError::UmbrellaExists(_)));
    }

    #[tokio::test]
    async fn apply_merge_skips_referenced_members() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(make_skill("narrow")).await.unwrap();
        registry.register(make_skill("ref")).await.unwrap();
        let store = Arc::new(SkillMetadataStore::new(
            tmp_root(),
            SkillLifecycleConfig::default(),
        ));
        let mut referenced = HashSet::new();
        referenced.insert("ref".to_string());
        let curator = SkillCurator::new(registry, Arc::clone(&store), referenced);
        // Seed metadata for both members so post-merge `get` resolves.
        store.ensure("narrow", SkillAuthor::User, 100).await;
        store.ensure("ref", SkillAuthor::User, 100).await;

        let report = curator
            .apply_merge(
                SkillDescriptor {
                    name: "umb".into(),
                    description: "u".into(),
                    prompt_template: "b".into(),
                    trigger_keywords: vec![],
                    embedding: None,
                },
                &["narrow".to_string(), "ref".to_string()],
                &store.root().join("skills"),
            )
            .await
            .unwrap();
        // `narrow` archived; `ref` skipped (referenced).
        assert_eq!(report.members_archived, vec!["narrow".to_string()]);
        assert_eq!(report.members_skipped, vec!["ref".to_string()]);
        assert_eq!(store.get("ref").await.unwrap().state, SkillState::Active);
    }
}
