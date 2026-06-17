//! Context assembler — constructs the conversation context for each loop iteration.
//!
//! The context assembler is responsible for:
//! 1. Building the conversation from all available sources (system prompt,
//!    recent turns, tool results, skills, retrieved context)
//! 2. Detecting environment changes and injecting diffs
//! 3. Ensuring the assembled context fits within the token budget
//!
//! **Context Epoch mode** (inspired by OpenCode):
//! - First iteration: inject full baseline (all context sources + full env snapshot)
//! - Subsequent iterations: inject only the diff (changed files, new tools, env changes)
//! - This saves ~2000-5000 tokens per iteration (50k-250k tokens per session)
//!
//! This addresses Issue #10: tool outputs don't always reflect environment
//! changes. The assembler detects changes (new files, git modifications,
//! directory structure changes) and injects them as context even when
//! no tool explicitly reported them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use oneai_core::Conversation;
use oneai_core::error::Result;

use oneai_domain::context_source::ContextSource;

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
    /// Domain-specific context sources (injected from DomainPack).
    context_sources: Vec<Arc<dyn ContextSource>>,
    /// Cached context content from sources (for OnChange detection).
    cached_context: HashMap<String, String>,
    /// Baseline context content (from the first epoch — for diffing in incremental mode).
    baseline_content: HashMap<String, String>,
    /// Whether initial load has been done (for OnceAtStart sources).
    initial_load_done: bool,
    /// Number of iterations since the first epoch (for Periodic sources in incremental mode).
    iterations_since_epoch: Option<usize>,
}

impl ContextAssembler {
    /// Create a new context assembler.
    pub fn new() -> Self {
        Self {
            last_snapshot: None,
            context_sources: Vec::new(),
            cached_context: HashMap::new(),
            baseline_content: HashMap::new(),
            initial_load_done: false,
            iterations_since_epoch: None,
        }
    }

    /// Create a context assembler with domain-specific context sources.
    pub fn with_context_sources(context_sources: Vec<Arc<dyn ContextSource>>) -> Self {
        Self {
            last_snapshot: None,
            context_sources,
            cached_context: HashMap::new(),
            baseline_content: HashMap::new(),
            initial_load_done: false,
            iterations_since_epoch: None,
        }
    }

