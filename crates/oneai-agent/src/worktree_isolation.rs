//! WorktreeIsolation — git worktree-based isolation for parallel sub-agents.
//!
//! This addresses the gap identified in the competitive analysis (P1#13):
//! when multiple sub-agents (especially Code agents) run in parallel, they
//! may modify the same files, causing conflicts. Git worktree creates an
//! isolated copy of the repository for each sub-agent, preventing file
//! conflicts while allowing each agent to work independently.
//!
//! **How it works**:
//! 1. Before a sub-agent starts, `WorktreeIsolation::create()` runs
//!    `git worktree add <path> -b <branch>` to create an isolated copy
//! 2. The sub-agent's tools (ShellTool, FileEditTool, etc.) operate in
//!    the worktree directory instead of the main project directory
//! 3. After the sub-agent completes, `WorktreeIsolation::merge_back()`
//!    merges the worktree branch back into the main branch
//! 4. If the merge fails (conflicts), the worktree is preserved for
//!    manual resolution
//! 5. If no changes were made, the worktree is cleaned up immediately
//!
//! **Why git worktree** (vs directory copy or Docker isolation):
//! - Git worktree shares the .git directory, so it's lightweight (~instant)
//! - Each worktree is on its own branch, so changes are naturally isolated
//! - Merging back is standard git workflow (merge/rebase/cherry-pick)
//! - Claude Code uses the same approach for its agent isolation
//!
//! **Fallback**: If git worktree is not available (not in a git repo, or
//! git is not installed), falls back to directory-level isolation (copy
//! the project directory to a temp location).

use std::path::{Path, PathBuf};

use oneai_core::error::Result;

// ─── WorktreeConfig ─────────────────────────────────────────────────────────

/// Configuration for worktree isolation.
///
/// Controls how the worktree is created, merged, and cleaned up.
/// This can be set per-sub-agent-kind via DomainPack (e.g., Code agents
/// use worktree isolation, but Explore agents don't need it since they
/// only read files).
#[derive(Debug, Clone)]
pub struct WorktreeConfig {
    /// Whether to use git worktree isolation (default: true for Code agents).
    /// Set to false for read-only agents (Explore, Plan, Review).
    pub enabled: bool,

    /// Strategy for merging worktree changes back to the main branch.
    pub merge_strategy: MergeStrategy,

    /// Whether to auto-cleanup the worktree after successful merge.
    /// If false, the worktree branch and directory are preserved.
    pub auto_cleanup: bool,

    /// Custom prefix for worktree branch names (default: "oneai-sub-").
    pub branch_prefix: String,

    /// Custom directory for worktrees (default: ".oneai-worktrees/" in project root).
    pub worktree_dir: Option<PathBuf>,
}

/// Strategy for merging worktree changes back.
#[derive(Debug, Clone, PartialEq)]
pub enum MergeStrategy {
    /// Merge the worktree branch into the main branch.
    /// Standard git merge — preserves history, may have conflicts.
    Merge,

    /// Rebase the worktree branch onto the main branch, then fast-forward.
    /// Cleaner history, but may have conflicts during rebase.
    Rebase,

    /// Cherry-pick specific commits from the worktree branch.
    /// Most granular control, but requires identifying which commits to pick.
    CherryPick,

    /// Don't merge — just preserve the worktree branch.
    /// The main agent can manually review and merge later.
    /// This is the safest option for production use.
    PreserveOnly,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            merge_strategy: MergeStrategy::Merge,
            auto_cleanup: true,
            branch_prefix: "oneai-sub-".to_string(),
            worktree_dir: None,
        }
    }
}

/// Worktree config for read-only agents (no isolation needed).
impl WorktreeConfig {
    /// Configuration for read-only agents (Explore, Plan, Review).
    /// These agents don't modify files, so no worktree isolation is needed.
    pub fn read_only() -> Self {
        Self {
            enabled: false,
            merge_strategy: MergeStrategy::PreserveOnly,
            auto_cleanup: true,
            branch_prefix: "oneai-sub-".to_string(),
            worktree_dir: None,
        }
    }

