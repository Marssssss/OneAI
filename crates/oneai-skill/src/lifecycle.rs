//! Skill lifecycle — durable per-skill metadata + automatic transitions + a
//! restorable backup snapshot (Phase 2.1 Stage B).
//!
//! OneAI skills are discovered from convention directories (see
//! [`crate::discovery`]) and registered in [`SkillRegistry`]. Until Stage B
//! they were stateless: every skill was equally "on" forever, there was no
//! record of how often each was used, and retiring a skill meant deleting its
//! file. This module adds the **lifecycle** dimension that makes the agent
//! "grow with you" (Hermes-style):
//!
//! - **`SkillState`** — `Active` / `Stale` / `Archived`. Skills never get
//!   *deleted*; an unused skill ages into `Stale` then `Archived`, and an
//!   archived skill is hidden from the model's skill menu but stays on disk
//!   and is restorable.
//! - **`SkillMetadata`** — per-skill `use_count`, `last_activity_at`,
//!   `pinned`, `created_by` (Agent / User / Bundled), `origin_note`. The
//!   `SkillTool` bumps `use_count` on each activation; the reflect sub-agent
//!   (and the model via `skill_manage`) sets `created_by = Agent` when it
//!   authors a skill.
//! - **`apply_automatic_transitions`** — a pure function that ages skills per
//!   a [`SkillLifecycleConfig`] (30d → Stale, 90d → Archived), with pinned
//!   skills and skills still referenced by cron/workflows exempt, and a grace
//!   window for never-used skills.
//! - **`SkillMetadataStore`** — durable JSON persistence (`metadata.json`)
//!   plus rotating `.json.gz` backup snapshots (kept to `backup_count`),
//!   individually restorable via [`SkillMetadataStore::rollback`].
//!
//! The store stays decoupled from the DomainPack layer: it takes plain
//! primitives ([`SkillLifecycleConfig`]) rather than the declarative
//! `SkillLifecyclePolicy` (which lives in `oneai-domain` and is folded into
//! `MemoryProfile` as a layer-7 sub-config, mirroring `WorkingStatePolicy`).
//! `oneai-agent`/`oneai-app` reads the policy and hands the primitives in.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use oneai_core::SkillDescriptor;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

// ─── SkillState / SkillAuthor ─────────────────────────────────────────────────

/// Lifecycle state of a single skill.
///
/// Skills age `Active → Stale → Archived` and are never hard-deleted — an
/// archived skill is hidden from the model's skill menu but stays on disk and
/// is restorable. `#[non_exhaustive]` per the v0.2.0 API-stability commitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillState {
    /// Healthy and shown in the skill menu.
    Active,
    /// Unused for `stale_after` but not yet `archive_after` — still shown,
    /// flagged as stale so the curator (and reflect sub-agent) can decide
    /// whether to retire or refresh it.
    Stale,
    /// Retired: hidden from the model's skill menu, kept on disk + in the
    /// backup snapshots, restorable via `skill_manage` / `oneai curator`.
    Archived,
}

/// Who authored a skill — drives lifecycle exemptions and provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillAuthor {
    /// Dropped into a convention dir by the human user (or discovered).
    #[default]
    User,
    /// Authored by the agent itself — via the `skill-creator` skill, the
    /// reflect sub-agent's `skill_manage` tool, or a delegation.
    Agent,
    /// Shipped with OneAI (the built-in `skill-creator` + coding/research
    /// presets). Bundled skills are pinned by default so the curator never
    /// auto-archives the always-on skill-creator.
    Bundled,
}

// ─── SkillMetadata ───────────────────────────────────────────────────────────

/// Durable per-skill lifecycle metadata. Keyed by skill name in the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub state: SkillState,
    /// Times the `SkillTool` activated this skill.
    #[serde(default)]
    pub use_count: u64,
    /// Unix-epoch seconds of the last `bump_use` (None = never used).
    #[serde(default)]
    pub last_activity_at: Option<u64>,
    /// Pinned skills are exempt from automatic stale/archive transitions and
    /// never appear in `curator run` retire candidates.
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub created_by: SkillAuthor,
    /// Free-form provenance — e.g. `"reflect"` for a learning the background
    /// review sub-agent persisted, `"delegated:plan"` for a handoff. Only set
    /// when `created_by == Agent`.
    #[serde(default)]
    pub origin_note: String,
    /// Unix-epoch seconds of first-seen (when the store first recorded it).
    #[serde(default)]
    pub created_at: u64,
    /// Unix-epoch seconds when the skill was archived (None = not archived).
    #[serde(default)]
    pub archived_at: Option<u64>,
}

