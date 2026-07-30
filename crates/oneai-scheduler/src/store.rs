//! Job store — the durable registry of cron jobs + the at-most-once CAS fire
//! point.
//!
//! `JobStore` is the "A" in the ABC (Abstract / Backend / Conductor): pluggable
//! backends, one orchestrator. Two backends ship:
//! - [`InMemoryJobStore`] — zero-config default; restart-dies (mirrors
//!   `InMemoryScheduler`).
//! - [`FileJobStore`] — `<root>/cron/jobs.json` atomic-rewrite (write tmp +
//!   rename), crash-safe. The store's [`JobStore::cas_mark_fired`] is the
//!   single CAS point: the ticker and the external `/cron/fire` receiver both
//!   route through it, so a fire window is taken at most once even under
//!   concurrent triggers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tracing::warn;

use crate::error::{CronError, Result};
use crate::job::CronJob;

/// The job-store seam.
#[async_trait]
pub trait JobStore: Send + Sync {
    /// Upsert a job (by id). The caller is responsible for `next_fire_at`.
    async fn upsert(&self, job: CronJob) -> Result<()>;

    /// Remove a job by id. Returns whether it existed.
    async fn remove(&self, id: &str) -> Result<bool>;

    /// Get a job by id.
    async fn get(&self, id: &str) -> Result<Option<CronJob>>;

    /// All jobs (the orchestrator scans this on each tick).
    async fn list(&self) -> Result<Vec<CronJob>>;

    /// The at-most-once CAS fire point (due-based — used by the ticker).
    ///
    /// If a job with `id` exists, is `enabled`, and is due
    /// (`next_fire_at` is `Some(t)` with `t <= now`), atomically: set
    /// `last_fired_at = now`, advance `next_fire_at` to the schedule's next
    /// fire after `now` (`None` for a one-shot already past → the job is done),
    /// persist, and return the job as it was at fire time. Returns `None` if
    /// not eligible (not due / disabled / not found). `next_fire_at == None`
    /// means "not armed / already fired one-shot" — never due here.
    async fn cas_mark_fired(&self, id: &str, now: DateTime<Utc>) -> Result<Option<CronJob>>;

    /// Force-fire a job regardless of due-ness (used by manual `cron fire`
    /// + the external `/cron/fire` receiver). Fires iff the job exists and is
    /// `enabled`; sets `last_fired_at = now` and advances `next_fire_at`. The
    /// caller is responsible for burst dedup (an external scheduler firing
    /// once is inherently at-most-once). Returns the snapshot or `None`.
    async fn force_fire(&self, id: &str, now: DateTime<Utc>) -> Result<Option<CronJob>>;
}

// ─── In-memory ───────────────────────────────────────────────────────────────

/// Zero-config in-memory store (restart-dies). Default for tests + CI.
pub struct InMemoryJobStore {
    jobs: RwLock<HashMap<String, CronJob>>,
}

