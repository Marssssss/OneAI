//! StateGraph checkpoint persistence + resume (gap-analysis P2 #14).
//!
//! Before this module, a `StateGraphExecutor` walk was fire-and-forget: a
//! crash, interrupt, or process restart mid-walk lost all progress (the
//! "durable execution" gap). Now the walk state — frontier, iteration
//! counter, accumulated interrupt checkpoints, and the full serializable
//! [`GraphState`] — can be persisted at every iteration boundary via a
//! [`GraphCheckpointStore`], and a later process can continue exactly where
//! the walk stopped via [`crate::StateGraphExecutor::resume`].
//!
//! Stores: [`InMemoryCheckpointStore`] (tests / single-process) and
//! [`FileCheckpointStore`] (one JSON file per run — survives restarts).
//! The store trait is sync on purpose: checkpoints are written once per
//! iteration boundary, never on a hot per-token path.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::state_graph::GraphState;
use oneai_core::error::{OneAIError, Result};

/// A durable snapshot of a StateGraph walk at an iteration boundary.
///
/// Fully serializable (GraphState is), so a checkpoint written by one
/// process can be resumed by another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphCheckpoint {
    /// Caller-provided identity of this graph run (e.g. session/task id).
    pub run_id: String,
    /// The graph being walked — `resume` validates it matches the graph it
    /// is handed (a checkpoint is meaningless against a different graph).
    pub graph_name: String,
    /// The frontier of ready node ids at the checkpoint moment.
    pub frontier: BTreeSet<String>,
    /// Iterations already consumed (resume continues from here, so the
    /// `max_iterations` bound still holds across restarts).
    pub iterations: usize,
    /// The full walk state (conversation, variables, decisions, …).
    pub state: GraphState,
    /// Interrupt checkpoints accumulated so far.
    pub interrupt_checkpoints: Vec<String>,
    /// Whether the run had completed when saved (completed checkpoints are
    /// normally deleted by the executor; this flag guards stale files).
    pub completed: bool,
    /// RFC 3339 UTC save timestamp.
    pub saved_at: String,
}

/// Durable storage for [`GraphCheckpoint`]s, keyed by run id.
///
/// Implementations must be infallible-best-effort about concurrency (the
/// executor calls `save` sequentially at iteration boundaries) and treat a
/// missing run id as `Ok(None)` on load.
pub trait GraphCheckpointStore: Send + Sync {
    /// Persist (overwrite) the checkpoint for its `run_id`.
    fn save(&self, checkpoint: &GraphCheckpoint) -> Result<()>;
    /// Load the checkpoint for `run_id`, or `None` when absent.
    fn load(&self, run_id: &str) -> Result<Option<GraphCheckpoint>>;
    /// Remove the checkpoint for `run_id` (idempotent — absent = Ok).
    fn delete(&self, run_id: &str) -> Result<()>;
}

// ─── InMemoryCheckpointStore ────────────────────────────────────────────────

/// Process-local checkpoint store — tests and single-process resume.
#[derive(Debug, Default)]
pub struct InMemoryCheckpointStore {
    checkpoints: Mutex<HashMap<String, GraphCheckpoint>>,
}

impl InMemoryCheckpointStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored checkpoints (test helper).
    pub fn len(&self) -> usize {
        self.checkpoints.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Whether the store is empty (test helper).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl GraphCheckpointStore for InMemoryCheckpointStore {
    fn save(&self, checkpoint: &GraphCheckpoint) -> Result<()> {
        self.checkpoints
            .lock()
            .map(|mut m| {
                m.insert(checkpoint.run_id.clone(), checkpoint.clone());
            })
            .map_err(|_| OneAIError::Workflow("checkpoint store lock poisoned".to_string()))
    }

    fn load(&self, run_id: &str) -> Result<Option<GraphCheckpoint>> {
        self.checkpoints
            .lock()
            .map(|m| m.get(run_id).cloned())
            .map_err(|_| OneAIError::Workflow("checkpoint store lock poisoned".to_string()))
    }

    fn delete(&self, run_id: &str) -> Result<()> {
        self.checkpoints
            .lock()
            .map(|mut m| {
                m.remove(run_id);
            })
            .map_err(|_| OneAIError::Workflow("checkpoint store lock poisoned".to_string()))
    }
}

// ─── FileCheckpointStore ────────────────────────────────────────────────────

/// File-backed checkpoint store — one `<run-id>.checkpoint.json` per run in
/// a directory. Survives process restarts (durable execution).
pub struct FileCheckpointStore {
    dir: PathBuf,
}

impl FileCheckpointStore {
    /// Create the store directory (if needed).
    pub fn new(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The directory this store writes into.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, run_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.checkpoint.json", sanitize_run_id(run_id)))
    }
}

impl std::fmt::Debug for FileCheckpointStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileCheckpointStore")
            .field("dir", &self.dir)
            .finish()
    }
}