impl Default for SkillMetadata {
    fn default() -> Self {
        Self {
            state: SkillState::Active,
            use_count: 0,
            last_activity_at: None,
            pinned: false,
            created_by: SkillAuthor::User,
            origin_note: String::new(),
            created_at: 0,
            archived_at: None,
        }
    }
}

impl SkillMetadata {
    /// Metadata for a skill newly seen by the store.
    pub(crate) fn fresh(created_by: SkillAuthor, now: u64) -> Self {
        Self {
            created_by,
            created_at: now,
            ..Default::default()
        }
    }
}

// ─── SkillLifecycleConfig ────────────────────────────────────────────────────

/// Primitive lifecycle thresholds — the decoupled counterpart to the
/// DomainPack-level `SkillLifecyclePolicy`. The store consumes this so
/// `oneai-skill` need not depend on `oneai-domain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillLifecycleConfig {
    /// Unused-for duration that ages a skill `Active → Stale`.
    pub stale_after: Duration,
    /// Unused-for duration that ages a skill `Stale → Archived`.
    pub archive_after: Duration,
    /// How many rotating backup snapshots to keep on disk.
    pub backup_count: usize,
    /// Whether `run` applies automatic transitions at all (off = record-only).
    pub auto_transitions: bool,
    /// Grace window: a never-used skill (`use_count == 0`) is not aged within
    /// its first `grace_unused` since `created_at` (give newly-authored
    /// skills time to be discovered).
    pub grace_unused: Duration,
}

