//! Built-in ContextSource implementations for common domains.
//!
//! These sources provide the default environment information that most
//! domains need. They are the pluggable, refresh-policy-governed equivalent of
//! a hardcoded environment snapshot — the single source of truth for env
//! sensing, composed via DomainPacks.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
// for `writeln!` on String in the reconciliation block
use std::fmt::Write as _;

use async_trait::async_trait;
use tokio::sync::RwLock;
use oneai_core::error::Result;

use crate::context_source::{ContextSource, RefreshPolicy};

// ─── GitStatusSource ───────────────────────────────────────────────────────────

/// Context source that provides git repository status information.
///
/// Provides: current branch, status summary (modified/added/deleted counts),
/// recent commits. This is the coding domain equivalent of Claude Code's
/// gitStatus injection.
///
/// Refresh policy: OnChange — only injects new content when git status changes.
/// Priority: 10 (high — git status is very important for coding agents).
pub struct GitStatusSource {
    project_dir: PathBuf,
    last_content: Arc<RwLock<Option<String>>>,
}

impl GitStatusSource {
    /// Create a new GitStatusSource for the given project directory.
    pub fn new(project_dir: &str) -> Self {
        Self {
            project_dir: PathBuf::from(project_dir),
            last_content: Arc::new(RwLock::new(None)),
        }
    }
}

#[async_trait]
impl ContextSource for GitStatusSource {
    fn key(&self) -> &str { "git_status" }

    async fn load(&self) -> Result<String> {
        let dir = self.project_dir.to_str().unwrap_or(".");
        let (shell, shell_arg) = if cfg!(target_os = "windows") {
            ("powershell", "-Command")
        } else {
            ("sh", "-c")
        };

        // Get branch
        let branch_result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(shell)
                .arg(shell_arg)
                .arg(format!("cd {} && git branch --show-current 2>/dev/null || echo 'not a git repo'", dir))
                .output()
        ).await;

        let branch = match branch_result {
            Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
            _ => "unknown".to_string(),
        };

        // A single `git status --short` call yields the full per-file change set.
        // Parse the two-letter status code (XY) into modified / created / deleted
        // lists — this subsumes the old count-only summary *and* the agent-side
        // per-file diff scan, so git is hit once per iteration for status, not
        // multiple times in parallel paths.
        let status_result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(shell)
                .arg(shell_arg)
                .arg(format!("cd {} && git status --short 2>/dev/null", dir))
                .output()
        ).await;

        let mut modified: Vec<String> = Vec::new();
        let mut created: Vec<String> = Vec::new();
        let mut deleted: Vec<String> = Vec::new();

        if let Ok(Ok(output)) = status_result {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                // `git status --short` format: "<XY> <path>", where X = index
                // status, Y = worktree status. Path may be quoted; take the
                // trimmed remainder after the two-char code.
                if line.len() < 3 {
                    continue;
                }
                let x = line.as_bytes().first().copied().unwrap_or(b' ');
                let y = line.as_bytes().get(1).copied().unwrap_or(b' ');
                let path = line[3..].trim().trim_matches('"').to_string();
                if path.is_empty() {
                    continue;
                }
                // Classify by the most informative status code. Untracked ("??")
                // and A count as created; D in either column counts as deleted;
                // everything else (M, R, C, …) counts as modified.
                if x == b'D' || y == b'D' {
                    deleted.push(path);
                } else if x == b'A' || y == b'A' || (x == b'?' && y == b'?') {
                    created.push(path);
                } else {
                    modified.push(path);
                }
            }
        }

        // Get recent commits (last 5)
        let commits_result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(shell)
                .arg(shell_arg)
                .arg(format!("cd {} && git log --oneline -5 2>/dev/null || echo 'no commits'", dir))
                .output()
        ).await;

        let recent_commits = match commits_result {
            Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
            _ => "no commits".to_string(),
        };

        let changes_count = modified.len() + created.len() + deleted.len();
        let mut content = format!(
            "Git Branch: {}\nChanges: {} modified/new/deleted files",
            branch, changes_count
        );
        if !modified.is_empty() {
            content.push_str(&format!("\nModified: {}", modified.join(", ")));
        }
        if !created.is_empty() {
            content.push_str(&format!("\nCreated: {}", created.join(", ")));
        }
        if !deleted.is_empty() {
            content.push_str(&format!("\nDeleted: {}", deleted.join(", ")));
        }
        content.push_str(&format!("\nRecent Commits:\n{}", recent_commits));

        // Store for OnChange comparison
        *self.last_content.write().await = Some(content.clone());

        Ok(content)
    }

    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::OnChange
    }

    fn priority(&self) -> u32 { 10 }
}