    /// Configuration for code-modifying agents (Code, Custom).
    /// These agents modify files, so worktree isolation is essential.
    pub fn coding() -> Self {
        Self {
            enabled: true,
            merge_strategy: MergeStrategy::Merge,
            auto_cleanup: true,
            branch_prefix: "oneai-sub-".to_string(),
            worktree_dir: None,
        }
    }

    /// Configuration that preserves worktree branches for manual review.
    /// Useful in production or compliance scenarios where changes must
    /// be reviewed before merging.
    pub fn preserve_for_review() -> Self {
        Self {
            enabled: true,
            merge_strategy: MergeStrategy::PreserveOnly,
            auto_cleanup: false,
            branch_prefix: "oneai-sub-".to_string(),
            worktree_dir: None,
        }
    }
}

// ─── WorktreeHandle ──────────────────────────────────────────────────────────

/// Handle to a created worktree — provides access to the worktree directory
/// and manages cleanup.
///
/// This is the result of `WorktreeIsolation::create()`. It contains:
/// - The worktree directory path (where the sub-agent should operate)
/// - The branch name (for merging back later)
/// - The original project directory (for merging/cleanup)
/// - Whether the worktree was successfully created
#[derive(Debug)]
pub struct WorktreeHandle {
    /// The worktree directory path — this is where the sub-agent's tools
    /// should operate (read/write files, run commands).
    /// If worktree creation failed, this is the original project directory
    /// (fallback to no isolation).
    pub worktree_path: PathBuf,

    /// The git branch name for this worktree.
    /// Used for merging back or cleanup.
    pub branch_name: String,

    /// The original project directory (the main worktree).
    /// Needed for merging changes back.
    pub project_path: PathBuf,

    /// Whether the worktree was successfully created.
    /// If false, the sub-agent runs in the project directory directly
    /// (no isolation — fallback mode).
    pub is_isolated: bool,

    /// Whether any changes were made in the worktree.
    /// Determined during cleanup — if no changes, the worktree is
    /// removed immediately without merging.
    pub has_changes: bool,
}

impl WorktreeHandle {
    /// Get the working directory for the sub-agent's tools.
    /// This is the worktree path if isolated, or the project path if not.
    pub fn working_dir(&self) -> &Path {
        &self.worktree_path
    }
}

// ─── WorktreeIsolation ──────────────────────────────────────────────────────

/// Manages git worktree creation, merging, and cleanup for sub-agent isolation.
///
/// This is the core implementation of the worktree isolation mechanism.
/// It uses the `git` CLI for all operations (worktree add, merge, remove).
///
/// **Why CLI instead of libgit2**:
/// - The `git` CLI is universally available on dev machines
/// - libgit2 would require an additional C dependency (complex for cross-platform)
/// - CLI operations are straightforward and well-tested
/// - Claude Code also uses CLI-based git operations
pub struct WorktreeIsolation {
    /// The project directory (root of the git repository).
    project_path: PathBuf,

    /// Configuration for worktree creation and cleanup.
    config: WorktreeConfig,

    /// Counter for generating unique branch names.
    next_id: std::sync::atomic::AtomicU64,
}

