//! Progressive checkpoint manager — auto-save per iteration with multiple backends.
//!
//! This addresses Issue #16: checkpoints are currently snapshot-based
//! (manual `save_checkpoint()` calls), not progressive (auto-save per step).
//!
//! Key improvements:
//! - Auto-save per iteration (configurable: EveryStep / EveryNSteps / CriticalNodes)
//! - Multiple backends: Memory / SQLite / Postgres
//! - Support for interrupt: pause execution at any node
//! - Support for replay/fork from any checkpoint
//!
//! Inspired by LangGraph's MemorySaver which auto-saves state after
//! every graph node execution, enabling recovery from any step.

use std::sync::Arc;
use async_trait::async_trait;

use oneai_core::error::Result;
use oneai_core::AgentState;

// ─── AutoSavePolicy ─────────────────────────────────────────────────────────

/// Policy for automatic checkpoint saving.
///
/// Controls when checkpoints are automatically saved during agent execution:
/// - EveryStep: safest, most storage-intensive
/// - EveryNSteps: balanced (save every N iterations)
/// - CriticalNodes: saves only at important moments (paradigm switches, tool calls)
#[derive(Debug, Clone)]
pub enum AutoSavePolicy {
    /// Save a checkpoint after every iteration.
    /// Most safe (can recover from any point), most storage-intensive.
    EveryStep,

    /// Save a checkpoint every N iterations.
    /// Balances safety and storage.
    EveryNSteps(usize),

    /// Save only at critical nodes:
    /// - Paradigm switches (Plan → ReAct → Reflect)
    /// - Tool call completions
    /// - Sub-agent completions
    /// Least storage, coarser recovery granularity.
    CriticalNodes,
}

impl Default for AutoSavePolicy {
    fn default() -> Self {
        Self::EveryNSteps(5) // Balanced default
    }
}

// ─── CheckpointBackend trait ────────────────────────────────────────────────

/// Checkpoint storage backend — the interface for different persistence strategies.
///
/// Provides three implementations:
/// - MemoryCheckpointBackend: in-memory (for development/testing)
/// - SqliteCheckpointBackend: SQLite (for single-device production)
/// - PostgresCheckpointBackend: PostgreSQL (for server-side production)
#[async_trait]
pub trait CheckpointBackend: Send + Sync {
    /// Save a checkpoint with the given ID and state.
    async fn save(&self, id: &str, state: &AgentState) -> Result<()>;

    /// Load a checkpoint by ID.
    async fn load(&self, id: &str) -> Result<AgentState>;

    /// List all available checkpoints.
    async fn list(&self) -> Result<Vec<oneai_core::CheckpointInfo>>;

    /// Delete a checkpoint by ID.
    async fn delete(&self, id: &str) -> Result<()>;

    /// Get the backend type name.
    fn backend_type(&self) -> &'static str;
}

// ─── MemoryCheckpointBackend ────────────────────────────────────────────────

/// In-memory checkpoint backend (for development/testing).
///
/// Stores checkpoints in a HashMap in memory. Fast but not persistent
/// across application restarts.
pub struct MemoryCheckpointBackend {
    checkpoints: tokio::sync::RwLock<std::collections::HashMap<String, AgentState>>,
}