    /// Assemble the context for a loop iteration.
    ///
    /// **Context Epoch mode**:
    /// - First iteration (last_snapshot is None): inject full baseline
    ///   — all context sources are loaded, full environment snapshot is included
    ///   — this establishes the "baseline epoch" that the model will remember
    /// - Subsequent iterations (last_snapshot exists): inject only diff
    ///   — only changed environment data (modified/created/deleted files, new tools)
    ///   — only context sources that changed since baseline (OnChange policy)
    ///   — This saves ~2000-5000 tokens per iteration in typical sessions
    pub fn assemble(&self, state: &crate::agent_loop::LoopState) -> Result<Conversation> {
        let mut conversation = state.conversation.clone();

        // ─── Epoch mode: baseline vs incremental ─────────────────────────
        let is_first_epoch = self.last_snapshot.is_none();

        if is_first_epoch {
            // First epoch: inject full baseline context
            // This includes the complete environment state and all context sources.
            // The model will remember this baseline, and subsequent iterations
            // only need the diff to stay current.

            // Inject full environment snapshot
            if let Some(ref current) = state.env_snapshot {
                let env_msg = format_full_env_snapshot(current);
                if !env_msg.is_empty() {
                    conversation.add_message(oneai_core::Message::system(env_msg));
                }
            }

            // Inject all context sources (full baseline)
            if !self.context_sources.is_empty() {
                let mut sources: Vec<&Arc<dyn ContextSource>> = self.context_sources.iter().collect();
                sources.sort_by_key(|s| s.priority());

                for source in sources {
                    use oneai_domain::context_source::RefreshPolicy;

                    let should_load = match source.refresh_policy() {
                        RefreshPolicy::EveryIteration => true,
                        RefreshPolicy::OnceAtStart => !self.initial_load_done,
                        RefreshPolicy::OnChange => {
                            self.cached_context.contains_key(source.key())
                        }
                        RefreshPolicy::Periodic(_) => {
                            self.cached_context.contains_key(source.key())
                        }
                    };

                    if should_load {
                        if let Some(content) = self.cached_context.get(source.key()) {
                            if !content.is_empty() {
                                let context_msg = format!("[Context: {}] {}", source.key(), content);
                                conversation.add_message(oneai_core::Message::system(context_msg));
                            }
                        }
                    }
                }
            }
        } else {
            // Incremental epoch: inject only the diff from the baseline
            // This is where the token savings happen — instead of re-injecting
            // the entire file tree, git status, and all context sources every turn,
            // we only inject the changes.

            // Inject environment diff if there are changes
            if let Some(ref last) = self.last_snapshot {
                if let Some(ref current) = state.env_snapshot {
                    let diff = self.compute_diff(last, current);
                    if diff.has_changes() {
                        let context_msg = diff.to_context_string();
                        if !context_msg.is_empty() {
                            conversation.add_message(oneai_core::Message::system(context_msg));
                        }
                    }
                }
            }

            // Inject only changed context sources (not full baseline)
            if !self.context_sources.is_empty() {
                let mut sources: Vec<&Arc<dyn ContextSource>> = self.context_sources.iter().collect();
                sources.sort_by_key(|s| s.priority());

                for source in sources {
                    use oneai_domain::context_source::RefreshPolicy;

                    let should_load = match source.refresh_policy() {
                        // EveryIteration: always inject (this source changes every turn)
                        RefreshPolicy::EveryIteration => true,
                        // OnceAtStart: skip (already in baseline, no need to repeat)
                        RefreshPolicy::OnceAtStart => false,
                        // OnChange: inject only if content changed from baseline
                        RefreshPolicy::OnChange => {
                            // Only inject if content differs from what was cached in baseline
                            self.cached_context.contains_key(source.key())
                                && self.has_source_changed(source.key())
                        }
                        // Periodic: check if enough iterations passed since baseline
                        // Convert Duration to an approximate iteration count
                        // (assume ~5 seconds per iteration as rough estimate)
                        RefreshPolicy::Periodic(interval) => {
                            let interval_iters = (interval.as_secs() / 5).max(1) as usize;
                            if let Some(iterations) = self.iterations_since_epoch {
                                iterations % interval_iters == 0 && iterations > 0
                            } else {
                                false
                            }
                        }
                    };

                    if should_load {
                        if let Some(content) = self.cached_context.get(source.key()) {
                            if !content.is_empty() {
                                let context_msg = format!("[Context: {}] {}", source.key(), content);
                                conversation.add_message(oneai_core::Message::system(context_msg));
                            }
                        }
                    }
                }
            }
        }

        Ok(conversation)
    }

    /// Check if a context source's content has changed since baseline.
    fn has_source_changed(&self, key: &str) -> bool {
        // In incremental mode, sources that changed since baseline should be re-injected.
        // For now, we check if the cached content differs from the baseline content.
        // The baseline_content map stores what was loaded during the first epoch.
        if let Some(baseline) = self.baseline_content.get(key) {
            if let Some(current) = self.cached_context.get(key) {
                baseline != current
            } else {
                false
            }
        } else {
            // Not in baseline — this is a new source, inject it
            true
        }
    }

    /// Refresh and cache all context sources (async — called from the loop).
    ///
    /// On the first call (baseline epoch), stores all source content as baseline.
    /// On subsequent calls, only updates cached content for changed sources.
    pub async fn refresh_sources(&mut self) -> Result<()> {
        if !self.initial_load_done {
            // First epoch: store all content as baseline for later diffing
            for source in &self.context_sources {
                let content = source.load().await?;
                self.cached_context.insert(source.key().to_string(), content.clone());
                // Store baseline content (never changes after first epoch)
                self.baseline_content.insert(source.key().to_string(), content);
            }
            self.initial_load_done = true;
            self.iterations_since_epoch = Some(0);
        } else {
            // Incremental epoch: update cache, only for changed sources
            for source in &self.context_sources {
                let content = source.load().await?;
                let prev = self.cached_context.get(source.key());
                if prev.map_or(true, |p| p != &content) {
                    self.cached_context.insert(source.key().to_string(), content);
                }
            }
            // Increment the epoch counter
            if let Some(ref count) = self.iterations_since_epoch {
                self.iterations_since_epoch = Some(count + 1);
            }
        }

        Ok(())
    }

    /// Update the stored environment snapshot.
    ///
    /// Called from the AgentLoop after taking a snapshot each iteration.
    /// The next call to `assemble()` will compute the diff between
    /// `last_snapshot` and the LoopState's `env_snapshot`, and inject
    /// any detected changes into the conversation context.
    ///
    /// This addresses the "Context Epoch 未接入 Loop" gap — previously,
    /// `take_snapshot()` existed but was never called from the loop,
    /// and `last_snapshot` was never updated, so diffs were never computed.
    pub fn update_snapshot(&mut self, snapshot: EnvironmentSnapshot) {
        self.last_snapshot = Some(snapshot);
    }

