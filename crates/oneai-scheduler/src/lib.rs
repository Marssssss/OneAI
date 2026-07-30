//! # OneAI Scheduler — in-memory timers + durable cron orchestration
//!
//! Phase 3.2 (evolution-plan §3.2). The scheduler grows from a single
//! restart-dies [`scheduler::InMemoryScheduler`] (the core-layer
//! [`TaskScheduler`](oneai_core::traits::TaskScheduler) impl) into a durable
//! cron orchestration stack:
//!
//! - [`CronScheduler`] trait ([`oneai_core::traits::CronScheduler`]) — the
//!   minimal host seam (`name`/`start` + safe defaults), held by
//!   `AppBuilder::cron_provider`.
//! - [`Schedule`] + [`parse_schedule`] — `"30m"` / `"every 2h"` / ISO /
//!   5-field cron, with [`Schedule::next_fire_after`].
//! - [`JobStore`] ABC: [`InMemoryJobStore`] (zero-config default) +
//!   [`FileJobStore`] (`<root>/cron/jobs.json` atomic-rewrite). The store's
//!   [`cas_mark_fired`](JobStore::cas_mark_fired) is the at-most-once CAS point.
//! - [`CronRunner`] — the delivery seam (mirrors `GatewayRunner`); the CLI
//!   routes a fired job into the gateway's `deliver_scheduled` (`deliver=origin`).
//! - [`CronSchedulerImpl`] — the orchestrator (the "C"): a ticker that scans
//!   the store, takes each due job through the CAS point, and delivers it.
//! - `oneshot` (feature `oneshot-http`) — external one-shot trigger receiver:
//!   `POST /cron/fire` (shared-secret bearer) + `provision(...)` outbound
//!   registration (feature `oneshot-provision`).
//!
//! The crate stays *below* `oneai-app` (only `oneai-core` dep) — it cannot call
//! `run_agent` directly; the CLI supplies the [`CronRunner`] impl that drives a
//! real `App` turn via the gateway.
//!
//! ## Stability
//!
//! Public enums carry `#[non_exhaustive]` per the v0.2.0+ stability commitment.

pub mod error;
pub mod job;
pub mod orchestrator;
pub mod runner;
pub mod scheduler;
pub mod store;

#[cfg(feature = "oneshot-http")]
pub mod oneshot;

pub use error::{CronError, Result};
pub use job::{parse_schedule, CronJob, DeliverMode, Schedule};
pub use oneai_core::traits::CronScheduler;
pub use orchestrator::{add_job, stop, CronSchedulerImpl, NoopCronRunner};
pub use runner::{CronRunner, DeliveryOutcome};
pub use store::{default_root, file_at, in_memory, FileJobStore, InMemoryJobStore, JobStore};

/// Re-export of the core-layer in-memory `TaskScheduler` (unchanged — kept as
/// the zero-config default for non-cron one-shot/periodic scheduling).
pub use scheduler::InMemoryScheduler;