impl Default for SkillLifecycleConfig {
    fn default() -> Self {
        Self {
            stale_after: Duration::from_secs(30 * 24 * 3600),
            archive_after: Duration::from_secs(90 * 24 * 3600),
            backup_count: 5,
            auto_transitions: true,
            grace_unused: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

// ─── SkillMetadataStore ──────────────────────────────────────────────────────

/// Durable per-skill lifecycle metadata + rotating restorable backups.
///
/// The metadata index lives at `<root>/metadata.json`; backups at
/// `<root>/backups/<unix_ts>.json.gz`. The store is async (a single `RwLock`
/// around the in-memory map); writes persist synchronously to disk.
pub struct SkillMetadataStore {
    root: PathBuf,
    config: SkillLifecycleConfig,
    inner: RwLock<HashMap<String, SkillMetadata>>,
}

impl SkillMetadataStore {
    /// Create a store rooted at `root` (created lazily on first persist). The
    /// index is *not* loaded here — call [`Self::load`] to hydrate from disk.
    pub fn new(root: impl Into<PathBuf>, config: SkillLifecycleConfig) -> Self {
        Self {
            root: root.into(),
            config,
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// The curator root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The active lifecycle config.
    pub fn config(&self) -> SkillLifecycleConfig {
        self.config
    }

    /// Load the metadata index from `<root>/metadata.json`. Missing/corrupt
    /// files reset to an empty index (logged) — never panics the agent.
    pub async fn load(&self) {
        let path = self.root.join("metadata.json");
        let map = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<HashMap<String, SkillMetadata>>(&bytes)
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "skill metadata at {} corrupt, resetting: {e}",
                        path.display()
                    );
                    HashMap::new()
                }),
            Err(_) => HashMap::new(),
        };
        *self.inner.write().await = map;
    }

    /// Persist the in-memory index to `<root>/metadata.json` (best-effort;
    /// IO errors are logged, not propagated — metadata is advisory).
    pub async fn persist(&self) {
        let map = self.inner.read().await.clone();
        let bytes = match serde_json::to_vec(&map) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("skill metadata serialize failed: {e}");
                return;
            }
        };
        if let Err(e) = std::fs::create_dir_all(&self.root) {
            tracing::warn!("skill metadata create_dir failed: {e}");
            return;
        }
        let path = self.root.join("metadata.json");
        if let Err(e) = std::fs::write(&path, bytes) {
            tracing::warn!("skill metadata persist to {} failed: {e}", path.display());
        }
    }

    /// Read metadata for a skill, or `None` if the store has never recorded it.
    pub async fn get(&self, name: &str) -> Option<SkillMetadata> {
        self.inner.read().await.get(name).cloned()
    }

    /// Seed a list of known-bundled skill names: mark provenance `Bundled` and,
    /// **only on first sight**, pin them so the always-on skill-creator is
    /// never auto-archived. A skill the user has since unpinned keeps its
    /// `pinned = false` — the seeding must not clobber a deliberate override.
    /// Idempotent across builds.
    pub async fn seed_bundled(&self, names: &[&str], now: u64) {
        for name in names {
            let existed = self.get(name).await.is_some();
            self.ensure(name, SkillAuthor::Bundled, now).await;
            if !existed {
                self.set_pinned(name, true, now).await;
            }
        }
    }

    /// Whole-index snapshot (cloned) — used by the curator for status/report.
    pub async fn list(&self) -> HashMap<String, SkillMetadata> {
        self.inner.read().await.clone()
    }

    /// Ensure a skill has a metadata entry; freshly-seen skills are seeded
    /// with `created_by` and `created_at = now`. Returns the (possibly new)
    /// metadata by value.
    pub async fn ensure(&self, name: &str, created_by: SkillAuthor, now: u64) -> SkillMetadata {
        let mut guard = self.inner.write().await;
        let entry = guard
            .entry(name.to_string())
            .or_insert_with(|| SkillMetadata::fresh(created_by, now));
        // If the entry predates a known author (e.g. a User-discovered skill
        // later recognized as Bundled), upgrade provenance but never downgrade.
        if created_by == SkillAuthor::Bundled && entry.created_by != SkillAuthor::Bundled {
            entry.created_by = SkillAuthor::Bundled;
        }
        if entry.created_at == 0 {
            entry.created_at = now;
        }
        entry.clone()
    }

    /// Record a `SkillTool` activation: `use_count += 1`, `last_activity_at =
    /// now`, and lift an archived/stale skill back to `Active` (use is the
    /// strongest "this skill is alive" signal).
    pub async fn bump_use(&self, name: &str, now: u64) {
        let mut guard = self.inner.write().await;
        let m = guard
            .entry(name.to_string())
            .or_insert_with(|| SkillMetadata::fresh(SkillAuthor::User, now));
        m.use_count = m.use_count.saturating_add(1);
        m.last_activity_at = Some(now);
        if m.state != SkillState::Active {
            tracing::info!(
                "skill '{}' was {}, bump_use reviving to Active",
                name,
                serde_json::to_string(&m.state).unwrap_or_default()
            );
        }
        m.state = SkillState::Active;
        m.archived_at = None;
        drop(guard);
        self.persist().await;
    }

    /// Pin / unpin a skill (pinned = exempt from auto-transitions).
    pub async fn set_pinned(&self, name: &str, pinned: bool, now: u64) -> SkillMetadata {
        let mut guard = self.inner.write().await;
        let m = guard
            .entry(name.to_string())
            .or_insert_with(|| SkillMetadata::fresh(SkillAuthor::User, now));
        m.pinned = pinned;
        let out = m.clone();
        drop(guard);
        self.persist().await;
        out
    }

    /// Force-archive a skill (manual `skill_manage` / `curator archive`).
    /// Idempotent. Returns the updated metadata.
    pub async fn archive(&self, name: &str, now: u64) -> SkillMetadata {
        let mut guard = self.inner.write().await;
        let m = guard
            .entry(name.to_string())
            .or_insert_with(|| SkillMetadata::fresh(SkillAuthor::User, now));
        m.state = SkillState::Archived;
        m.archived_at = Some(now);
        let out = m.clone();
        drop(guard);
        self.persist().await;
        out
    }

    /// Restore an archived skill to `Active` (manual `skill_manage` /
    /// `curator restore`). Idempotent.
    pub async fn restore(&self, name: &str, now: u64) -> SkillMetadata {
        let mut guard = self.inner.write().await;
        let m = guard
            .entry(name.to_string())
            .or_insert_with(|| SkillMetadata::fresh(SkillAuthor::User, now));
        m.state = SkillState::Active;
        m.archived_at = None;
        m.last_activity_at = Some(now);
        let out = m.clone();
        drop(guard);
        self.persist().await;
        out
    }

    // ─── Automatic transitions ───────────────────────────────────────────────

    /// Apply `Active → Stale → Archived` aging per the config, **in place**.
    ///
    /// Exemptions (a skill is never aged by this pass when any holds):
    /// - `pinned`
    /// - `created_by == Bundled` (the always-on skill-creator etc.)
    /// - listed in `exempt` (skills the caller knows are cron-/workflow-
    ///   referenced — the curator computes this from the live DomainPack)
    /// - never used AND still inside `grace_unused` since `created_at`
    ///   (give freshly-authored skills a chance to be discovered)
    ///
    /// Returns the names that changed state (for the curator's report).
    pub async fn apply_automatic_transitions(
        &self,
        now: SystemTime,
        exempt: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        if !self.config.auto_transitions {
            return Vec::new();
        }
        let now_s = to_unix(now);
        let grace = self.config.grace_unused.as_secs();
        let stale = self.config.stale_after.as_secs();
        let archive = self.config.archive_after.as_secs();
        let mut changed = Vec::new();
        let mut guard = self.inner.write().await;
        for (name, m) in guard.iter_mut() {
            if m.pinned || m.created_by == SkillAuthor::Bundled || exempt.contains(name) {
                continue;
            }
            if m.state == SkillState::Archived {
                continue;
            }
            // Idle seconds: time since last use, or — for never-used skills —
            // time since creation. Never-used skills get a grace window so a
            // freshly-authored skill isn't archived before the model discovers
            // it. A skill with unknown creation (`created_at == 0`, legacy
            // data) at a real timestamp ages as `now_s` → archived, which is
            // the safe default for stale unknowns.
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
            let next = if idle >= archive {
                SkillState::Archived
            } else if idle >= stale {
                SkillState::Stale
            } else {
                SkillState::Active
            };
            if next != m.state {
                if next == SkillState::Archived {
                    m.archived_at = Some(now_s);
                } else {
                    m.archived_at = None;
                }
                m.state = next;
                changed.push(name.clone());
            }
        }
        drop(guard);
        if !changed.is_empty() {
            self.persist().await;
        }
        changed
    }

    // ─── Backup snapshots ─────────────────────────────────────────────────────

    /// Write a restorable snapshot of the given skills (their full
    /// `SkillDescriptor` content + current metadata) to
    /// `<root>/backups/<unix_ts>.json.gz`, then prune to `backup_count`.
    ///
    /// The snapshot is built from the in-memory registry list, not the
    /// filesystem, so it captures exactly what the agent sees — including
    /// skills authored in-session via `skill_manage` that may not yet be on
    /// disk. `.json.gz` (not `.tar.gz`) keeps this crate free of a `tar`
    /// dependency; the capability — a restorable N-deep rotating snapshot —
    /// is identical.
    pub async fn write_backup(&self, skills: &[SkillDescriptor], now: SystemTime) -> PathBuf {
        let meta = self
            .inner
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<HashMap<_, _>>();
        let snapshot = BackupSnapshot {
            taken_at: to_unix(now),
            skills: skills.to_vec(),
            metadata: meta,
        };
        let bytes = match serde_json::to_vec(&snapshot) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("skill backup serialize failed: {e}");
                return PathBuf::new();
            }
        };
        let backups_dir = self.root.join("backups");
        let _ = std::fs::create_dir_all(&backups_dir);
        let path = backups_dir.join(format!("{}.json.gz", snapshot.taken_at));
        if write_gz(&path, &bytes).is_err() {
            return PathBuf::new();
        }
        self.prune_backups();
        path
    }

    /// List available backup snapshot ids (unix timestamps), newest first.
    pub fn list_backups(&self) -> Vec<u64> {
        let dir = self.root.join("backups");
        let mut ids = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(stem) = name.strip_suffix(".json.gz") {
                    if let Ok(ts) = stem.parse::<u64>() {
                        ids.push(ts);
                    }
                }
            }
        }
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids
    }

    /// Read a backup snapshot by id (unix timestamp).
    pub fn read_backup(&self, id: u64) -> Option<BackupSnapshot> {
        let path = self.root.join("backups").join(format!("{id}.json.gz"));
        let bytes = read_gz(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Restore a backup snapshot: re-materializes each skill's `SKILL.md` into
    /// `restore_dir` and replaces the in-memory metadata index with the
    /// snapshot's, then persists. Returns the number of skills restored.
    ///
    /// `restore_dir` is typically the user's global skills dir (`~/.oneai/skills/`).
    /// Existing skill files of the same name are overwritten.
    pub async fn rollback(&self, id: u64, restore_dir: &Path) -> Result<usize, RollbackError> {
        let snapshot = self
            .read_backup(id)
            .ok_or(RollbackError::BackupNotFound(id))?;
        let _ = std::fs::create_dir_all(restore_dir);
        for skill in &snapshot.skills {
            let skill_dir = restore_dir.join(&skill.name);
            std::fs::create_dir_all(&skill_dir).map_err(|e| RollbackError::Io(e.to_string()))?;
            // Re-materialize as a SKILL.md with frontmatter + body so the
            // discovery pass picks it back up identically.
            let body = if skill.prompt_template.is_empty() {
                String::new()
            } else {
                skill.prompt_template.clone()
            };
            let frontmatter = format!(
                "---\nname: {name}\ndescription: {desc}\n---\n",
                name = skill.name,
                desc = skill.description.replace('\n', " ")
            );
            std::fs::write(skill_dir.join("SKILL.md"), format!("{frontmatter}{body}"))
                .map_err(|e| RollbackError::Io(e.to_string()))?;
        }
        // Replace the metadata index wholesale with the snapshot's.
        *self.inner.write().await = snapshot.metadata.clone();
        self.persist().await;
        Ok(snapshot.skills.len())
    }

    /// Keep only the newest `backup_count` snapshots.
    fn prune_backups(&self) {
        let mut ids = self.list_backups();
        if ids.len() <= self.config.backup_count {
            return;
        }
        let drop_ids = ids.split_off(self.config.backup_count);
        let dir = self.root.join("backups");
        for id in drop_ids {
            let _ = std::fs::remove_file(dir.join(format!("{id}.json.gz")));
        }
    }
}

/// Error restoring a backup snapshot.
#[derive(Debug)]
#[non_exhaustive]
pub enum RollbackError {
    /// No backup snapshot with the given id exists.
    BackupNotFound(u64),
    /// Filesystem IO during re-materialization.
    Io(String),
}

impl std::fmt::Display for RollbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BackupNotFound(id) => write!(f, "no backup snapshot with id {id}"),
            Self::Io(s) => write!(f, "rollback IO error: {s}"),
        }
    }
}