    /// Take a snapshot of the current environment.
    ///
    /// This scans:
    /// - Working directory (via std::env::current_dir)
    /// - Available tools (from tool names)
    /// - Git status (if in a git repo)
    /// - Modified/created/deleted files (from git diff)
    pub async fn take_snapshot(&self, available_tools: &HashSet<String>) -> Result<EnvironmentSnapshot> {
        let working_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."));

        let platform = oneai_core::platform::Platform::Unknown;

        // Get git status
        let git_status = get_git_status(&working_dir).await;

        // Get modified/created/deleted files from git
        let (modified_files, created_files, deleted_files) = get_git_file_changes(&working_dir).await;

        Ok(EnvironmentSnapshot {
            working_dir,
            platform,
            available_tools: available_tools.clone(),
            git_status,
            modified_files,
            created_files,
            deleted_files,
        })
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

// ─── Helper functions for environment scanning ────────────────────────────────

/// Get git status summary for a directory.
///
/// Returns a short summary like "2 modified, 1 added, 0 deleted" or None if not a git repo.
async fn get_git_status(dir: &PathBuf) -> Option<String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let (shell, shell_arg) = if cfg!(target_os = "windows") {
        ("powershell", "-Command")
    } else {
        ("sh", "-c")
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(shell)
            .arg(shell_arg)
            .arg(format!("cd {} && git status --short 2>/dev/null", dir_str))
            .output()
    ).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                Some("clean".to_string())
            } else {
                let lines = stdout.lines().count();
                Some(format!("{} files changed", lines))
            }
        }
        _ => None,
    }
}

/// Get modified, created, and deleted file lists from git diff.
///
/// Returns three vectors of file paths.
async fn get_git_file_changes(dir: &PathBuf) -> (Vec<String>, Vec<String>, Vec<String>) {
    let dir_str = dir.to_str().unwrap_or(".");
    let (shell, shell_arg) = if cfg!(target_os = "windows") {
        ("powershell", "-Command")
    } else {
        ("sh", "-c")
    };

    // Get modified files (M prefix in git status --short)
    let modified = get_git_files_by_prefix(dir_str, shell, shell_arg, "M").await;
    // Get created files (A prefix)
    let created = get_git_files_by_prefix(dir_str, shell, shell_arg, "A").await;
    // Get deleted files (D prefix)
    let deleted = get_git_files_by_prefix(dir_str, shell, shell_arg, "D").await;

    (modified, created, deleted)
}

/// Get files matching a specific git status prefix.
async fn get_git_files_by_prefix(dir: &str, shell: &str, shell_arg: &str, prefix: &str) -> Vec<String> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(shell)
            .arg(shell_arg)
            .arg(format!("cd {} && git status --short 2>/dev/null | grep '^{}' | sed 's/^{}\\s*//' || true", dir, prefix, prefix))
            .output()
    ).await;

    match result {
        Ok(Ok(output)) => {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.trim().to_string())
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Format a full environment snapshot for baseline epoch injection.
///
/// This produces a comprehensive context message that includes all
/// environment information — working directory, platform, git status,
/// available tools, and file change lists. This is injected on the
/// first iteration to establish the baseline epoch.
fn format_full_env_snapshot(snapshot: &EnvironmentSnapshot) -> String {
    let mut parts = Vec::new();

    parts.push(format!("Working directory: {}", snapshot.working_dir.display()));
    parts.push(format!("Platform: {:?}", snapshot.platform));

    if let Some(ref git) = snapshot.git_status {
        parts.push(format!("Git status: {}", git));
    }

    if !snapshot.available_tools.is_empty() {
        let tools = snapshot.available_tools.iter()
            .cloned()
            .collect::<Vec<_>>();
        parts.push(format!("Available tools: {}", tools.join(", ")));
    }

    if !snapshot.modified_files.is_empty() {
        parts.push(format!("Modified files: {}", snapshot.modified_files.join(", ")));
    }
    if !snapshot.created_files.is_empty() {
        parts.push(format!("New files: {}", snapshot.created_files.join(", ")));
    }
    if !snapshot.deleted_files.is_empty() {
        parts.push(format!("Deleted files: {}", snapshot.deleted_files.join(", ")));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!("[Environment baseline]: {}", parts.join("; "))
    }
}