/// Keep run ids filesystem-safe: anything not alphanumeric/`-`/`_`/`.`
/// becomes `_` (prevents path traversal via crafted run ids).
fn sanitize_run_id(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl GraphCheckpointStore for FileCheckpointStore {
    fn save(&self, checkpoint: &GraphCheckpoint) -> Result<()> {
        let json = serde_json::to_string_pretty(checkpoint)
            .map_err(|e| OneAIError::Workflow(format!("checkpoint serialize failed: {}", e)))?;
        std::fs::write(self.path_for(&checkpoint.run_id), json)
            .map_err(|e| OneAIError::Workflow(format!("checkpoint write failed: {}", e)))
    }

    fn load(&self, run_id: &str) -> Result<Option<GraphCheckpoint>> {
        let path = self.path_for(run_id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| OneAIError::Workflow(format!("checkpoint read failed: {}", e)))?;
        let checkpoint: GraphCheckpoint = serde_json::from_str(&content).map_err(|e| {
            OneAIError::Workflow(format!("checkpoint parse failed for {}: {}", run_id, e))
        })?;
        Ok(Some(checkpoint))
    }

    fn delete(&self, run_id: &str) -> Result<()> {
        match std::fs::remove_file(self.path_for(run_id)) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(OneAIError::Workflow(format!(
                "checkpoint delete failed: {}",
                e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_checkpoint(run_id: &str) -> GraphCheckpoint {
        GraphCheckpoint {
            run_id: run_id.to_string(),
            graph_name: "test-graph".to_string(),
            frontier: BTreeSet::from(["node-a".to_string()]),
            iterations: 3,
            state: GraphState::new(),
            interrupt_checkpoints: vec!["interrupt_test-graph_node-x".to_string()],
            completed: false,
            saved_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn in_memory_store_roundtrip_and_delete() {
        let store = InMemoryCheckpointStore::new();
        assert!(store.load("run-1").unwrap().is_none());

        store.save(&sample_checkpoint("run-1")).unwrap();
        assert_eq!(store.len(), 1);
        let loaded = store.load("run-1").unwrap().expect("present");
        assert_eq!(loaded.graph_name, "test-graph");
        assert_eq!(loaded.iterations, 3);
        assert_eq!(loaded.frontier.len(), 1);

        store.delete("run-1").unwrap();
        assert!(store.is_empty());
        assert!(store.load("run-1").unwrap().is_none());
        // Deleting again is idempotent.
        store.delete("run-1").unwrap();
    }

    #[test]
    fn file_store_roundtrip_across_instances() {
        let dir =
            std::env::temp_dir().join(format!("oneai-checkpoint-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let store = FileCheckpointStore::new(&dir).unwrap();
            store.save(&sample_checkpoint("run/with:weird id")).unwrap();
        }
        // A fresh store instance (i.e. a fresh process) sees the file.
        {
            let store = FileCheckpointStore::new(&dir).unwrap();
            let loaded = store
                .load("run/with:weird id")
                .unwrap()
                .expect("checkpoint survives restart");
            assert_eq!(loaded.graph_name, "test-graph");
            assert_eq!(loaded.interrupt_checkpoints.len(), 1);
            store.delete("run/with:weird id").unwrap();
            assert!(store.load("run/with:weird id").unwrap().is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_run_id_blocks_traversal() {
        assert_eq!(sanitize_run_id("../../etc"), ".._.._etc");
        assert_eq!(sanitize_run_id("ok-run_id.1"), "ok-run_id.1");
        assert!(!sanitize_run_id("a/b\\c").contains('/'));
        assert!(!sanitize_run_id("a/b\\c").contains('\\'));
    }
}
