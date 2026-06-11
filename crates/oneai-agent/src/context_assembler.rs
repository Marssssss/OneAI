//! Context assembler — constructs the conversation context for each loop iteration.
//!
//! The context assembler is responsible for:
//! 1. Building the conversation from all available sources (system prompt,
//!    recent turns, tool results, skills, retrieved context)
//! 2. Detecting environment changes and injecting diffs
//! 3. Ensuring the assembled context fits within the token budget
//!
//! This addresses Issue #10: tool outputs don't always reflect environment
//! changes. The assembler detects changes (new files, git modifications,
//! directory structure changes) and injects them as context even when
//! no tool explicitly reported them.

use std::collections::HashSet;
use std::path::PathBuf;

use oneai_core::Conversation;
use oneai_core::error::Result;

// ─── EnvironmentSnapshot ────────────────────────────────────────────────────

/// A snapshot of the current environment state.
///
/// Taken at the start of each loop iteration and compared with
/// the previous snapshot. If changes are detected, they are
/// injected into the context as additional messages.
///
/// Environment changes include:
/// - Modified/deleted/created files (via filesystem scan)
/// - Git status changes (if in a git repository)
/// - Available tools changes (if new tools were registered)
/// - Working directory changes
#[derive(Debug, Clone)]
pub struct EnvironmentSnapshot {
    /// Current working directory.
    pub working_dir: PathBuf,

    /// Current platform.
    pub platform: oneai_core::platform::Platform,

    /// Set of available tool names.
    pub available_tools: HashSet<String>,

    /// Git status (if in a git repo): short summary of changes.
    /// Format: "2 modified, 1 added, 0 deleted"
    pub git_status: Option<String>,

    /// List of modified files (since last snapshot).
    pub modified_files: Vec<String>,

    /// List of newly created files (since last snapshot).
    pub created_files: Vec<String>,

    /// List of deleted files (since last snapshot).
    pub deleted_files: Vec<String>,
}

impl EnvironmentSnapshot {
    /// Create an empty snapshot.
    pub fn empty() -> Self {
        Self {
            working_dir: PathBuf::new(),
            platform: oneai_core::platform::Platform::Unknown,
            available_tools: HashSet::new(),
            git_status: None,
            modified_files: Vec::new(),
            created_files: Vec::new(),
            deleted_files: Vec::new(),
        }
    }
}

// ─── EnvironmentDiff ────────────────────────────────────────────────────────

/// The diff between two environment snapshots.
///
/// If the diff is empty, no environment changes are injected into the context.
/// This prevents redundant context injection when nothing has changed.
#[derive(Debug, Clone)]
pub struct EnvironmentDiff {
    /// Files that were modified.
    pub modified_files: Vec<String>,

    /// Files that were created.
    pub created_files: Vec<String>,

    /// Files that were deleted.
    pub deleted_files: Vec<String>,

    /// Tools that were added.
    pub added_tools: Vec<String>,

    /// Tools that were removed.
    pub removed_tools: Vec<String>,

    /// Whether the working directory changed.
    pub working_dir_changed: bool,

    /// Whether the git status changed.
    pub git_status_changed: bool,

    /// The new git status summary (if changed).
    pub new_git_status: Option<String>,
}

impl EnvironmentDiff {
    /// Check if there are any changes.
    pub fn has_changes(&self) -> bool {
        !self.modified_files.is_empty()
            || !self.created_files.is_empty()
            || !self.deleted_files.is_empty()
            || !self.added_tools.is_empty()
            || !self.removed_tools.is_empty()
            || self.working_dir_changed
            || self.git_status_changed
    }

    /// Format the diff as a human-readable string for context injection.
    pub fn to_context_string(&self) -> String {
        if !self.has_changes() {
            return String::new();
        }

        let mut parts = Vec::new();

        if !self.modified_files.is_empty() {
            parts.push(format!("Modified files: {}", self.modified_files.join(", ")));
        }
        if !self.created_files.is_empty() {
            parts.push(format!("New files: {}", self.created_files.join(", ")));
        }
        if !self.deleted_files.is_empty() {
            parts.push(format!("Deleted files: {}", self.deleted_files.join(", ")));
        }
        if !self.added_tools.is_empty() {
            parts.push(format!("New tools available: {}", self.added_tools.join(", ")));
        }
        if !self.removed_tools.is_empty() {
            parts.push(format!("Tools removed: {}", self.removed_tools.join(", ")));
        }
        if self.working_dir_changed {
            parts.push("Working directory changed".to_string());
        }
        if let Some(git) = &self.new_git_status {
            parts.push(format!("Git status: {}", git));
        }

        format!("[Environment changes]: {}", parts.join("; "))
    }
}

// ─── ContextAssembler ───────────────────────────────────────────────────────

/// Context assembler — constructs conversation context per loop iteration.
///
/// The assembler:
/// 1. Takes the current conversation from LoopState
/// 2. Detects environment changes by comparing snapshots
/// 3. Injects relevant changes as context messages
/// 4. Returns the assembled conversation for inference
///
/// This ensures the model always has up-to-date environment information,
/// even when tool outputs don't directly reflect the changes.
pub struct ContextAssembler {
    /// The previous environment snapshot (for diffing).
    last_snapshot: Option<EnvironmentSnapshot>,
}

impl ContextAssembler {
    /// Create a new context assembler.
    pub fn new() -> Self {
        Self { last_snapshot: None }
    }

    /// Assemble the context for a loop iteration.
    ///
    /// 1. Takes the current snapshot of the environment
    /// 2. Computes the diff from the last snapshot
    /// 3. If changes detected, injects them into the conversation
    /// 4. Updates the last snapshot
    pub fn assemble(&self, state: &crate::agent_loop::LoopState) -> Result<Conversation> {
        // Implementation:
        // 1. Take current environment snapshot
        // 2. Compute diff with last_snapshot
        // 3. If diff.has_changes(), inject EnvironmentDiff message
        // 4. Return assembled conversation
        todo!("Implementation in full code phase")
    }

    /// Take a snapshot of the current environment.
    ///
    /// This scans:
    /// - Working directory (via std::env::current_dir)
    /// - Available tools (from tool registry)
    /// - Git status (if in a git repo)
    /// - File modifications (compare with known state)
    async fn take_snapshot(&self) -> Result<EnvironmentSnapshot> {
        // Implementation: scan filesystem, check git status, etc.
        todo!("Implementation in full code phase")
    }

    /// Compute the diff between two snapshots.
    fn compute_diff(&self, old: &EnvironmentSnapshot, new: &EnvironmentSnapshot) -> EnvironmentDiff {
        EnvironmentDiff {
            modified_files: new.modified_files.clone(),
            created_files: new.created_files.clone(),
            deleted_files: new.deleted_files.clone(),
            added_tools: new.available_tools.difference(&old.available_tools).cloned().collect(),
            removed_tools: old.available_tools.difference(&new.available_tools).cloned().collect(),
            working_dir_changed: old.working_dir != new.working_dir,
            git_status_changed: old.git_status != new.git_status,
            new_git_status: if old.git_status != new.git_status {
                new.git_status.clone()
            } else {
                None
            },
        }
    }
}

impl Default for ContextAssembler {
    fn default() -> Self {
        Self::new()
    }
}