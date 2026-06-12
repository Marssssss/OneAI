//! Built-in ContextSource implementations for common domains.
//!
//! These sources provide the default environment information that most
//! domains need. They replace the hardcoded `EnvironmentSnapshot` with
//! pluggable implementations that can be independently configured.

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

        // Get status summary
        let status_result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(shell)
                .arg(shell_arg)
                .arg(format!("cd {} && git status --short 2>/dev/null | wc -l", dir))
                .output()
        ).await;

        let changes_count = match status_result {
            Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
            _ => "0".to_string(),
        };

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

        let content = format!(
            "Git Branch: {}\nChanges: {} modified/new/deleted files\nRecent Commits:\n{}",
            branch, changes_count, recent_commits
        );

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
                    format!("{}... [truncated]", &tree[..2000])
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
        assert!(content.contains("Git Branch:"));
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
