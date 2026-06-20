//! State checkpointing — save/load agent state for interruption recovery.
//!
//! FilePersistence saves agent state checkpoints as JSON files on disk.
//! This enables:
//! - Recovery after interruptions (app crash, network loss)
//! - Resuming long-running workflows from the last completed step
//! - Debugging by inspecting saved state
//!
//! Platform-specific implementations may use SQLite or key-value stores
//! instead of JSON files (Phase 6).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use oneai_core::{AgentState, CheckpointInfo};
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::StatePersistence;

/// File-based state persistence implementation.
///
/// Saves agent state checkpoints to JSON files on disk.
/// Each checkpoint is stored as a separate file named by its ID.
pub struct FilePersistence {
    /// The base directory path for checkpoint files.
    base_path: PathBuf,
}

impl FilePersistence {
    /// Create a new file-based persistence with the given directory path.
    ///
    /// The directory will be created if it doesn't exist when saving.
    pub fn new(base_path: impl Into<String>) -> Self {
        Self {
            base_path: PathBuf::from(base_path.into()),
        }
    }

    /// Create with a PathBuf directly.
    pub fn from_path(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    /// Get the checkpoint file path for a given ID.
    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.base_path.join(format!("{}.json", id))
    }

    /// Get the base path.
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// Ensure the base directory exists.
    async fn ensure_directory(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.base_path).await.map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to create checkpoint directory '{}': {}",
                self.base_path.display(), e
            ))
        })
    }
}

#[async_trait]
impl StatePersistence for FilePersistence {
    /// Save a checkpoint of the current agent state.
    ///
    /// Creates a JSON file in the base directory named by the checkpoint ID.
    async fn save_checkpoint(&self, state: &AgentState) -> Result<String> {
        self.ensure_directory().await?;

        // Generate checkpoint ID if not provided
        let checkpoint_id = if state.session_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            format!("{}_{}", state.session_id, state.timestamp.timestamp())
        };

        let path = self.checkpoint_path(&checkpoint_id);

        let json = serde_json::to_string_pretty(state).map_err(|e| {
            OneAIError::Serialization(format!("Failed to serialize agent state: {}", e))
        })?;

        tokio::fs::write(&path, json).await.map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to write checkpoint file '{}': {}",
                path.display(), e
            ))
        })?;

        tracing::info!("Checkpoint saved: {}", checkpoint_id);
        Ok(checkpoint_id)
    }

    /// Load a checkpoint by ID.
    ///
    /// Reads the JSON file and deserializes it into an AgentState.
    async fn load_checkpoint(&self, id: &str) -> Result<AgentState> {
        let path = self.checkpoint_path(id);

        let json = tokio::fs::read_to_string(&path).await.map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to read checkpoint file '{}': {}",
                path.display(), e
            ))
        })?;

        let state: AgentState = serde_json::from_str(&json).map_err(|e| {
            OneAIError::Serialization(format!("Failed to deserialize agent state: {}", e))
        })?;

        tracing::info!("Checkpoint loaded: {}", id);
        Ok(state)
    }

    /// List all available checkpoints.
    ///
    /// Scans the base directory for .json files and returns metadata
    /// about each checkpoint.
    async fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>> {
        self.ensure_directory().await?;

        let mut entries = tokio::fs::read_dir(&self.base_path).await.map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to read checkpoint directory '{}': {}",
                self.base_path.display(), e
            ))
        })?;

        let mut checkpoints = Vec::new();

        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            OneAIError::Persistence(format!("Failed to read directory entry: {}", e))
        })? {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let file_name = path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");

                // Try to load the state to get metadata
                let json = tokio::fs::read_to_string(&path).await.map_err(|e| {
                    OneAIError::Persistence(format!(
                        "Failed to read checkpoint '{}': {}", path.display(), e
                    ))
                })?;

                let state: AgentState = serde_json::from_str(&json).map_err(|e| {
                    OneAIError::Serialization(format!(
                        "Failed to deserialize checkpoint '{}': {}", path.display(), e
                    ))
                })?;

                checkpoints.push(CheckpointInfo {
                    id: file_name.to_string(),
                    session_id: state.session_id.clone(),
                    timestamp: state.timestamp,
                    description: format!(
                        "Session {} - paradigm {} - step {}",
                        state.session_id,
                        state.active_paradigm,
                        state.active_step.unwrap_or_default()
                    ),
                });
            }
        }

        // Sort by timestamp descending (most recent first)
        checkpoints.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        Ok(checkpoints)
    }

    /// Delete a checkpoint by ID.
    async fn delete_checkpoint(&self, id: &str) -> Result<()> {
        let path = self.checkpoint_path(id);

        tokio::fs::remove_file(&path).await.map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to delete checkpoint file '{}': {}",
                path.display(), e
            ))
        })?;

        tracing::info!("Checkpoint deleted: {}", id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::GlobalState;
    use tempfile::TempDir;

    fn make_state(session_id: &str, paradigm: &str) -> AgentState {
        AgentState {
            session_id: session_id.to_string(),
            global_state: GlobalState::new(),
            active_paradigm: paradigm.to_string(),
            active_step: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_save_and_load_checkpoint() {
        let tmp_dir = TempDir::new().unwrap();
        let persistence = FilePersistence::new(tmp_dir.path().to_str().unwrap());

        let state = make_state("session1", "react");
        let checkpoint_id = persistence.save_checkpoint(&state).await.unwrap();

        // Load it back
        let loaded = persistence.load_checkpoint(&checkpoint_id).await.unwrap();
        assert_eq!(loaded.session_id, "session1");
        assert_eq!(loaded.active_paradigm, "react");
    }

    #[tokio::test]
    async fn test_list_checkpoints() {
        let tmp_dir = TempDir::new().unwrap();
        let persistence = FilePersistence::new(tmp_dir.path().to_str().unwrap());

        // Save two checkpoints
        let state1 = make_state("session1", "react");
        let state2 = make_state("session2", "plan");

        persistence.save_checkpoint(&state1).await.unwrap();
        persistence.save_checkpoint(&state2).await.unwrap();

        // List checkpoints
        let checkpoints = persistence.list_checkpoints().await.unwrap();
        assert_eq!(checkpoints.len(), 2);
    }

    #[tokio::test]
    async fn test_delete_checkpoint() {
        let tmp_dir = TempDir::new().unwrap();
        let persistence = FilePersistence::new(tmp_dir.path().to_str().unwrap());

        let state = make_state("session1", "react");
        let checkpoint_id = persistence.save_checkpoint(&state).await.unwrap();

        // Delete it
        persistence.delete_checkpoint(&checkpoint_id).await.unwrap();

        // Try to load — should fail
        let result = persistence.load_checkpoint(&checkpoint_id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_load_nonexistent_checkpoint() {
        let tmp_dir = TempDir::new().unwrap();
        let persistence = FilePersistence::new(tmp_dir.path().to_str().unwrap());

        let result = persistence.load_checkpoint("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_save_creates_directory() {
        let tmp_dir = TempDir::new().unwrap();
        let new_dir = tmp_dir.path().join("new_subdir");
        let persistence = FilePersistence::from_path(new_dir.clone());

        let state = make_state("session1", "react");
        let result = persistence.save_checkpoint(&state).await;
        assert!(result.is_ok());
        assert!(new_dir.exists());
    }
}