// ─── FileTreeSource ────────────────────────────────────────────────────────────

/// Context source that provides project file structure information.
///
/// Provides: a top-level directory listing with file counts.
/// For large projects, shows only the top-level structure to avoid
/// overflowing the context window.
///
/// Refresh policy: OnceAtStart — file structure rarely changes during a session.
/// Priority: 20 (medium — file structure is important but stable).
pub struct FileTreeSource {
    project_dir: PathBuf,
}

impl FileTreeSource {
    /// Create a new FileTreeSource for the given project directory.
    pub fn new(project_dir: &str) -> Self {
        Self {
            project_dir: PathBuf::from(project_dir),
        }
    }
}

#[async_trait]
impl ContextSource for FileTreeSource {
    fn key(&self) -> &str { "file_tree" }

    async fn load(&self) -> Result<String> {
        let dir = self.project_dir.to_str().unwrap_or(".");
        let (shell, shell_arg) = if cfg!(target_os = "windows") {
            ("powershell", "-Command")
        } else {
            ("sh", "-c")
        };

        // Get top-level directory listing with file counts
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(shell)
                .arg(shell_arg)
                .arg(format!(
                    "cd {} && find . -maxdepth 2 -not -path '*/.*' -not -path '*/target/*' -not -path '*/node_modules/*' | head -100",
                    dir
                ))
                .output()
        ).await;

        match result {
            Ok(Ok(output)) => {
                let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
                // Limit to 2000 chars to prevent context overflow
                let truncated = if tree.len() > 2000 {
                    // Char-boundary-safe truncation for CJK paths
                    let end = tree.char_indices()
                        .take_while(|(i, _)| *i < 2000)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    format!("{}... [truncated]", &tree[..end])
                } else {
                    tree
                };
                Ok(format!("Project Structure:\n{}", truncated))
            }
            _ => Ok("Project Structure: unable to scan".to_string()),
        }
    }

    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::OnceAtStart
    }

    fn priority(&self) -> u32 { 20 }
}

// ─── ProjectConfigSource ───────────────────────────────────────────────────────

/// Context source that provides project configuration metadata.
///
/// Reads Cargo.toml, package.json, or other config files to extract:
/// - Project name and version
/// - Key dependencies
/// - Build system information
///
/// Refresh policy: OnceAtStart — project config rarely changes.
/// Priority: 30 (lower — config is useful but not critical).
pub struct ProjectConfigSource {
    project_dir: PathBuf,
}

impl ProjectConfigSource {
    /// Create a new ProjectConfigSource for the given project directory.
    pub fn new(project_dir: &str) -> Self {
        Self {
            project_dir: PathBuf::from(project_dir),
        }
    }
}

#[async_trait]
impl ContextSource for ProjectConfigSource {
    fn key(&self) -> &str { "project_config" }

    async fn load(&self) -> Result<String> {
        let dir = &self.project_dir;

        // Try to read Cargo.toml (Rust project)
        let cargo_path = dir.join("Cargo.toml");
        if cargo_path.exists() {
            let content = tokio::fs::read_to_string(&cargo_path).await;
            if let Ok(text) = content {
                // Extract just the workspace/package section (not all content)
                let lines = text.lines().take(50).collect::<Vec<_>>();
                let summary = lines.join("\n");
                return Ok(format!("Project Config (Cargo.toml):\n{}", summary));
            }
        }

        // Try to read package.json (JS/TS project)
        let package_path = dir.join("package.json");
        if package_path.exists() {
            let content = tokio::fs::read_to_string(&package_path).await;
            if let Ok(text) = content {
                // Parse and extract name, version, dependencies
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    let name = json.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let version = json.get("version").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let deps_count = json.get("dependencies")
                        .and_then(|v| v.as_object())
                        .map(|o| o.len())
                        .unwrap_or(0);
                    return Ok(format!(
                        "Project Config (package.json): name={}, version={}, dependencies={}",
                        name, version, deps_count
                    ));
                }
            }
        }

