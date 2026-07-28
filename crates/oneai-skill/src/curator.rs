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
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use oneai_core::SkillDescriptor;

use crate::lifecycle::{
    now_unix, SkillAuthor, SkillLifecycleConfig, SkillMetadata, SkillMetadataStore, SkillState,
};
use crate::registry::SkillRegistry;

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
}
