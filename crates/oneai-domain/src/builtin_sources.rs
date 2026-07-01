//! Built-in ContextSource implementations for common domains.
//!
//! These sources provide the default environment information that most
//! domains need. They are the pluggable, refresh-policy-governed equivalent of
//! a hardcoded environment snapshot — the single source of truth for env
//! sensing, composed via DomainPacks.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
}
