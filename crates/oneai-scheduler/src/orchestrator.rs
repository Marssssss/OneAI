//! The cron orchestrator — a [`CronScheduler`] provider backed by a
//! [`JobStore`] + a delivery [`CronRunner`] seam + a ticker loop.
//!
//! This is the "C" (Conductor) in the ABC: one orchestrator over pluggable
//! backends. [`CronSchedulerImpl::start`] spawns a tokio task that ticks
//! [`CronSchedulerImpl::fire_due`] every [`tick_interval`](Self::tick_interval)
//! — scan the store, take each due job through the store's
//! [`cas_mark_fired`](JobStore::cas_mark_fired) at-most-once CAS point, and
//! deliver it via the [`CronRunner`].
//!
//! The external one-shot HTTP receiver ([`crate::oneshot`]) drives the same
//! [`CronSchedulerImpl`] — a `POST /cron/fire?job=...` calls
//! [`CronSchedulerImpl::trigger`] which re-uses the CAS path so an external
//! trigger and the ticker never double-fire a window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use oneai_core::error::Result as CoreResult;
use oneai_core::traits::CronScheduler;

use crate::error::Result;
use crate::job::CronJob;
use crate::runner::{CronRunner, DeliveryOutcome};
use crate::store::JobStore;

/// The durable cron orchestrator.
pub struct CronSchedulerImpl {
    store: Arc<dyn JobStore>,
    runner: Arc<dyn CronRunner>,
    tick: Duration,
    started: AtomicBool,
    /// Holds the ticker task so callers can stop it (supervisor shutdown).
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CronSchedulerImpl {
    /// Build with a store backend + delivery runner. Default tick = 30s (the
    /// orchestrator re-checks due jobs every 30s; cron granularity is
    /// minute-level so 30s is ample and cheap).
    pub fn new(store: Arc<dyn JobStore>, runner: Arc<dyn CronRunner>) -> Self {
        Self::with_tick(store, runner, Duration::from_secs(30))
    }

    /// Override the tick interval (e.g. 1s for tests).
    pub fn with_tick(
        store: Arc<dyn JobStore>,
        runner: Arc<dyn CronRunner>,
        tick: Duration,
    ) -> Self {
        Self {
            store,
            runner,
            tick,
            started: AtomicBool::new(false),
            handle: Mutex::new(None),
        }
    }

    /// The store backend (for CLI `add`/`list`/`rm`).
    pub fn store(&self) -> &Arc<dyn JobStore> {
        &self.store
    }

    /// The tick interval.
    pub fn tick_interval(&self) -> Duration {
        self.tick
    }

    /// Fire all due jobs at `now` (scan → CAS → deliver). Returns the count
    /// delivered. Driven by the ticker AND callable directly (tests, manual).
    pub async fn fire_due(&self, now: DateTime<Utc>) -> Result<u32> {
        let jobs = self.store.list().await?;
        let mut fired = 0u32;
        for job in jobs {
            // The CAS point — at-most-once per fire window.
            match self.store.cas_mark_fired(&job.id, now).await {
                Ok(Some(snapshot)) => {
                    fired += 1;
                    self.deliver(snapshot).await;
                }
                Ok(None) => {} // not due / disabled / not found
                Err(e) => {
                    warn!(job = %job.id, error = %e, "cas_mark_fired failed");
                }
            }
        }
        Ok(fired)
    }

    /// Trigger a specific job by id now (used by the external `/cron/fire`
    /// receiver + CLI `cron fire <id>`). Routes through the same CAS path so
    /// it can't double-fire the current window.
    pub async fn trigger(&self, id: &str, now: DateTime<Utc>) -> Result<bool> {
        match self.store.cas_mark_fired(id, now).await? {
            Some(snapshot) => {
                self.deliver(snapshot).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Deliver a fired job, mapping outcomes to logs (delivery failure does
    /// NOT roll back the CAS — at-most-once fire; a lost delivery is logged).
    async fn deliver(&self, job: CronJob) {
        debug!(job = %job.id, name = %job.name, "delivering fired job");
        match self.runner.deliver(&job).await {
            Ok(DeliveryOutcome::Done { reply, iterations }) => {
                debug!(job = %job.id, iterations, reply_len = reply.len(), "job delivered");
            }
            Ok(DeliveryOutcome::Rejected { reason }) => {
                warn!(job = %job.id, reason = %reason, "delivery rejected");
            }
            Ok(DeliveryOutcome::Error { message }) => {
                warn!(job = %job.id, message = %message, "delivery error");
            }
            Err(e) => {
                warn!(job = %job.id, error = %e, "delivery failed");
            }
        }
    }
}

#[async_trait]
impl CronScheduler for CronSchedulerImpl {
    fn name(&self) -> &str {
        "file"
    }

    async fn start(&self) -> CoreResult<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(()); // idempotent
        }
        // Reconcile: re-arm next_fire for any job missing it (loaded from a
        // pre-Phase-3.2 store, or a one-shot whose schedule shifted).
        self.reconcile()
            .await
            .map_err(|e| oneai_core::error::OneAIError::Other(format!("cron reconcile: {e}")))?;

        let tick = self.tick;
        // Clone a lightweight `Arc<Self>` for the spawned ticker task. The
        // store + runner are shared Arcs, so this clone faithfully delivers
        // without owning the task's own JoinHandle (which stays with `self`).
        let store_for_task = self.store.clone();
        let me = self.clone_arc();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            interval.tick().await; // skip immediate
            loop {
                interval.tick().await;
                let now = Utc::now();
                let jobs = match store_for_task.list().await {
                    Ok(j) => j,
                    Err(e) => {
                        warn!(error = %e, "cron tick: list failed");
                        continue;
                    }
                };
                for job in jobs {
                    match store_for_task.cas_mark_fired(&job.id, now).await {
                        Ok(Some(snapshot)) => {
                            me.deliver(snapshot).await;
                        }
                        Ok(None) => {}
                        Err(e) => warn!(job = %job.id, error = %e, "cron tick: cas failed"),
                    }
                }
            }
        });
        *self.handle.lock().await = Some(handle);
        info!(tick_ms = tick.as_millis() as u64, "cron scheduler started");
        Ok(())
    }

    async fn fire_due(&self, now: DateTime<Utc>) -> CoreResult<u32> {
        CronSchedulerImpl::fire_due(self, now)
            .await
            .map_err(|e| oneai_core::error::OneAIError::Other(format!("cron fire_due: {e}")))
    }

    async fn reconcile(&self) -> CoreResult<()> {
        let now = Utc::now();
        let mut jobs = self.store.list().await.map_err(|e| {
            oneai_core::error::OneAIError::Other(format!("cron reconcile list: {e}"))
        })?;
        let mut changed = false;
        for job in jobs.iter_mut() {
            if job.next_fire_at.is_none() {
                job.next_fire_at = job.schedule.next_fire_after(now);
                let _ = self.store.upsert(job.clone()).await;
                changed = true;
            }
        }
        let _ = changed;
        Ok(())
    }
}

impl CronSchedulerImpl {
    /// Clone the orchestrator into an `Arc<Self>` for the spawned ticker task.
    /// Only safe to call once (during start); the resulting arc shares the
    /// store + runner + handle with `self`.
    fn clone_arc(&self) -> Arc<CronSchedulerImpl> {
        // Reconstruct a shareable handle WITHOUT the JoinHandle (the spawned
        // task must not own its own handle). The store + runner are shared
        // Arcs, so this is a faithful lightweight clone for delivery.
        Arc::new(CronSchedulerImpl {
            store: self.store.clone(),
            runner: self.runner.clone(),
            tick: self.tick,
            started: AtomicBool::new(true),
            handle: Mutex::new(None),
        })
    }
}

/// Stop the ticker (for tests / supervisor shutdown). Idempotent.
pub async fn stop(sched: &CronSchedulerImpl) {
    if let Some(h) = sched.handle.lock().await.take() {
        h.abort();
    }
}

/// Add a job to a store, computing `next_fire_at` from the schedule relative
/// to `now` if unset. Convenience for the CLI.
pub async fn add_job(
    store: &Arc<dyn JobStore>,
    mut job: CronJob,
    now: DateTime<Utc>,
) -> Result<()> {
    if job.next_fire_at.is_none() {
        job.next_fire_at = job.schedule.next_fire_after(now);
    }
    store.upsert(job).await
}
/// A no-op [`CronRunner`] that records deliveries (for tests + the `Silent`
/// delivery path when no gateway is wired).
pub struct NoopCronRunner {
    pub fired: std::sync::Mutex<Vec<String>>,
}

impl NoopCronRunner {
    pub fn new() -> Self {
        Self {
            fired: std::sync::Mutex::new(Vec::new()),
        }
    }
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.fired.lock().unwrap())
    }
}

