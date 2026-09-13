//! # OneAI Persistence
//!
//! State persistence and checkpoint management for agent loop recovery.
//! New: SqliteSessionStore for STM/LTM/conversation persistence (session resume).
//! New: SqliteUsageTracker for persistent token-usage tracking.
//! New: FileWorkingStateStore — per-task append-only event log (the cross-session
//!   working-state substrate, replacing the old progressive-checkpoint manager).
//! New (feature `postgres`): PgWorkingStateStore — the same event log in a
//!   shared Postgres with a transactional brief index (MVS3 cloud storage
//!   externalization; runtime-selected via `ONEAI_PG_DSN`).

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.

//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.

pub mod checkpoint;
pub mod host_allowlist;
#[cfg(feature = "postgres")]
mod pg_common;
#[cfg(feature = "postgres")]
pub mod pg_host_allowlist;
#[cfg(feature = "postgres")]
pub mod pg_memory_store;
#[cfg(feature = "postgres")]
pub mod pg_usage_tracker;
#[cfg(feature = "postgres")]
pub mod pg_working_state_store;
pub mod session_event_store;
pub mod sqlite_store;
pub mod state;
pub mod thinking_effort;
pub mod usage_tracker;
pub mod working_state_store;

pub use checkpoint::*;
pub use host_allowlist::*;
#[cfg(feature = "postgres")]
pub use pg_host_allowlist::PgHostAllowlist;
#[cfg(feature = "postgres")]
pub use pg_memory_store::PgMemoryStore;
#[cfg(feature = "postgres")]
pub use pg_usage_tracker::PgUsageTracker;
#[cfg(feature = "postgres")]
pub use pg_working_state_store::PgWorkingStateStore;
pub use session_event_store::*;
pub use sqlite_store::*;
pub use state::*;
pub use thinking_effort::*;
pub use usage_tracker::*;
pub use working_state_store::*;