impl std::error::Error for RollbackError {}

// ─── BackupSnapshot ──────────────────────────────────────────────────────────

/// A restorable point-in-time snapshot of every skill the agent saw at backup
/// time, plus the metadata index then.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSnapshot {
    /// Unix-epoch seconds when the snapshot was taken (also the backup id).
    pub taken_at: u64,
    /// Full skill descriptors (name + description + prompt + keywords + embedding).
    pub skills: Vec<SkillDescriptor>,
    /// Metadata index at snapshot time.
    pub metadata: HashMap<String, SkillMetadata>,
}

// ─── helpers ────────────────────────────────────────────────────────────────

/// `SystemTime` → unix seconds (0 if before epoch, which never happens here).
pub fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `now` as unix seconds.
pub fn now_unix() -> u64 {
    to_unix(SystemTime::now())
}

fn write_gz(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let f = std::fs::File::create(path)?;
    let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
    enc.write_all(bytes)?;
    enc.finish()?;
    Ok(())
}

fn read_gz(path: &Path) -> std::io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mut dec = flate2::read::GzDecoder::new(f);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        let name = std::thread::current().name().unwrap_or("test").to_string();
        let mut h: u64 = 1469598103934665603;
        for b in name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let p = std::env::temp_dir().join(format!("oneai-skill-life-{h:x}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn days(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[tokio::test]
    async fn bump_use_increments_and_revives() {
        let store = SkillMetadataStore::new(tmp_root(), SkillLifecycleConfig::default());
        store.bump_use("debug-analysis", 1000).await;
        store.bump_use("debug-analysis", 2000).await;
        let m = store.get("debug-analysis").await.unwrap();
        assert_eq!(m.use_count, 2);
        assert_eq!(m.last_activity_at, Some(2000));
        assert_eq!(m.state, SkillState::Active);

        // Archive then bump revives.
        store.archive("debug-analysis", 3000).await;
        assert_eq!(
            store.get("debug-analysis").await.unwrap().state,
            SkillState::Archived
        );
        store.bump_use("debug-analysis", 4000).await;
        let m = store.get("debug-analysis").await.unwrap();
        assert_eq!(m.state, SkillState::Active);
        assert!(m.archived_at.is_none());
    }

    #[tokio::test]
    async fn transitions_age_active_to_stale_to_archived() {
        let cfg = SkillLifecycleConfig {
            stale_after: days(10),
            archive_after: days(100),
            ..Default::default()
        };
        let store = SkillMetadataStore::new(tmp_root(), cfg);
        // A skill used once at t=0.
        store.bump_use("foo", 0).await;

        // t=5 (< stale_after 10) → still Active.
        let changed = store
            .apply_automatic_transitions(SystemTime::UNIX_EPOCH + days(5), &Default::default())
            .await;
        assert!(changed.is_empty());
        assert_eq!(store.get("foo").await.unwrap().state, SkillState::Active);

        // t=20 (>= stale_after 10, < archive_after 100) → Stale.
        let changed = store
            .apply_automatic_transitions(SystemTime::UNIX_EPOCH + days(20), &Default::default())
            .await;
        assert_eq!(changed, vec!["foo".to_string()]);
        assert_eq!(store.get("foo").await.unwrap().state, SkillState::Stale);

        // t=200 (>= archive_after 100) → Archived.
        let changed = store
            .apply_automatic_transitions(SystemTime::UNIX_EPOCH + days(200), &Default::default())
            .await;
        assert_eq!(changed, vec!["foo".to_string()]);
        assert_eq!(store.get("foo").await.unwrap().state, SkillState::Archived);
        assert!(store.get("foo").await.unwrap().archived_at.is_some());
    }

    #[tokio::test]
    async fn pinned_and_bundled_exempt_from_aging() {
        let cfg = SkillLifecycleConfig {
            stale_after: days(10),
            archive_after: days(20),
            ..Default::default()
        };
        let store = SkillMetadataStore::new(tmp_root(), cfg);
        store.bump_use("pinned-one", 0).await;
        store.set_pinned("pinned-one", true, 0).await;
        store.bump_use("bundled-one", 0).await;
        store.ensure("bundled-one", SkillAuthor::Bundled, 0).await;

        let changed = store
            .apply_automatic_transitions(SystemTime::UNIX_EPOCH + days(365), &Default::default())
            .await;
        assert!(
            changed.is_empty(),
            "pinned/bundled must not age: {changed:?}"
        );
        assert_eq!(
            store.get("pinned-one").await.unwrap().state,
            SkillState::Active
        );
        assert_eq!(
            store.get("bundled-one").await.unwrap().state,
            SkillState::Active
        );
    }

    #[tokio::test]
    async fn exempt_set_protects_referenced_skills() {
        let cfg = SkillLifecycleConfig {
            stale_after: days(10),
            archive_after: days(20),
            ..Default::default()
        };
        let store = SkillMetadataStore::new(tmp_root(), cfg);
        store.bump_use("cron-ref", 0).await;
        let mut exempt = std::collections::HashSet::new();
        exempt.insert("cron-ref".to_string());

        let changed = store
            .apply_automatic_transitions(SystemTime::UNIX_EPOCH + days(365), &exempt)
            .await;
        assert!(changed.is_empty());
        assert_eq!(
            store.get("cron-ref").await.unwrap().state,
            SkillState::Active
        );
    }

    #[tokio::test]
    async fn grace_window_protects_never_used_fresh_skill() {
        let cfg = SkillLifecycleConfig {
            stale_after: days(1),
            archive_after: days(2),
            grace_unused: days(10),
            ..Default::default()
        };
        let store = SkillMetadataStore::new(tmp_root(), cfg);
        // Skill seen at t=0, never used.
        store.ensure("fresh", SkillAuthor::User, 0).await;
        // t=3: past stale/archive thresholds but inside grace (use_count==0,
        // created_at=0 → 3 < 10) → no aging.
        let changed = store
            .apply_automatic_transitions(SystemTime::UNIX_EPOCH + days(3), &Default::default())
            .await;
        assert!(
            changed.is_empty(),
            "fresh skill inside grace aged: {changed:?}"
        );
        // t=365: well past grace → archived (use_count==0, created_at long ago).
        let changed = store
            .apply_automatic_transitions(SystemTime::UNIX_EPOCH + days(365), &Default::default())
            .await;
        assert_eq!(changed, vec!["fresh".to_string()]);
    }

    #[tokio::test]
    async fn persist_and_reload_round_trip() {
        let root = tmp_root();
        {
            let store = SkillMetadataStore::new(root.clone(), SkillLifecycleConfig::default());
            store.bump_use("persist-me", 1234).await;
            store.set_pinned("persist-me", true, 1234).await;
            // bump_use / set_pinned persist implicitly; nothing else to do.
        }
        // Re-open with a fresh store handle.
        let store = SkillMetadataStore::new(root, SkillLifecycleConfig::default());
        store.load().await;
        let m = store.get("persist-me").await.unwrap();
        assert_eq!(m.use_count, 1);
        assert!(m.pinned);
        assert_eq!(m.last_activity_at, Some(1234));
    }

    #[tokio::test]
    async fn backup_write_list_read_rollback_round_trip() {
        let root = tmp_root();
        let store = SkillMetadataStore::new(root.clone(), SkillLifecycleConfig::default());
        store.bump_use("foo", 100).await;

        let skills = vec![SkillDescriptor {
            name: "foo".into(),
            description: "d".into(),
            prompt_template: "body text".into(),
            trigger_keywords: vec!["k".into()],
            embedding: None,
        }];
        let path = store
            .write_backup(&skills, SystemTime::UNIX_EPOCH + days(5))
            .await;
        assert!(path.exists());
        let ids = store.list_backups();
        assert_eq!(ids.len(), 1);

        // Mutate, then rollback.
        store.archive("foo", 999).await;
        assert_eq!(store.get("foo").await.unwrap().state, SkillState::Archived);

        let restore_dir = root.join("restore-target");
        let n = store.rollback(ids[0], &restore_dir).await.unwrap();
        assert_eq!(n, 1);
        // Metadata index restored: foo back to the snapshot's state (Active,
        // use_count 1, last_activity_at 100).
        let m = store.get("foo").await.unwrap();
        assert_eq!(m.state, SkillState::Active);
        assert_eq!(m.use_count, 1);
        // SKILL.md re-materialized.
        let md = std::fs::read_to_string(restore_dir.join("foo/SKILL.md")).unwrap();
        assert!(md.contains("name: foo"));
        assert!(md.contains("body text"));
    }

    #[tokio::test]
    async fn backup_prune_keeps_only_n() {
        let cfg = SkillLifecycleConfig {
            backup_count: 3,
            ..Default::default()
        };
        let store = SkillMetadataStore::new(tmp_root(), cfg);
        let skill = SkillDescriptor {
            name: "x".into(),
            description: "d".into(),
            prompt_template: "".into(),
            trigger_keywords: vec![],
            embedding: None,
        };
        for i in 0..6 {
            store
                .write_backup(
                    std::slice::from_ref(&skill),
                    SystemTime::UNIX_EPOCH + days(i + 1),
                )
                .await;
        }
        assert_eq!(store.list_backups().len(), 3);
        // Newest 3 kept (i=5,4,3 → ts = 6,5,4 days).
        let ids = store.list_backups();
        assert!(ids[0] > ids[1] && ids[1] > ids[2]);
    }
}