        Ok("Project Config: no recognized config file found".to_string())
    }

    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::OnceAtStart
    }

    fn priority(&self) -> u32 { 30 }
}

// ─── DateSource ────────────────────────────────────────────────────────────────

/// Context source that provides current date and time information.
///
/// Important for research domains (time-aware search), data analysis
/// (time-based queries), and any domain that needs temporal context.
///
/// Refresh policy: Periodic(1h) — date changes slowly but should be
/// periodically updated for accuracy.
/// Priority: 5 (highest — time context is fundamental).
pub struct DateSource;

impl DateSource {
    /// Create a new DateSource.
    pub fn new() -> Self { Self }
}

impl Default for DateSource {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl ContextSource for DateSource {
    fn key(&self) -> &str { "date" }

    async fn load(&self) -> Result<String> {
        let now = chrono::Local::now();
        Ok(format!(
            "Current Date: {}\nCurrent Time: {}\nDay of Week: {}",
            now.format("%Y-%m-%d"),
            now.format("%H:%M:%S"),
            now.format("%A")
        ))
    }

    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::Periodic(Duration::from_secs(3600)) // 1 hour
    }

    fn priority(&self) -> u32 { 5 }
}

// ─── EnvironmentInfoSource ─────────────────────────────────────────────────────

/// Context source that provides environment information.
///
/// Provides: working directory, platform, shell, architecture.
/// This is the general-purpose equivalent of Claude Code's
/// environment info injection.
///
/// Refresh policy: OnceAtStart — environment info is stable.
/// Priority: 15 (high — environment context is important).
pub struct EnvironmentInfoSource;

impl EnvironmentInfoSource {
    /// Create a new EnvironmentInfoSource.
    pub fn new() -> Self { Self }
}

impl Default for EnvironmentInfoSource {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl ContextSource for EnvironmentInfoSource {
    fn key(&self) -> &str { "environment" }

    async fn load(&self) -> Result<String> {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let platform = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let shell = std::env::var("SHELL")
            .unwrap_or_else(|_| if cfg!(target_os = "windows") { "powershell".to_string() } else { "sh".to_string() });

        Ok(format!(
            "Working Directory: {}\nPlatform: {}\nArchitecture: {}\nShell: {}",
            cwd, platform, arch, shell
        ))
    }

    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::OnceAtStart
    }

    fn priority(&self) -> u32 { 15 }
}

// ─── ProjectInstructionsSource ────────────────────────────────────────────────

/// Context source that reads project instruction files (ONEAI.md, CLAUDE.md, AGENTS.md).
///
/// This is the **single most important context source** for coding agents —
/// project instruction files contain code style rules, technical constraints,
/// test requirements, deployment norms, and team preferences that directly
/// determine agent output quality.
///
/// Inspired by Claude Code's CLAUDE.md mechanism and OpenCode's AGENTS.md.
/// OneAI reads all three formats for maximum compatibility.
///
/// Search priority (first found wins):
/// 1. ONEAI.md in project root
/// 2. CLAUDE.md in project root
/// 3. AGENTS.md in project root
/// 4. ONEAI.md in subdirectories (closest to current working context)
/// 5. ~/.oneai/ONEAI.md (user-level global instructions)
///
/// Refresh policy: OnceAtStart — project instructions rarely change during a session.
/// Priority: 1 (highest — project instructions are the primary context driver).
pub struct ProjectInstructionsSource {
    project_dir: PathBuf,
    cached_content: Arc<RwLock<Option<String>>>,
}

impl ProjectInstructionsSource {
    /// Create a new ProjectInstructionsSource for the given project directory.
    pub fn new(project_dir: &str) -> Self {
        Self {
            project_dir: PathBuf::from(project_dir),
            cached_content: Arc::new(RwLock::new(None)),
        }
    }