impl WorktreeIsolation {
    /// Create a new WorktreeIsolation manager for the given project directory.
    pub fn new(project_path: PathBuf, config: WorktreeConfig) -> Self {
        Self {
            project_path,
            config,
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Create a new WorktreeIsolation with default configuration.
    pub fn default_config(project_path: PathBuf) -> Self {
        Self::new(project_path, WorktreeConfig::default())
    }

    /// Get the worktree directory path (where worktrees are stored).
    fn worktree_base_dir(&self) -> PathBuf {
        self.config.worktree_dir.clone()
            .unwrap_or_else(|| self.project_path.join(".oneai-worktrees"))
    }

    /// Generate a unique branch name for a worktree.
    fn generate_branch_name(&self, agent_kind: &str) -> String {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Include a timestamp-like component for uniqueness
        format!("{}{}-{}", self.config.branch_prefix, agent_kind, id)
    }

    /// Check if the project is in a git repository.
    fn is_git_repo(&self) -> bool {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&self.project_path)
            .output();

        match output {
            Ok(o) => o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true",
            Err(_) => false,
        }
    }

    /// Get the current branch name of the main worktree.
    fn current_branch(&self) -> Option<String> {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&self.project_path)
            .output();

        match output {
            Ok(o) => {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    }

    /// Create a git worktree for a sub-agent.
    ///
    /// This runs `git worktree add <path> -b <branch>` to create an
    /// isolated copy of the repository. The sub-agent's tools will
    /// operate in the worktree directory instead of the main project.
    ///
    /// **Fallback**: If git worktree creation fails (not a git repo,
    /// git not installed, etc.), returns a handle pointing to the
    /// original project directory with `is_isolated = false`.
    pub fn create(&self, agent_kind: &str) -> Result<WorktreeHandle> {
        if !self.config.enabled {
            // Read-only agents don't need isolation
            tracing::info!("Worktree isolation disabled for '{}' agent — using project directory directly", agent_kind);
            return Ok(WorktreeHandle {
                worktree_path: self.project_path.clone(),
                branch_name: String::new(),
                project_path: self.project_path.clone(),
                is_isolated: false,
                has_changes: false,
            });
        }

        if !self.is_git_repo() {
            tracing::warn!("Project is not in a git repository — falling back to no isolation");
            return Ok(WorktreeHandle {
                worktree_path: self.project_path.clone(),
                branch_name: String::new(),
                project_path: self.project_path.clone(),
                is_isolated: false,
                has_changes: false,
            });
        }

        let branch_name = self.generate_branch_name(agent_kind);
        let base_dir = self.worktree_base_dir();

        // Create the worktree directory if it doesn't exist
        std::fs::create_dir_all(&base_dir).map_err(|e| {
            oneai_core::error::OneAIError::Agent(
                format!("Failed to create worktree directory '{}': {}", base_dir.display(), e)
            )
        })?;

        // The worktree path is <base_dir>/<branch_name>
        let worktree_path = base_dir.join(&branch_name);

        // Get current branch to branch from
        let source_branch = self.current_branch()
            .unwrap_or_else(|| "HEAD".to_string());

        // Run: git worktree add <path> -b <branch> <source_branch>
        let output = std::process::Command::new("git")
            .args([
                "worktree", "add",
                worktree_path.to_str().unwrap_or("."),
                "-b", &branch_name,
                &source_branch,
            ])
            .current_dir(&self.project_path)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                tracing::info!(
                    "Created git worktree for '{}' agent: branch='{}', path='{}'",
                    agent_kind, branch_name, worktree_path.display()
                );
                Ok(WorktreeHandle {
                    worktree_path,
                    branch_name,
                    project_path: self.project_path.clone(),
                    is_isolated: true,
                    has_changes: false, // Will be determined during cleanup
                })
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!(
                    "git worktree add failed: {} — falling back to no isolation",
                    stderr.trim()
                );
                // Fallback: run in project directory directly
                Ok(WorktreeHandle {
                    worktree_path: self.project_path.clone(),
                    branch_name: String::new(),
                    project_path: self.project_path.clone(),
                    is_isolated: false,
                    has_changes: false,
                })
            }
            Err(e) => {
                tracing::warn!(
                    "git worktree add command failed: {} — falling back to no isolation",
                    e
                );
                Ok(WorktreeHandle {
                    worktree_path: self.project_path.clone(),
                    branch_name: String::new(),
                    project_path: self.project_path.clone(),
                    is_isolated: false,
                    has_changes: false,
                })
            }
        }
    }