impl Default for NoopCronRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CronRunner for NoopCronRunner {
    async fn deliver(&self, job: &CronJob) -> Result<DeliveryOutcome> {
        self.fired.lock().unwrap().push(job.id.clone());
        Ok(DeliveryOutcome::Done {
            reply: String::new(),
            iterations: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{parse_schedule, CronJob};
    use crate::store::InMemoryJobStore;
    use chrono::Utc;
    use std::time::Duration;

    #[tokio::test]
    async fn fire_due_delivers_due_job_once() {
        let store: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
        let mut j = CronJob::new("j1", "j1", parse_schedule("*/5 * * * *").unwrap());
        j.task = "hi".into();
        j.next_fire_at = Some(Utc::now());
        store.upsert(j).await.unwrap();

        let noop = Arc::new(NoopCronRunner::new());
        let runner: Arc<dyn CronRunner> = noop.clone();
        let sched = CronSchedulerImpl::with_tick(store.clone(), runner, Duration::from_secs(1));

        let n = sched.fire_due(Utc::now()).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(noop.take(), vec!["j1".to_string()]);
        // Same instant: not due again (advanced).
        let n2 = sched.fire_due(Utc::now()).await.unwrap();
        assert_eq!(n2, 0);
    }

    #[tokio::test]
    async fn trigger_uses_cas_path() {
        let store: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
        let mut j = CronJob::new("j1", "j1", parse_schedule("30m").unwrap());
        j.task = "hi".into();
        j.next_fire_at = Some(Utc::now());
        store.upsert(j).await.unwrap();

        let noop = Arc::new(NoopCronRunner::new());
        let runner: Arc<dyn CronRunner> = noop.clone();
        let sched = CronSchedulerImpl::with_tick(store.clone(), runner, Duration::from_secs(1));

        assert!(sched.trigger("j1", Utc::now()).await.unwrap());
        // Second trigger in same window: CAS rejects (advanced).
        assert!(!sched.trigger("j1", Utc::now()).await.unwrap());
    }
}