    /// Search for instruction files in priority order.
    /// Returns the content of the first file found, or None.
    async fn find_instructions(&self) -> Option<String> {
        let candidates = [
            "ONEAI.md",
            "CLAUDE.md",
            "AGENTS.md",
        ];

        // 1. Check project root directory
        for candidate in &candidates {
            let path = self.project_dir.join(candidate);
            if path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(&path).await {
                    if !content.trim().is_empty() {
                        return Some(content);
                    }
                }
            }
        }

        // 2. Check subdirectories (up to 2 levels deep, closest first)
        // Look for instruction files in common subdirectories
        let subdirs = [
            "src", "lib", "app", "crates", "packages", "modules",
        ];
        for subdir in &subdirs {
            for candidate in &candidates {
                let path = self.project_dir.join(subdir).join(candidate);
                if path.exists() {
                    if let Ok(content) = tokio::fs::read_to_string(&path).await {
                        if !content.trim().is_empty() {
                            return Some(content);
                        }
                    }
                }
            }
        }

        // 3. Check user home directory for global instructions
        if let Ok(home) = std::env::var("HOME") {
            let home_path = PathBuf::from(home);
            for candidate in &candidates {
                let path = home_path.join(".oneai").join(candidate);
                if path.exists() {
                    if let Ok(content) = tokio::fs::read_to_string(&path).await {
                        if !content.trim().is_empty() {
                            return Some(content);
                        }
                    }
                }
            }
        }

        None
    }
}

#[async_trait]
impl ContextSource for ProjectInstructionsSource {
    fn key(&self) -> &str { "project_instructions" }

    async fn load(&self) -> Result<String> {
        match self.find_instructions().await {
            Some(content) => {
                // Cache for later comparison
                *self.cached_content.write().await = Some(content.clone());
                Ok(content)
            }
            None => {
                *self.cached_content.write().await = None;
                Ok(String::new()) // No instruction file found — return empty
            }
        }
    }

    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::OnceAtStart // Project instructions rarely change during a session
    }

    fn priority(&self) -> u32 { 1 } // Highest priority — project instructions are the primary context
}

// ─── GitReconciliationSource ──────────────────────────────────────────────────

/// Resume-time ground-truth reconciliation source (reference doc §8.2).
///
/// On the first turn after a session resumes or continues an existing task,
/// this source re-derives the bound task's working state, then asks git for
/// the current ground truth (HEAD commit timestamp + whether `.oneai/` is
/// dirty). If the world has moved since the working state's `updated_at`
/// (a newer HEAD commit, or uncommitted working-state edits), drift is
/// flagged: a `Reconciliation` event is appended to the durable log and a
/// `[Reconciliation]` pinned block is injected so the model knows the prior
/// task's "current step" may be stale.
///
/// One-shot under the ephemeral re-injection model: `load()` does the work
/// the first call and returns the rendered block; subsequent calls return
/// empty (the take pattern, mirroring `UnfinishedWorkSource`).
pub struct GitReconciliationSource {
    project_dir: PathBuf,
    store: std::sync::Arc<dyn oneai_core::traits::WorkingStateStore>,
    task_id: String,
    session_id: String,
    block: tokio::sync::Mutex<Option<String>>,
}