impl MemoryCheckpointBackend {
    pub fn new() -> Self {
        Self {
            checkpoints: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for MemoryCheckpointBackend {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl CheckpointBackend for MemoryCheckpointBackend {
    async fn save(&self, id: &str, state: &AgentState) -> Result<()> {
        self.checkpoints.write().await.insert(id.to_string(), state.clone());
        Ok(())
    }

    async fn load(&self, id: &str) -> Result<AgentState> {
        self.checkpoints.read().await
            .get(id)
            .cloned()
            .ok_or_else(|| oneai_core::error::OneAIError::Persistence(
                format!("Checkpoint '{}' not found", id)
            ))
    }

    async fn list(&self) -> Result<Vec<oneai_core::CheckpointInfo>> {
        // Implementation: convert stored AgentStates to CheckpointInfo
        todo!("Implementation in full code phase")
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.checkpoints.write().await.remove(id);
        Ok(())
    }

    fn backend_type(&self) -> &'static str { "memory" }
}

// ─── SqliteCheckpointBackend ────────────────────────────────────────────────

/// SQLite checkpoint backend (for single-device production use).
///
/// Stores checkpoints in a SQLite database. Persistent across app restarts.
/// Suitable for mobile and desktop deployments.
pub struct SqliteCheckpointBackend {
    db_path: std::path::PathBuf,
}

impl SqliteCheckpointBackend {
    pub fn new(db_path: impl Into<std::path::PathBuf>) -> Self {
        Self { db_path: db_path.into() }
    }
}

// Note: Full implementation requires rusqlite dependency and
// schema creation (CREATE TABLE checkpoints ...).
// This will be implemented in the full code phase.

// ─── ProgressiveCheckpointManager ───────────────────────────────────────────

/// Progressive checkpoint manager — orchestrates auto-save per iteration.
///
/// Integrated into the AgentLoop: after each iteration, `auto_checkpoint()`
/// is called to save the current state according to the configured policy.
///
/// Features:
/// - Auto-save per iteration (configurable policy)
/// - Interrupt support: pause execution at any node
/// - Replay from any checkpoint
/// - Fork from any checkpoint (branch into a new execution path)
pub struct ProgressiveCheckpointManager {
    /// The storage backend.
    backend: Arc<dyn CheckpointBackend>,

    /// The auto-save policy.
    auto_save: AutoSavePolicy,

    /// The last checkpoint ID (for efficient incremental saves).
    last_checkpoint_id: Option<String>,
}

impl ProgressiveCheckpointManager {
    /// Create a new progressive checkpoint manager.
    pub fn new(backend: Arc<dyn CheckpointBackend>, auto_save: AutoSavePolicy) -> Self {
        Self {
            backend,
            auto_save,
            last_checkpoint_id: None,
        }
    }

    /// Create with default settings (MemoryBackend, EveryNSteps(5)).
    pub fn with_defaults() -> Self {
        Self::new(
            Arc::new(MemoryCheckpointBackend::new()),
            AutoSavePolicy::default(),
        )
    }

    /// Auto-save a checkpoint based on the current policy.
    ///
    /// Called at the end of each AgentLoop iteration.
    /// The policy determines whether this iteration triggers a save:
    /// - EveryStep: always save
    /// - EveryNSteps(n): save if iteration % n == 0
    /// - CriticalNodes: save only at paradigm switches, tool calls, etc.
    pub async fn auto_checkpoint(
        &mut self,
        state: &AgentState,
        iteration: usize,
        is_critical_node: bool,
    ) -> Result<String> {
        let should_save = match &self.auto_save {
            AutoSavePolicy::EveryStep => true,
            AutoSavePolicy::EveryNSteps(n) => iteration % n == 0,
            AutoSavePolicy::CriticalNodes => is_critical_node,
        };

        if should_save {
            let checkpoint_id = format!("{}_iter_{}", state.session_id, iteration);
            self.backend.save(&checkpoint_id, state).await?;
            self.last_checkpoint_id = Some(checkpoint_id.clone());
            Ok(checkpoint_id)
        } else {
            Ok(self.last_checkpoint_id.clone().unwrap_or_default())
        }
    }

    /// Load a checkpoint for recovery/replay.
    pub async fn load_checkpoint(&self, id: &str) -> Result<AgentState> {
        self.backend.load(id).await
    }

    /// List all available checkpoints.
    pub async fn list_checkpoints(&self) -> Result<Vec<oneai_core::CheckpointInfo>> {
        self.backend.list().await
    }

    /// Delete a checkpoint.
    pub async fn delete_checkpoint(&self, id: &str) -> Result<()> {
        self.backend.delete(id).await
    }
}