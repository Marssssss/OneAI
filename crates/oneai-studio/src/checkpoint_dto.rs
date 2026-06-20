//! Checkpoint DTO — converts checkpoint data into JSON for the Studio frontend.
//!
//! Enables checkpoint time-travel: listing, viewing, and restoring checkpoints.

use serde::{Deserialize, Serialize};
use oneai_core::CheckpointInfo;
use oneai_core::AgentState;

// ─── CheckpointListView ──────────────────────────────────────────────

/// A list of checkpoints for the time-travel timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointListView {
    /// Total number of checkpoints.
    pub total: usize,
    /// Checkpoint entries, sorted by timestamp descending.
    pub checkpoints: Vec<CheckpointEntryView>,
}

// ─── CheckpointEntryView ─────────────────────────────────────────────

/// A single checkpoint entry in the timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEntryView {
    /// Checkpoint ID.
    pub id: String,
    /// Session ID.
    pub session_id: String,
    /// Timestamp (ISO 8601).
    pub timestamp: String,
    /// Human-readable description.
    pub description: String,
    /// Active paradigm at checkpoint time.
    pub paradigm: String,
    /// Active step (string representation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

// ─── CheckpointDetailView ────────────────────────────────────────────

/// Detailed checkpoint view — includes the full agent state for restoration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointDetailView {
    /// Checkpoint metadata.
    pub entry: CheckpointEntryView,
    /// The full agent state at checkpoint time (for restoration).
    pub state: AgentStateView,
}

// ─── AgentStateView ──────────────────────────────────────────────────

/// Frontend-friendly agent state representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStateView {
    /// Session ID.
    pub session_id: String,
    /// Active paradigm.
    pub active_paradigm: String,
    /// Active step (string, e.g., "3" or "final").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_step: Option<String>,
    /// Timestamp.
    pub timestamp: String,
    /// Conversation messages count.
    pub conversation_length: usize,
}

// ─── Conversion ──────────────────────────────────────────────────────

impl CheckpointEntryView {
    /// Convert a CheckpointInfo to a frontend-friendly CheckpointEntryView.
    pub fn from_checkpoint_info(info: &CheckpointInfo) -> Self {
        // Parse paradigm and step from description
        // Format: "Session {id} - paradigm {p} - step {s}"
        let paradigm = extract_paradigm(&info.description);
        let step = extract_step(&info.description);

        Self {
            id: info.id.clone(),
            session_id: info.session_id.clone(),
            timestamp: info.timestamp.to_rfc3339(),
            description: info.description.clone(),
            paradigm,
            step,
        }
    }
}

impl CheckpointListView {
    /// Convert a list of CheckpointInfo to a CheckpointListView.
    pub fn from_checkpoint_infos(infos: &[CheckpointInfo]) -> Self {
        Self {
            total: infos.len(),
            checkpoints: infos.iter()
                .map(CheckpointEntryView::from_checkpoint_info)
                .collect(),
        }
    }
}

impl CheckpointDetailView {
    /// Convert a CheckpointInfo and AgentState to a detailed view.
    pub fn from_info_and_state(info: &CheckpointInfo, state: &AgentState) -> Self {
        Self {
            entry: CheckpointEntryView::from_checkpoint_info(info),
            state: AgentStateView {
                session_id: state.session_id.clone(),
                active_paradigm: state.active_paradigm.clone(),
                active_step: state.active_step.clone(),
                timestamp: state.timestamp.to_rfc3339(),
                conversation_length: state.global_state.conversation.messages.len(),
            },
        }
    }
}

/// Extract paradigm from description string like "Session xxx - paradigm react - step 3".
fn extract_paradigm(description: &str) -> String {
    description.split(" - ")
        .find_map(|part| {
            if part.starts_with("paradigm ") {
                Some(part.strip_prefix("paradigm ").unwrap_or("").to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Extract step number from description string.
fn extract_step(description: &str) -> Option<String> {
    description.split(" - ")
        .find_map(|part| {
            if part.starts_with("step ") {
                Some(part.strip_prefix("step ").unwrap_or("").to_string())
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_checkpoint_info(id: &str, session_id: &str, paradigm: &str, step: usize) -> CheckpointInfo {
        CheckpointInfo {
            id: id.to_string(),
            session_id: session_id.to_string(),
            timestamp: chrono::Utc::now(),
            description: format!("Session {} - paradigm {} - step {}", session_id, paradigm, step),
        }
    }

    #[test]
    fn test_checkpoint_entry_view_from_info() {
        let info = make_checkpoint_info("cp_1", "sess_1", "react", 5);
        let view = CheckpointEntryView::from_checkpoint_info(&info);

        assert_eq!(view.id, "cp_1");
        assert_eq!(view.session_id, "sess_1");
        assert_eq!(view.paradigm, "react");
        assert_eq!(view.step, Some("5".to_string()));
    }

    #[test]
    fn test_checkpoint_list_view() {
        let infos = vec![
            make_checkpoint_info("cp_1", "sess_1", "react", 3),
            make_checkpoint_info("cp_2", "sess_1", "plan", 1),
        ];

        let view = CheckpointListView::from_checkpoint_infos(&infos);
        assert_eq!(view.total, 2);
        assert_eq!(view.checkpoints.len(), 2);
    }

    #[test]
    fn test_checkpoint_list_view_json() {
        let infos = vec![
            make_checkpoint_info("cp_1", "sess_1", "react", 3),
        ];

        let view = CheckpointListView::from_checkpoint_infos(&infos);
        let json = serde_json::to_string_pretty(&view).unwrap();
        assert!(json.contains("\"cp_1\""));
        assert!(json.contains("\"react\""));
    }

    #[test]
    fn test_extract_paradigm() {
        assert_eq!(extract_paradigm("Session s1 - paradigm react - step 3"), "react");
        assert_eq!(extract_paradigm("Session s1 - paradigm plan - step 1"), "plan");
        assert_eq!(extract_paradigm("unknown format"), "unknown");
    }

    #[test]
    fn test_extract_step() {
        assert_eq!(extract_step("Session s1 - paradigm react - step 3"), Some("3".to_string()));
        assert_eq!(extract_step("Session s1 - paradigm plan - step 1"), Some("1".to_string()));
        assert_eq!(extract_step("no step info"), None);
    }
}