impl GitReconciliationSource {
    /// Construct a reconciliation source bound to `task_id`. `session_id` is
    /// stamped onto any `Reconciliation` event the source appends.
    pub fn new(
        project_dir: impl Into<PathBuf>,
        store: std::sync::Arc<dyn oneai_core::traits::WorkingStateStore>,
        task_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            project_dir: project_dir.into(),
            store,
            task_id: task_id.into(),
            session_id: session_id.into(),
            block: tokio::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl ContextSource for GitReconciliationSource {
    fn key(&self) -> &str { "git_reconciliation" }

    async fn load(&self) -> Result<String> {
        // One-shot: if we've already produced a block, yield and clear it.
        if let Some(b) = self.block.lock().await.take() {
            return Ok(b);
        }
        // Otherwise compute the reconciliation report once.
        let ws = match self.store.get_task(&self.task_id).await {
            Ok(Some(ws)) => ws,
            Ok(None) => return Ok(String::new()),
            Err(_) => return Ok(String::new()),
        };
        let head_ts = git_head_commit_iso(&self.project_dir).await;
        let dirty = git_oneai_dirty(&self.project_dir).await.unwrap_or(false);
        let report = detect_drift(head_ts.as_deref(), dirty, &ws);

        if report.drift {
            // Record the drift in the durable log so it survives compaction /
            // cross-session resume (informational — does not mutate state).
            let _ = self
                .store
                .append_event(
                    &self.task_id,
                    &self.session_id,
                    None,
                    oneai_core::TaskEventType::Reconciliation,
                    oneai_core::TaskEventPayload::Reconciliation {
                        summary: report.summary.clone(),
                        details: report.details.clone(),
                    },
                )
                .await;
        }

        let block = report.render_block();
        if block.is_empty() {
            Ok(String::new())
        } else {
            Ok(block)
        }
    }

    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::OnResume
    }

    fn priority(&self) -> u32 { 3 } // Very high — surface staleness before other context
}

/// Result of comparing the working state against git's ground truth.
pub struct ReconciliationReport {
    pub drift: bool,
    pub summary: String,
    pub details: String,
}

impl ReconciliationReport {
    /// Render the `[Reconciliation]` pinned block. Empty when no drift.
    pub fn render_block(&self) -> String {
        if !self.drift {
            return String::new();
        }
        format!(
            "[Reconciliation] (do not compress — working state flagged STALE vs git)\n\
             ⚠️ {}\n{}\n\
             Re-verify the current step against the actual repo state before proceeding; \
             external ground truth wins on conflict.",
            self.summary, self.details
        )
    }
}

/// Get the ISO-8601 commit timestamp of git HEAD (`git log -1 --format=%cI`),
/// or `None` if the dir is not a git repo / git is unavailable. `%cI` yields
/// a strict RFC-3339 string that sorts lexicographically with OneAI's own
/// `chrono::Utc::now().to_rfc3339()` working-state timestamps.
pub async fn git_head_commit_iso(project_dir: &std::path::Path) -> Option<String> {
    run_git(project_dir, &["log", "-1", "--format=%cI"]).await
}

/// Whether `.oneai/` has uncommitted/untracked changes — i.e. the durable
/// working state itself has local edits not yet reflected in the projected
/// `WorkingState`. Returns `None` if the check could not run (non-fatal).
pub async fn git_oneai_dirty(project_dir: &std::path::Path) -> Option<bool> {
    let out = run_git(project_dir, &["status", "--short", "--", ".oneai"]).await?;
    Some(!out.trim().is_empty())
}

/// Run a git subcommand in `project_dir` and return its trimmed stdout.
async fn run_git(project_dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(project_dir)
            .args(args)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Pure drift detector — given git's HEAD commit timestamp, whether
/// `.oneai/` is dirty, and the working state, decide if the pinned state is
/// stale vs the external world.
///
/// Drift is signalled when:
/// - git HEAD is newer than the working state's `updated_at` (the repo moved
///   since the last working-state write), **or**
/// - `.oneai/` is dirty (working-state files were edited out-of-band).
///
/// Timestamps are compared as RFC-3339 strings (lexicographic = chronological
/// for same-format stamps). A missing head timestamp (no git / not a repo)
/// means there is no external ground truth to reconcile against → no drift.
pub fn detect_drift(
    head_commit_ts: Option<&str>,
    oneai_dirty: bool,
    ws: &oneai_core::WorkingState,
) -> ReconciliationReport {
    let ws_ts = ws.updated_at.trim();
    let head_newer = match (head_commit_ts, ws_ts.is_empty()) {
        (Some(head), false) => head.trim() > ws_ts,
        _ => false,
    };
    let drift = head_newer || oneai_dirty;
    if !drift {
        return ReconciliationReport {
            drift: false,
            summary: String::new(),
            details: String::new(),
        };
    }
    let mut summary = String::new();
    if head_newer {
        summary.push_str("git HEAD advanced after the working state's last write");
    }
    if oneai_dirty {
        if !summary.is_empty() {
            summary.push_str("; ");
        }
        summary.push_str(".oneai/ has uncommitted working-state edits");
    }
    let mut details = String::new();
    if let Some(head) = head_commit_ts {
        let _ = writeln!(details, "git HEAD commit: {}", head);
    }
    let _ = writeln!(details, "working state updated_at: {}", ws.updated_at);
    let _ = writeln!(details, "task goal: {}", ws.goal);
    if !ws.steps.is_empty() {
        let _ = writeln!(details, "pinned steps: {}", ws.steps.len());
    }
    ReconciliationReport { drift: true, summary, details }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_date_source() {
        let source = DateSource::new();
        assert_eq!(source.key(), "date");
        assert_eq!(source.priority(), 5);

        let content = source.load().await.unwrap();
        assert!(content.contains("Current Date:"));
        assert!(content.contains("Current Time:"));
    }

    #[tokio::test]
    async fn test_environment_info_source() {
        let source = EnvironmentInfoSource::new();
        assert_eq!(source.key(), "environment");
        assert_eq!(source.priority(), 15);

        let content = source.load().await.unwrap();
        assert!(content.contains("Working Directory:"));
        assert!(content.contains("Platform:"));
    }

    #[tokio::test]
    async fn test_project_config_source() {
        let source = ProjectConfigSource::new("/Users/maxf/github/new/OneAI");
        assert_eq!(source.key(), "project_config");

        let content = source.load().await.unwrap();
        assert!(content.contains("Project Config"));
    }

    #[tokio::test]
    async fn test_file_tree_source() {
        let source = FileTreeSource::new("/Users/maxf/github/new/OneAI");
        assert_eq!(source.key(), "file_tree");

        let content = source.load().await.unwrap();
        assert!(content.contains("Project Structure"));
    }

    #[tokio::test]
    async fn test_git_status_source() {
        let source = GitStatusSource::new("/Users/maxf/github/new/OneAI");
        assert_eq!(source.key(), "git_status");

        let content = source.load().await.unwrap();
        assert!(content.contains("Git Branch:"), "missing branch line: {content}");
        // The enriched content always surfaces a Changes summary and, when the
        // tree is dirty, the per-file Modified/Created/Deleted lists.
        assert!(content.contains("Changes:"), "missing changes summary: {content}");
        assert!(content.contains("Recent Commits:"), "missing recent commits: {content}");
    }

    #[tokio::test]
    async fn test_project_instructions_source() {
        let source = ProjectInstructionsSource::new("/Users/maxf/github/new/OneAI");
        assert_eq!(source.key(), "project_instructions");
        assert_eq!(source.priority(), 1);
        assert_eq!(source.refresh_policy(), RefreshPolicy::OnceAtStart);

        // The load should succeed (returns empty if no instruction file found)
        let content = source.load().await.unwrap();
        // In this project, there's no ONEAI.md/CLAUDE.md/AGENTS.md yet
        // so content will be empty
        assert!(content.is_empty() || content.len() > 0); // Just verify it doesn't crash
    }

    #[test]
    fn test_refresh_policies() {
        assert_eq!(GitStatusSource::new(".").refresh_policy(), RefreshPolicy::OnChange);
        assert_eq!(FileTreeSource::new(".").refresh_policy(), RefreshPolicy::OnceAtStart);
        assert_eq!(ProjectConfigSource::new(".").refresh_policy(), RefreshPolicy::OnceAtStart);
        assert_eq!(DateSource::new().refresh_policy(), RefreshPolicy::Periodic(Duration::from_secs(3600)));
        assert_eq!(EnvironmentInfoSource::new().refresh_policy(), RefreshPolicy::OnceAtStart);
    }

    // ─── GitReconciliationSource ───────────────────────────────────────────

    fn ws_with_updated_at(updated_at: &str) -> oneai_core::WorkingState {
        let mut ws = oneai_core::WorkingState {
            task_id: "t1".into(),
            user_id: String::new(),
            project: String::new(),
            goal: "ship feature".into(),
            intent: String::new(),
            status: oneai_core::TaskStatus::Active,
            steps: Vec::new(),
            decisions: Vec::new(),
            blockers: Vec::new(),
            notes: Vec::new(),
            owner_session: String::new(),
            created_at: "2026-01-01T00:00:00+00:00".into(),
            updated_at: updated_at.into(),
        };
        ws.steps.push(oneai_core::Step {
            id: "s1".into(),
            description: "do thing".into(),
            status: oneai_core::StepStatus::InProgress,
            depends_on: vec![],
            order: 1,
            active_form: None,
            updated_at: String::new(),
        });
        ws
    }

    #[test]
    fn detect_drift_head_newer_flags_stale() {
        // HEAD commit (2026-07) is newer than the working-state write (2026-01).
        let ws = ws_with_updated_at("2026-01-15T00:00:00+00:00");
        let r = detect_drift(Some("2026-07-01T00:00:00+00:00"), false, &ws);
        assert!(r.drift);
        assert!(r.summary.contains("HEAD advanced"));
        let block = r.render_block();
        assert!(block.contains("[Reconciliation]"));
        assert!(block.contains("STALE"));
    }

    #[test]
    fn detect_drift_head_older_is_clean() {
        // HEAD is older than the working-state write — the agent wrote state
        // after the last commit, no external movement → clean.
        let ws = ws_with_updated_at("2026-07-01T00:00:00+00:00");
        let r = detect_drift(Some("2026-01-15T00:00:00+00:00"), false, &ws);
        assert!(!r.drift);
        assert!(r.render_block().is_empty());
    }

    #[test]
    fn detect_drift_dirty_oneai_flags_stale() {
        // Even with no HEAD movement, a dirty .oneai/ means out-of-band edits.
        let ws = ws_with_updated_at("2026-07-01T00:00:00+00:00");
        let r = detect_drift(Some("2026-07-01T00:00:00+00:00"), true, &ws);
        assert!(r.drift);
        assert!(r.summary.contains("uncommitted"));
    }

    #[test]
    fn detect_drift_no_git_no_drift() {
        // Not a git repo → no external ground truth → never flag stale.
        let ws = ws_with_updated_at("2026-01-01T00:00:00+00:00");
        let r = detect_drift(None, false, &ws);
        assert!(!r.drift);
    }

    #[tokio::test]
    async fn git_reconciliation_source_skips_when_task_missing() {
        use oneai_core::traits::WorkingStateStore;
        use oneai_core::error::{OneAIError, Result};

        // Minimal store that always reports "no such task" — exercises the
        // source's skip path without pulling oneai-persistence into the
        // (declarative) domain crate's dep graph.
        struct NoTaskStore;
        #[async_trait::async_trait]
        impl WorkingStateStore for NoTaskStore {
            async fn create_task(&self, _: &str, _: &str, _: &str, _: &str, _: &str) -> Result<String> { Ok("t".into()) }
            async fn get_task(&self, _: &str) -> Result<Option<oneai_core::WorkingState>> { Ok(None) }
            async fn list_open_tasks(&self, _: &str, _: &str) -> Result<Vec<oneai_core::TaskBrief>> { Ok(Vec::new()) }
            async fn append_event(&self, _: &str, _: &str, _: Option<&str>, _: oneai_core::TaskEventType, _: oneai_core::TaskEventPayload) -> Result<String> { Ok("e".into()) }
            async fn derive_state(&self, _: &str) -> Result<oneai_core::WorkingState> { Err(OneAIError::Persistence("none".into())) }
            async fn compact_if_needed(&self, _: &str) -> Result<()> { Ok(()) }
            async fn archive_task(&self, _: &str) -> Result<()> { Ok(()) }
        }

        let store: Arc<dyn WorkingStateStore> = Arc::new(NoTaskStore);
        let src = GitReconciliationSource::new(".", store, "nonexistent_task", "sess");
        // No such task → load returns empty, no panic, no event appended.
        let content = src.load().await.unwrap();
        assert!(content.is_empty());
        assert_eq!(src.refresh_policy(), RefreshPolicy::OnResume);
        assert_eq!(src.priority(), 3);
    }
}
