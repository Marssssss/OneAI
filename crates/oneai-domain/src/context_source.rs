//! ContextSource trait — pluggable domain-specific environment sensing.
//!
//! Each domain has its own set of context sources that provide environment
//! information to the agent. Coding domains inject git status and file trees;
//! research domains inject date and web search caches; IoT domains inject
//! device registries and sensor readings.
//!
//! ContextSources are independently refreshable with different policies:
//! - EveryIteration: refresh on each loop iteration (high-frequency data)
//! - OnChange: refresh only when a diff is detected (incremental updates)
//! - OnceAtStart: load once and never refresh (stable data)
//! - Periodic: refresh at a fixed interval

use std::time::Duration;

use async_trait::async_trait;
use oneai_core::error::Result;

// ─── RefreshPolicy ─────────────────────────────────────────────────────────────

/// Policy for when a ContextSource should refresh its data.
///
/// Different sources have different stability characteristics:
/// - Git status changes frequently → EveryIteration or OnChange
/// - Project config (Cargo.toml) rarely changes → OnceAtStart
/// - Current date changes daily → Periodic(24h)
/// - Device registry is stable → OnceAtStart
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshPolicy {
    /// Always refresh on each loop iteration.
    /// Use for frequently-changing data like git status.
    EveryIteration,

    /// Refresh only when a diff is detected compared to the previous snapshot.
    /// More efficient than EveryIteration — only produces new tokens when data changes.
    /// Inspired by OpenCode's Context Epoch incremental update mechanism.
    OnChange,

    /// Load once at session start, then never refresh.
    /// Use for stable data like project config, database schema.
    OnceAtStart,

    /// Fire once, on the first turn after a session **resumes or continues**
    /// an existing task — the resume-time ground-truth reconciliation pass
    /// (reference doc §8.2). Under the ephemeral re-injection model the
    /// assembler calls `load()` every turn, so an `OnResume` source is itself
    /// responsible for yielding its content once and then empty (the take
    /// pattern, mirroring `UnfinishedWorkSource`).
    OnResume,

    /// Refresh at a fixed time interval.
    /// Use for data that changes on a known schedule, like date/time.
    Periodic(Duration),
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self::EveryIteration
    }
}

// ─── ContextSource Trait ───────────────────────────────────────────────────────

/// Trait for domain-specific context sources.
///
/// Each ContextSource provides environment information that gets injected
/// into the agent's conversation as system messages. The source determines:
/// - What information to provide (via `load()`)
/// - When to refresh (via `refresh_policy()`)
/// - Where in the injection order (via `priority()`)
///
/// Implementations may hold internal state (previous snapshots for diffing,
/// cached data) using internal `RwLock<Option<...>>` or similar mechanisms.
/// The trait methods themselves do not expose mutable state.
#[async_trait]
pub trait ContextSource: Send + Sync {
    /// Unique key for this context source.
    ///
    /// Used for deduplication and identification. Examples:
    /// - "git_status" — current git branch/status/commits
    /// - "file_tree" — project file structure
    /// - "project_config" — Cargo.toml/package.json metadata
    /// - "date" — current date and time
    /// - "device_registry" — IoT device list
    fn key(&self) -> &str;

    /// Load the current state as a string for injection into the conversation.
    ///
    /// The returned string is injected as a system message with the format:
    /// `[Context: {key}] {content}`
    ///
    /// Implementations should be efficient — if the data hasn't changed,
    /// OnChange sources should detect this and return the same string
    /// (the ContextAssembler will skip injection if content matches previous).
    async fn load(&self) -> Result<String>;

    /// The refresh policy for this source.
    ///
    /// Determines when `load()` should be called during the agent loop.
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::EveryIteration
    }

    /// Priority for injection order.
    ///
    /// Lower numbers are injected earlier and have higher priority.
    /// Sources with the same priority are injected in registration order.
    fn priority(&self) -> u32 {
        100 // Default priority — moderate
    }
}
