//! # OneAI Scheduler
//!
//! Task scheduling with platform heartbeat support.
//!
//! The InMemoryScheduler provides core-layer scheduling using tokio timers.
//! Platform-specific implementations (Android WorkManager, HarmonyOS WorkScheduler,
//! desktop daemon) will be provided in Phase 6.

pub mod scheduler;

pub use scheduler::*;
pub use oneai_core::ScheduledTask;
pub use oneai_core::TaskHandle;