impl InMemoryJobStore {
    pub fn new() -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryJobStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl JobStore for InMemoryJobStore {
    async fn upsert(&self, job: CronJob) -> Result<()> {
        self.jobs.write().await.insert(job.id.clone(), job);
        Ok(())
    }
    async fn remove(&self, id: &str) -> Result<bool> {
        Ok(self.jobs.write().await.remove(id).is_some())
    }
    async fn get(&self, id: &str) -> Result<Option<CronJob>> {
        Ok(self.jobs.read().await.get(id).cloned())
    }
    async fn list(&self) -> Result<Vec<CronJob>> {
        Ok(self.jobs.read().await.values().cloned().collect())
    }
    async fn cas_mark_fired(&self, id: &str, now: DateTime<Utc>) -> Result<Option<CronJob>> {
        let mut jobs = self.jobs.write().await;
        let job = match jobs.get_mut(id) {
            Some(j) => j,
            None => return Ok(None),
        };
        if !job.enabled {
            return Ok(None);
        }
        let due = job.next_fire_at.is_some_and(|t| t <= now);
        if !due {
            return Ok(None);
        }
        let snapshot = job.clone();
        job.last_fired_at = Some(now);
        job.next_fire_at = job.schedule.next_fire_after(now);
        Ok(Some(snapshot))
    }
    async fn force_fire(&self, id: &str, now: DateTime<Utc>) -> Result<Option<CronJob>> {
        let mut jobs = self.jobs.write().await;
        let job = match jobs.get_mut(id) {
            Some(j) => j,
            None => return Ok(None),
        };
        if !job.enabled {
            return Ok(None);
        }
        let snapshot = job.clone();
        job.last_fired_at = Some(now);
        job.next_fire_at = job.schedule.next_fire_after(now);
        Ok(Some(snapshot))
    }
}

// ─── File-backed ─────────────────────────────────────────────────────────────

/// File-backed store. `<root>/cron/jobs.json` — a single JSON map
/// `{ job_id: CronJob }` rewritten atomically (write `jobs.json.tmp` then
/// rename). Adequate for cron (few jobs, low write rate); mirrors the
/// crash-safe atomic-rewrite discipline of the gateway's `ChannelDirectory`.
pub struct FileJobStore {
    root: PathBuf,
    jobs: RwLock<HashMap<String, CronJob>>,
}

impl FileJobStore {
    /// Open (or create) a store rooted at `root`. The `cron/` subdir + file are
    /// created lazily on first write. Existing jobs are loaded so the
    /// orchestrator re-arms after a restart.
    pub async fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let jobs = load_jobs(&root).await?;
        Ok(Self {
            root,
            jobs: RwLock::new(jobs),
        })
    }

    fn jobs_path(&self) -> PathBuf {
        self.root.join("cron").join("jobs.json")
    }

    async fn persist(&self) -> Result<()> {
        let map = self.jobs.read().await.clone();
        write_jobs_atomic(&self.jobs_path(), &map).await
    }
}

async fn load_jobs(root: &Path) -> Result<HashMap<String, CronJob>> {
    let path = root.join("cron").join("jobs.json");
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            if bytes.trim_ascii().is_empty() {
                return Ok(HashMap::new());
            }
            match serde_json::from_slice::<HashMap<String, CronJob>>(&bytes) {
                Ok(m) => Ok(m),
                Err(e) => {
                    warn!(
                        "cron jobs.json corrupt at '{}': {} — starting empty",
                        path.display(),
                        e
                    );
                    Ok(HashMap::new())
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(CronError::Store(format!(
            "read jobs.json '{}': {}",
            path.display(),
            e
        ))),
    }
}

async fn write_jobs_atomic(path: &Path, map: &HashMap<String, CronJob>) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| CronError::Store(format!("mkdir '{}': {}", parent.display(), e)))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(map)?;
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| CronError::Store(format!("write '{}': {}", tmp.display(), e)))?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| CronError::Store(format!("rename '{}': {}", path.display(), e)))?;
    Ok(())
}

#[async_trait]
impl JobStore for FileJobStore {
    async fn upsert(&self, job: CronJob) -> Result<()> {
        self.jobs.write().await.insert(job.id.clone(), job);
        self.persist().await
    }
    async fn remove(&self, id: &str) -> Result<bool> {
        let existed = self.jobs.write().await.remove(id).is_some();
        if existed {
            self.persist().await?;
        }
        Ok(existed)
    }
    async fn get(&self, id: &str) -> Result<Option<CronJob>> {
        Ok(self.jobs.read().await.get(id).cloned())
    }
    async fn list(&self) -> Result<Vec<CronJob>> {
        Ok(self.jobs.read().await.values().cloned().collect())
    }
    async fn cas_mark_fired(&self, id: &str, now: DateTime<Utc>) -> Result<Option<CronJob>> {
        // CAS the in-memory copy under the write lock, then persist if changed.
        let snapshot = {
            let mut jobs = self.jobs.write().await;
            let job = match jobs.get_mut(id) {
                Some(j) => j,
                None => return Ok(None),
            };
            if !job.enabled {
                return Ok(None);
            }
            let due = job.next_fire_at.is_some_and(|t| t <= now);
            if !due {
                return Ok(None);
            }
            let snap = job.clone();
            job.last_fired_at = Some(now);
            job.next_fire_at = job.schedule.next_fire_after(now);
            snap
        };
        self.persist().await?;
        Ok(Some(snapshot))
    }
    async fn force_fire(&self, id: &str, now: DateTime<Utc>) -> Result<Option<CronJob>> {
        let snapshot = {
            let mut jobs = self.jobs.write().await;
            let job = match jobs.get_mut(id) {
                Some(j) => j,
                None => return Ok(None),
            };
            if !job.enabled {
                return Ok(None);
            }
            let snap = job.clone();
            job.last_fired_at = Some(now);
            job.next_fire_at = job.schedule.next_fire_after(now);
            snap
        };
        self.persist().await?;
        Ok(Some(snapshot))
    }
}