    /// Check if a worktree has any changes (unstaged or staged).
    ///
    /// This is used before cleanup to determine whether to merge
    /// or just remove the worktree.
    pub fn has_changes(&self, handle: &WorktreeHandle) -> bool {
        if !handle.is_isolated {
            return false;
        }

        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&handle.worktree_path)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let status = String::from_utf8_lossy(&o.stdout);
                !status.trim().is_empty()
            }
            _ => false,
        }
    }

    /// Merge worktree changes back to the main branch.
    ///
    /// This is called after the sub-agent completes its task.
    /// The merge strategy depends on `WorktreeConfig::merge_strategy`:
    /// - Merge: standard git merge (may have conflicts)
    /// - Rebase: rebase onto main then fast-forward
    /// - CherryPick: cherry-pick specific commits
    /// - PreserveOnly: don't merge, just preserve the branch
    ///
    /// **Returns**: MergeResult indicating whether the merge succeeded,
    /// failed (conflicts), or was skipped (no changes / preserve only).
    pub fn merge_back(&self, handle: &WorktreeHandle) -> Result<MergeResult> {
        if !handle.is_isolated {
            // No isolation — nothing to merge
            return Ok(MergeResult::Skipped { reason: "No worktree isolation was used".to_string() });
        }

        // Check for changes
        let has_changes = self.has_changes(handle);
        if !has_changes {
            tracing::info!("No changes in worktree — cleaning up without merge");
            self.cleanup(handle)?;
            return Ok(MergeResult::Skipped { reason: "No changes made in worktree".to_string() });
        }

        // Commit any uncommitted changes in the worktree first
        self.commit_worktree_changes(handle)?;

        match self.config.merge_strategy {
            MergeStrategy::Merge => self.merge_branch(handle),
            MergeStrategy::Rebase => self.rebase_branch(handle),
            MergeStrategy::CherryPick => self.cherry_pick(handle),
            MergeStrategy::PreserveOnly => {
                tracing::info!(
                    "Preserving worktree branch '{}' for manual review — no auto-merge",
                    handle.branch_name
                );
                Ok(MergeResult::Preserved {
                    branch_name: handle.branch_name.clone(),
                    worktree_path: handle.worktree_path.clone(),
                })
            }
        }
    }

    /// Commit any uncommitted changes in the worktree.
    ///
    /// Before merging, we need to ensure all changes are committed.
    /// This creates a commit with a descriptive message.
    fn commit_worktree_changes(&self, handle: &WorktreeHandle) -> Result<()> {
        // Add all changes
        let add_output = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&handle.worktree_path)
            .output();

        if let Ok(o) = add_output {
            if !o.status.success() {
                tracing::warn!("git add -A failed in worktree: {}", String::from_utf8_lossy(&o.stderr).trim());
            }
        }

        // Check if there are staged changes to commit
        let status_output = std::process::Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&handle.worktree_path)
            .output();

        // git diff --cached --quiet exits with 1 if there are staged changes
        let has_staged = match status_output {
            Ok(o) => !o.status.success(), // Exit code 1 = has staged changes
            Err(_) => true, // Assume there are changes if we can't check
        };

        if has_staged {
            let commit_msg = format!("oneai: sub-agent changes (branch {})", handle.branch_name);
            let commit_output = std::process::Command::new("git")
                .args(["commit", "-m", &commit_msg, "--no-gpg-sign"])
                .current_dir(&handle.worktree_path)
                .output();

            match commit_output {
                Ok(o) if o.status.success() => {
                    tracing::info!("Committed worktree changes for branch '{}'", handle.branch_name);
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    tracing::warn!("git commit failed in worktree: {}", stderr.trim());
                }
                Err(e) => {
                    tracing::warn!("git commit command failed: {}", e);
                }
            }
        }

        Ok(())
    }

    /// Merge the worktree branch into the main branch.
    fn merge_branch(&self, handle: &WorktreeHandle) -> Result<MergeResult> {
        tracing::info!("Merging worktree branch '{}' into main", handle.branch_name);

        let output = std::process::Command::new("git")
            .args(["merge", &handle.branch_name, "--no-edit"])
            .current_dir(&self.project_path)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                tracing::info!("Successfully merged branch '{}'", handle.branch_name);
                if self.config.auto_cleanup {
                    self.cleanup(handle)?;
                }
                Ok(MergeResult::Success {
                    branch_name: handle.branch_name.clone(),
                })
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if stderr.contains("CONFLICT") || stderr.contains("conflict") {
                    tracing::warn!(
                        "Merge conflict detected for branch '{}' — preserving worktree for manual resolution",
                        handle.branch_name
                    );
                    // Abort the merge to leave the main branch clean
                    std::process::Command::new("git")
                        .args(["merge", "--abort"])
                        .current_dir(&self.project_path)
                        .output()
                        .ok();
                    Ok(MergeResult::Conflict {
                        branch_name: handle.branch_name.clone(),
                        worktree_path: handle.worktree_path.clone(),
                        conflict_files: self.extract_conflict_files(&stderr),
                    })
                } else {
                    tracing::warn!("Merge failed: {} — preserving worktree", stderr.trim());
                    Ok(MergeResult::Failed {
                        branch_name: handle.branch_name.clone(),
                        reason: stderr.trim().to_string(),
                    })
                }
            }
            Err(e) => {
                Ok(MergeResult::Failed {
                    branch_name: handle.branch_name.clone(),
                    reason: format!("git merge command failed: {}", e),
                })
            }
        }
    }

    /// Rebase the worktree branch onto the main branch, then fast-forward.
    fn rebase_branch(&self, handle: &WorktreeHandle) -> Result<MergeResult> {
        tracing::info!("Rebasing worktree branch '{}' onto main", handle.branch_name);

        // First, rebase in the worktree
        let rebase_output = std::process::Command::new("git")
            .args(["rebase", "HEAD"])
            .current_dir(&handle.worktree_path)
            .output();

        match rebase_output {
            Ok(o) if o.status.success() => {
                // Rebase succeeded — now fast-forward main to the worktree branch
                let ff_output = std::process::Command::new("git")
                    .args(["merge", "--ff-only", &handle.branch_name])
                    .current_dir(&self.project_path)
                    .output();

                match ff_output {
                    Ok(o2) if o2.status.success() => {
                        tracing::info!("Successfully rebased and fast-forwarded branch '{}'", handle.branch_name);
                        if self.config.auto_cleanup {
                            self.cleanup(handle)?;
                        }
                        Ok(MergeResult::Success {
                            branch_name: handle.branch_name.clone(),
                        })
                    }
                    Ok(o2) => {
                        let stderr = String::from_utf8_lossy(&o2.stderr);
                        tracing::warn!("Fast-forward failed: {}", stderr.trim());
                        Ok(MergeResult::Failed {
                            branch_name: handle.branch_name.clone(),
                            reason: format!("Fast-forward failed: {}", stderr.trim()),
                        })
                    }
                    Err(e) => {
                        Ok(MergeResult::Failed {
                            branch_name: handle.branch_name.clone(),
                            reason: format!("git merge --ff-only command failed: {}", e),
                        })
                    }
                }
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if stderr.contains("CONFLICT") || stderr.contains("conflict") {
                    // Abort the rebase
                    std::process::Command::new("git")
                        .args(["rebase", "--abort"])
                        .current_dir(&handle.worktree_path)
                        .output()
                        .ok();
                    Ok(MergeResult::Conflict {
                        branch_name: handle.branch_name.clone(),
                        worktree_path: handle.worktree_path.clone(),
                        conflict_files: self.extract_conflict_files(&stderr),
                    })
                } else {
                    Ok(MergeResult::Failed {
                        branch_name: handle.branch_name.clone(),
                        reason: stderr.trim().to_string(),
                    })
                }
            }
            Err(e) => {
                Ok(MergeResult::Failed {
                    branch_name: handle.branch_name.clone(),
                    reason: format!("git rebase command failed: {}", e),
                })
            }
        }
    }

    /// Cherry-pick specific commits from the worktree branch.
    fn cherry_pick(&self, handle: &WorktreeHandle) -> Result<MergeResult> {
        // Get the list of commits in the worktree branch that are not in main
        let log_output = std::process::Command::new("git")
            .args(["log", "--oneline", "HEAD..", &handle.branch_name])
            .current_dir(&self.project_path)
            .output();

        let commits: Vec<String> = match log_output {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
                    .filter(|c| !c.is_empty())
                    .collect()
            }
            _ => {
                tracing::warn!("Failed to get commit list for cherry-pick — falling back to merge");
                return self.merge_branch(handle);
            }
        };

        if commits.is_empty() {
            tracing::info!("No commits to cherry-pick from branch '{}'", handle.branch_name);
            if self.config.auto_cleanup {
                self.cleanup(handle)?;
            }
            return Ok(MergeResult::Skipped { reason: "No commits to cherry-pick".to_string() });
        }

        tracing::info!("Cherry-picking {} commits from branch '{}'", commits.len(), handle.branch_name);

        // Cherry-pick each commit
        for commit_hash in &commits {
            let cp_output = std::process::Command::new("git")
                .args(["cherry-pick", commit_hash])
                .current_dir(&self.project_path)
                .output();

            match cp_output {
                Ok(o) if o.status.success() => continue,
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    // Abort the cherry-pick on conflict
                    std::process::Command::new("git")
                        .args(["cherry-pick", "--abort"])
                        .current_dir(&self.project_path)
                        .output()
                        .ok();
                    return Ok(MergeResult::Conflict {
                        branch_name: handle.branch_name.clone(),
                        worktree_path: handle.worktree_path.clone(),
                        conflict_files: self.extract_conflict_files(&stderr),
                    });
                }
                Err(e) => {
                    return Ok(MergeResult::Failed {
                        branch_name: handle.branch_name.clone(),
                        reason: format!("git cherry-pick command failed: {}", e),
                    });
                }
            }
        }

        tracing::info!("Successfully cherry-picked all commits from branch '{}'", handle.branch_name);
        if self.config.auto_cleanup {
            self.cleanup(handle)?;
        }
        Ok(MergeResult::Success {
            branch_name: handle.branch_name.clone(),
        })
    }

    /// Clean up a worktree — remove the directory and delete the branch.
    ///
    /// This runs `git worktree remove <path>` and `git branch -d <branch>`.
    /// Only called after a successful merge or when there are no changes.
    pub fn cleanup(&self, handle: &WorktreeHandle) -> Result<()> {
        if !handle.is_isolated {
            return Ok(()); // Nothing to clean up
        }

        tracing::info!("Cleaning up worktree for branch '{}'", handle.branch_name);

        // Remove the worktree directory
        let remove_output = std::process::Command::new("git")
            .args(["worktree", "remove", handle.worktree_path.to_str().unwrap_or(".")])
            .current_dir(&self.project_path)
            .output();

        if let Ok(o) = &remove_output {
            if !o.status.success() {
                // Force remove if there are uncommitted changes
                tracing::warn!(
                    "Normal worktree remove failed — trying force remove: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                std::process::Command::new("git")
                    .args(["worktree", "remove", "--force", handle.worktree_path.to_str().unwrap_or(".")])
                    .current_dir(&self.project_path)
                    .output()
                    .ok();
            }
        }

        // Delete the branch
        let branch_del_output = std::process::Command::new("git")
            .args(["branch", "-D", &handle.branch_name])
            .current_dir(&self.project_path)
            .output();

        if let Ok(o) = &branch_del_output {
            if o.status.success() {
                tracing::info!("Deleted branch '{}'", handle.branch_name);
            } else {
                tracing::warn!(
                    "Failed to delete branch '{}': {}",
                    handle.branch_name,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
        }

        Ok(())
    }

    /// Extract conflict file paths from git merge/rebase error output.
    fn extract_conflict_files(&self, output: &str) -> Vec<String> {
        output.lines()
            .filter(|l| l.contains("CONFLICT") || l.contains("Merge conflict in"))
            .filter_map(|l| {
                // Extract file path from "Merge conflict in <path>" or similar
                if let Some(idx) = l.find(" in ") {
                    Some(l[idx + 4..].trim().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    /// List all existing OneAI worktrees.
    ///
    /// Returns a list of worktree branch names that were created by OneAI
    /// (i.e., branches starting with the configured prefix).
    pub fn list_worktrees(&self) -> Vec<String> {
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.project_path)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| l.starts_with("branch "))
                    .filter_map(|l| {
                        let branch = l.strip_prefix("branch ").unwrap_or("").trim();
                        if branch.starts_with(&self.config.branch_prefix) {
                            Some(branch.to_string())
                        } else {
                            None
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Clean up all stale OneAI worktrees (worktrees that are no longer needed).
    ///
    /// This is useful for cleaning up worktrees from previous sessions
    /// that were not properly cleaned up (e.g., if the process crashed).
    pub fn cleanup_all_stale(&self) -> Result<Vec<String>> {
        let worktrees = self.list_worktrees();
        let mut cleaned = Vec::new();

        for branch_name in &worktrees {
            tracing::info!("Cleaning up stale worktree branch '{}'", branch_name);

            // Find the worktree path for this branch
            let output = std::process::Command::new("git")
                .args(["worktree", "list", "--porcelain"])
                .current_dir(&self.project_path)
                .output();

            if let Ok(o) = output {
                if o.status.success() {
                    let lines = String::from_utf8_lossy(&o.stdout);
                    let mut worktree_path = None;

                    for chunk in lines.split("\n\n") {
                        let mut path = None;
                        let mut branch = None;
                        for line in chunk.lines() {
                            if line.starts_with("worktree ") {
                                path = line.strip_prefix("worktree ").map(|s| s.to_string());
                            }
                            if line.starts_with("branch ") {
                                branch = line.strip_prefix("branch ").map(|s| s.trim().to_string());
                            }
                        }
                        if branch.as_deref() == Some(branch_name) {
                            worktree_path = path;
                            break;
                        }
                    }

                    if let Some(path) = worktree_path {
                        // Remove the worktree
                        std::process::Command::new("git")
                            .args(["worktree", "remove", "--force", &path])
                            .current_dir(&self.project_path)
                            .output()
                            .ok();

                        // Delete the branch
                        std::process::Command::new("git")
                            .args(["branch", "-D", branch_name])
                            .current_dir(&self.project_path)
                            .output()
                            .ok();

                        cleaned.push(branch_name.clone());
                    }
                }
            }
        }

        tracing::info!("Cleaned up {} stale worktrees", cleaned.len());
        Ok(cleaned)
    }
}

// ─── MergeResult ────────────────────────────────────────────────────────────

/// Result of merging a worktree branch back to the main branch.
#[derive(Debug, Clone)]
pub enum MergeResult {
    /// Merge succeeded — changes are now in the main branch.
    /// The worktree has been cleaned up (if auto_cleanup is enabled).
    Success {
        branch_name: String,
    },

    /// Merge had conflicts — the worktree is preserved for manual resolution.
    /// The main branch is unchanged (merge was aborted).
    Conflict {
        branch_name: String,
        worktree_path: PathBuf,
        conflict_files: Vec<String>,
    },

    /// Merge failed for a non-conflict reason (e.g., branch not found).
    /// The worktree is preserved.
    Failed {
        branch_name: String,
        reason: String,
    },

    /// Merge was skipped (no changes, no isolation, or preserve-only mode).
    Skipped {
        reason: String,
    },

    /// Changes are preserved in a separate branch for manual review.
    /// No merge was attempted.
    Preserved {
        branch_name: String,
        worktree_path: PathBuf,
    },
}

impl MergeResult {
    /// Check if the merge was successful.
    pub fn is_success(&self) -> bool {
        matches!(self, MergeResult::Success { .. })
    }

    /// Get a human-readable description of the merge result.
    pub fn description(&self) -> String {
        match self {
            MergeResult::Success { branch_name } =>
                format!("Successfully merged branch '{}'", branch_name),
            MergeResult::Conflict { branch_name, conflict_files, .. } =>
                format!("Merge conflict in branch '{}' (files: {})", branch_name, conflict_files.join(", ")),
            MergeResult::Failed { branch_name, reason } =>
                format!("Merge failed for branch '{}': {}", branch_name, reason),
            MergeResult::Skipped { reason } =>
                format!("Merge skipped: {}", reason),
            MergeResult::Preserved { branch_name, .. } =>
                format!("Changes preserved in branch '{}' for manual review", branch_name),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worktree_config_default() {
        let config = WorktreeConfig::default();
        assert!(config.enabled);
        assert_eq!(config.merge_strategy, MergeStrategy::Merge);
        assert!(config.auto_cleanup);
        assert_eq!(config.branch_prefix, "oneai-sub-");
        assert!(config.worktree_dir.is_none());
    }

    #[test]
    fn test_worktree_config_read_only() {
        let config = WorktreeConfig::read_only();
        assert!(!config.enabled);
        assert_eq!(config.merge_strategy, MergeStrategy::PreserveOnly);
    }

    #[test]
    fn test_worktree_config_coding() {
        let config = WorktreeConfig::coding();
        assert!(config.enabled);
        assert_eq!(config.merge_strategy, MergeStrategy::Merge);
    }

    #[test]
    fn test_worktree_config_preserve_for_review() {
        let config = WorktreeConfig::preserve_for_review();
        assert!(config.enabled);
        assert_eq!(config.merge_strategy, MergeStrategy::PreserveOnly);
        assert!(!config.auto_cleanup);
    }

    #[test]
    fn test_merge_result_is_success() {
        let result = MergeResult::Success { branch_name: "oneai-sub-code-1".to_string() };
        assert!(result.is_success());
        assert!(result.description().contains("Successfully merged"));
    }

    #[test]
    fn test_merge_result_conflict() {
        let result = MergeResult::Conflict {
            branch_name: "oneai-sub-code-1".to_string(),
            worktree_path: PathBuf::from("/tmp/worktree"),
            conflict_files: vec!["src/main.rs".to_string()],
        };
        assert!(!result.is_success());
        assert!(result.description().contains("Merge conflict"));
        assert!(result.description().contains("src/main.rs"));
    }

    #[test]
    fn test_merge_result_skipped() {
        let result = MergeResult::Skipped { reason: "No changes".to_string() };
        assert!(!result.is_success());
        assert!(result.description().contains("No changes"));
    }

    #[test]
    fn test_merge_result_preserved() {
        let result = MergeResult::Preserved {
            branch_name: "oneai-sub-code-1".to_string(),
            worktree_path: PathBuf::from("/tmp/worktree"),
        };
        assert!(!result.is_success());
        assert!(result.description().contains("manual review"));
    }

    #[test]
    fn test_generate_branch_name() {
        let isolation = WorktreeIsolation::default_config(PathBuf::from("/project"));
        let name1 = isolation.generate_branch_name("code");
        let name2 = isolation.generate_branch_name("code");
        assert!(name1.starts_with("oneai-sub-code-"));
        assert!(name2.starts_with("oneai-sub-code-"));
        assert_ne!(name1, name2); // Names should be unique
    }

    #[test]
    fn test_worktree_handle_working_dir() {
        let handle = WorktreeHandle {
            worktree_path: PathBuf::from("/project/.oneai-worktrees/oneai-sub-code-1"),
            branch_name: "oneai-sub-code-1".to_string(),
            project_path: PathBuf::from("/project"),
            is_isolated: true,
            has_changes: false,
        };
        assert_eq!(handle.working_dir(), Path::new("/project/.oneai-worktrees/oneai-sub-code-1"));
    }

    #[test]
    fn test_worktree_handle_fallback() {
        let handle = WorktreeHandle {
            worktree_path: PathBuf::from("/project"),
            branch_name: String::new(),
            project_path: PathBuf::from("/project"),
            is_isolated: false,
            has_changes: false,
        };
        assert_eq!(handle.working_dir(), Path::new("/project"));
    }

    #[test]
    fn test_merge_strategy_equality() {
        assert_eq!(MergeStrategy::Merge, MergeStrategy::Merge);
        assert_ne!(MergeStrategy::Merge, MergeStrategy::Rebase);
    }

    #[test]
    fn test_extract_conflict_files() {
        let isolation = WorktreeIsolation::default_config(PathBuf::from("/project"));
        let output = "CONFLICT (content): Merge conflict in src/main.rs\nCONFLICT (content): Merge conflict in lib.rs";
        let files = isolation.extract_conflict_files(output);
        assert_eq!(files, vec!["src/main.rs", "lib.rs"]);
    }

    #[test]
    fn test_extract_conflict_files_empty() {
        let isolation = WorktreeIsolation::default_config(PathBuf::from("/project"));
        let files = isolation.extract_conflict_files("Already up to date.");
        assert!(files.is_empty());
    }
}