/// Default root: `~/.oneai` (mirrors the gateway / supervisor / working-state).
pub fn default_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".oneai")
}

/// Convenience: an `Arc<dyn JobStore>` in-memory (for tests / zero-config runs).
pub fn in_memory() -> Arc<dyn JobStore> {
    Arc::new(InMemoryJobStore::new())
}

/// Convenience: an `Arc<dyn JobStore>` backed by `FileJobStore` at `root`.
pub async fn file_at(root: impl Into<PathBuf>) -> Result<Arc<dyn JobStore>> {
    Ok(Arc::new(FileJobStore::new(root).await?))
}

/// Convenience: re-arm `next_fire_at` for a job whose schedule may have changed
/// (called by the orchestrator after an upsert that didn't set it).
pub fn recompute_next_fire(job: &mut CronJob, now: DateTime<Utc>) {
    if job.next_fire_at.is_none() {
        job.next_fire_at = job.schedule.next_fire_after(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::parse_schedule;
    use chrono::Utc;

    fn sample_job(id: &str, sched: &str) -> CronJob {
        let schedule = parse_schedule(sched).unwrap();
        let mut j = CronJob::new(id.to_string(), id.to_string(), schedule);
        j.next_fire_at = j.schedule.next_fire_after(Utc::now());
        j.task = "hello".into();
        j
    }

    #[tokio::test]
    async fn in_memory_upsert_get_remove() {
        let s = InMemoryJobStore::new();
        s.upsert(sample_job("j1", "30m")).await.unwrap();
        assert!(s.get("j1").await.unwrap().is_some());
        assert!(s.remove("j1").await.unwrap());
        assert!(s.get("j1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cas_marks_fired_and_advances() {
        let s = InMemoryJobStore::new();
        let mut j = sample_job("j1", "*/5 * * * *");
        // Force it due now.
        j.next_fire_at = Some(Utc::now());
        s.upsert(j.clone()).await.unwrap();
        let now = Utc::now();
        let fired = s.cas_mark_fired("j1", now).await.unwrap();
        assert!(fired.is_some());
        // Second call in the same instant: not due again (advanced).
        let again = s.cas_mark_fired("j1", now).await.unwrap();
        assert!(again.is_none());
        let after = s.get("j1").await.unwrap().unwrap();
        assert!(after.next_fire_at.unwrap() > now);
    }

    #[tokio::test]
    async fn cas_skips_disabled() {
        let s = InMemoryJobStore::new();
        let mut j = sample_job("j1", "*/5 * * * *");
        j.enabled = false;
        j.next_fire_at = Some(Utc::now());
        s.upsert(j).await.unwrap();
        assert!(s.cas_mark_fired("j1", Utc::now()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn file_store_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let s = FileJobStore::new(tmp.path()).await.unwrap();
            s.upsert(sample_job("j1", "30m")).await.unwrap();
        }
        // Reopen: job persists.
        let s = FileJobStore::new(tmp.path()).await.unwrap();
        assert!(s.get("j1").await.unwrap().is_some());
        assert_eq!(s.list().await.unwrap().len(), 1);
    }
}
