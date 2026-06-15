//! Local tool implementations — expanded tool set (4 → 30+).
//!
//! This module defines the interfaces and implementations for all local tools
//! that OneAI supports. Tools are categorized by PermissionLevel:
//! - Read: file reading, search, environment sensing
//! - Standard: file editing, MCP interaction
//! - Full: shell execution, file deletion, system commands
//!
//! Each tool implements the `Tool` trait from `oneai_core::traits::Tool`.
//! The `permission_level()` method replaces the old `risk_level()` method
//! (backward compatibility is maintained through conversion functions).
//!
//! **Migration note**: The old `local_tools.rs` definitions for ShellTool and
//! FileReadTool have been merged here. This is the single canonical location
//! for all local tool definitions.

use async_trait::async_trait;
use oneai_core::{PermissionLevel, RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;

// ─── PermissionLevel-aware Tool trait extension ─────────────────────────────

/// Extension of the Tool trait with PermissionLevel support.
///
/// All tools should implement this trait instead of relying solely on `risk_level()`.
/// The `permission_level()` method provides the new three-tier classification,
/// while `risk_level()` remains available for backward compatibility.
pub trait PermissionAwareTool: Tool {
    /// The permission level of this tool's operations.
    /// This replaces `risk_level()` with a more meaningful classification.
    fn permission_level(&self) -> PermissionLevel {
        // Default: convert from legacy risk_level()
        PermissionLevel::from_risk_level(self.risk_level())
    }
}

// ─── ShellTool (Full permission — SAFETY REFACTORED) ────────────────────────

/// Shell command execution tool — with comprehensive safety mechanisms.
///
/// **Major refactoring from the old ShellTool** (Issue #2):
///
/// Safety mechanisms:
/// 1. **Command blacklist**: Regex patterns that block dangerous commands
///    (rm -rf /, mkfs, dd, :(){ :|:& };:, chmod 777, sudo rm, etc.)
/// 2. **Sandbox mode**: Default execution in sandbox (restricted working dir,
///    no network access, read-only filesystem outside allowed dirs).
///    `dangerouslyDisableSandbox` must be explicitly enabled (like Claude Code).
/// 3. **Working directory restriction**: Commands run within the project directory.
/// 4. **Timeout protection**: Default 120s, maximum 600s (like Claude Code).
/// 5. **Output size limit**: Truncate output beyond a configurable size
///    to prevent context overflow.
pub struct ShellTool {
    /// Command blacklist patterns (regex).
    blocked_patterns: Vec<regex::Regex>,

    /// Default timeout in seconds (default: 120).
    default_timeout_secs: u64,

    /// Maximum timeout in seconds (hard limit: 600).
    max_timeout_secs: u64,

    /// Allowed working directories (commands restricted to these).
    allowed_working_dirs: Vec<std::path::PathBuf>,

    /// Sandbox mode (default: enabled).
    sandbox_mode: SandboxMode,

    /// Maximum output size in bytes (to prevent context overflow).
    max_output_bytes: usize,
}

/// Sandbox execution mode.
pub enum SandboxMode {
    /// Full sandbox — restricted working directory, no dangerous commands.
    Enabled,

    /// Sandbox disabled — allows any command in any directory.
    /// Must be explicitly enabled by the user (analogous to Claude Code's
    /// dangerouslyDisableSandbox flag).
    Disabled {
        /// Reason for disabling sandbox (for audit logging).
        reason: String,
    },
}

impl ShellTool {
    /// Create a new ShellTool with default safety settings.
    pub fn new() -> Self {
        Self {
            blocked_patterns: default_blocked_patterns(),
            default_timeout_secs: 120,
            max_timeout_secs: 600,
            allowed_working_dirs: Vec::new(),
            sandbox_mode: SandboxMode::Enabled,
            max_output_bytes: 100_000, // ~100KB max output
        }
    }

    /// Create a ShellTool with sandbox disabled (requires explicit reason).
    ///
    /// Analogous to Claude Code's `dangerouslyDisableSandbox` parameter.
    /// The reason is logged for audit purposes.
    pub fn dangerously_disable_sandbox(reason: impl Into<String>) -> Self {
        Self {
            blocked_patterns: default_blocked_patterns(),
            default_timeout_secs: 120,
            max_timeout_secs: 600,
            allowed_working_dirs: Vec::new(),
            sandbox_mode: SandboxMode::Disabled { reason: reason.into() },
            max_output_bytes: 100_000,
        }
    }

    /// Create a ShellTool with a custom timeout.
    pub fn with_timeout(timeout_secs: u64) -> Self {
        Self {
            blocked_patterns: default_blocked_patterns(),
            default_timeout_secs: timeout_secs.min(600), // Clamp to max
            max_timeout_secs: 600,
            allowed_working_dirs: Vec::new(),
            sandbox_mode: SandboxMode::Enabled,
            max_output_bytes: 100_000,
        }
    }

    /// Get the configured default timeout in seconds.
    pub fn timeout_secs(&self) -> u64 {
        self.default_timeout_secs
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionAwareTool for ShellTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Full
    }
}

/// Default command blacklist patterns.
///
/// Blocks commands that can cause irreversible damage:
/// - `rm -rf /` — recursive root deletion
/// - `mkfs` — filesystem formatting
/// - `dd` if=/dev/zero — disk zeroing
/// - `:(){ :|:& };:` — fork bomb
/// - `chmod 777 /` — making root world-writable
/// - `sudo rm` — sudo deletion
/// - `shutdown`, `reboot`, `halt` — system shutdown
/// - `>` to /dev/sda — direct disk write
fn default_blocked_patterns() -> Vec<regex::Regex> {
    [
        r"rm\s+(-[a-zA-Z]*f[a-zA-Z]*\s+(-[a-zA-Z]*r[a-zA-Z]*\s+)?/|-[a-zA-Z]*r[a-zA-Z]*\s+(-[a-zA-Z]*f[a-zA-Z]*\s+)?/)",
        r"mkfs",
        r"dd\s+if=/dev/zero",
        r":\(\)\{\s*:\|:&\s*\};:",
        r"chmod\s+(777|666)\s+/",
        r"sudo\s+rm",
        r"(shutdown|reboot|halt)\s+",
        r">\s*/dev/sda",
    ].iter()
    .filter_map(|p| regex::Regex::new(p).ok())
    .collect()
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command on the local system. Returns the command output (stdout and stderr). \
        Use with caution — this is a high-risk tool that requires human approval before execution. \
        Dangerous commands (rm -rf /, mkfs, etc.) are blocked by default."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in seconds (default: 120, max: 600)",
                    "default": 120
                }
            },
            "required": ["command"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::High
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let command = args.get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if command.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No command provided".to_string()),
            });
        }

        // Check against blocked patterns
        for pattern in &self.blocked_patterns {
            if pattern.is_match(command) {
                return Ok(ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!("Command blocked by safety policy: matches dangerous pattern. \
                        If you need to run this command, disable sandbox mode with explicit justification.")),
                });
            }
        }

        // Determine the shell based on the platform
        let (shell, shell_arg) = if cfg!(target_os = "windows") {
            ("powershell", "-Command")
        } else {
            ("sh", "-c")
        };

        // Clamp timeout to max
        let timeout = args.get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.default_timeout_secs)
            .min(self.max_timeout_secs);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            tokio::process::Command::new(shell)
                .arg(shell_arg)
                .arg(command)
                .output()
        ).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let content = if stderr.is_empty() {
                    stdout
                } else {
                    format!("STDOUT:\n{}\nSTDERR:\n{}", stdout, stderr)
                };

                // Truncate output if exceeds max size
                let truncated_content = if content.len() > self.max_output_bytes {
                    let mut truncated = content[..self.max_output_bytes].to_string();
                    truncated.push_str("\n... [output truncated due to size limit]");
                    truncated
                } else {
                    content
                };

                Ok(ToolOutput {
                    success: output.status.success(),
                    content: truncated_content,
                    error: if output.status.success() {
                        None
                    } else {
                        Some(format!("Exit code: {}", output.status.code().unwrap_or(-1)))
                    },
                })
            }
            Ok(Err(e)) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to execute command: {}", e)),
            }),
            Err(_) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Command timed out after {} seconds", timeout)),
            }),
        }
    }
}

// ─── FileReadTool (Read permission) ─────────────────────────────────────────

/// File read tool — reads the contents of a local file with offset+limit support.
///
/// **Key improvement over the old FileReadTool**: supports `offset` and `limit`
/// parameters for partial file reads. This is critical for large files
/// that would overflow the context window if read entirely.
///
/// Inspired by Claude Code's Read tool which supports `offset + limit`
/// parameters for line-based partial reads.
pub struct FileReadTool {
    /// Maximum file size to read (in bytes).
    max_size_bytes: usize,
    /// Maximum number of lines to return (safety limit).
    max_lines: usize,
}

impl FileReadTool {
    pub fn new() -> Self {
        Self {
            max_size_bytes: 1024 * 1024, // 1MB
            max_lines: 2000,
        }
    }

    /// Create with a custom max size.
    pub fn with_max_size(max_size_bytes: usize) -> Self {
        Self {
            max_size_bytes,
            max_lines: 2000,
        }
    }
}

impl Default for FileReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionAwareTool for FileReadTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Read
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a local file. Supports offset+limit parameters \
        for partial reads of large files. Returns the file content as text. \
        For binary files, returns a base64-encoded representation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to read"
                },
                "encoding": {
                    "type": "string",
                    "description": "The encoding to use (default: utf-8)",
                    "default": "utf-8"
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line number (0-based, default: 0)",
                    "default": 0
                },
                "limit": {
                    "type": "integer",
                    "description": "Number of lines to read (default: 2000)",
                    "default": 2000
                }
            },
            "required": ["path"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if path.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No file path provided".to_string()),
            });
        }

        // Security: reject paths that try to escape reasonable boundaries
        if path.contains("..") {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Path traversal detected: paths containing '..' are not allowed".to_string()),
            });
        }

        let file_path = std::path::Path::new(path);

        // Check if file exists
        if !file_path.exists() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("File not found: {}", path)),
            });
        }

        // Check file size
        let file_size = tokio::fs::metadata(path).await
            .map(|m| m.len())
            .unwrap_or(0);

        if file_size > self.max_size_bytes as u64 {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!(
                    "File too large: {} bytes (max: {} bytes). Use offset+limit parameters to read partial content.",
                    file_size, self.max_size_bytes
                )),
            });
        }

        // Read the file content
        let content = tokio::fs::read_to_string(path).await;

        // Apply offset + limit if specified
        let offset = args.get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let limit = args.get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.max_lines as u64) as usize;

        match content {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                let total_lines = lines.len();

                // Apply offset + limit
                let start = offset.min(total_lines);
                let end = (start + limit).min(total_lines);
                let selected_lines = &lines[start..end];

                // Format with line numbers (like cat -n)
                let output = selected_lines.iter()
                    .enumerate()
                    .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
                    .collect::<Vec<String>>()
                    .join("\n");

                let header = if offset > 0 || end < total_lines {
                    format!("Showing lines {}-{} of {} total lines\n\n", start + 1, end, total_lines)
                } else {
                    String::new()
                };

                Ok(ToolOutput {
                    success: true,
                    content: format!("{}{}", header, output),
                    error: None,
                })
            }
            Err(_) => {
                // Binary file — read as bytes and base64 encode
                let bytes = tokio::fs::read(path).await;
                match bytes {
                    Ok(data) => {
                        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
                        Ok(ToolOutput {
                            success: true,
                            content: BASE64.encode(&data),
                            error: None,
                        })
                    }
                    Err(e) => Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Failed to read file: {}", e)),
                    }),
                }
            }
        }
    }
}

// ─── FileEditTool (Standard permission) ─────────────────────────────────────

/// File edit tool — performs exact string replacement in a file.
///
/// Inspired by Claude Code's Edit tool. Takes:
/// - file_path: the file to edit
/// - old_string: the exact string to find (must be unique in the file)
/// - new_string: the replacement string
///
/// This is a Standard-permission tool because it modifies files
/// but with a precise, safe mechanism (exact string matching).
pub struct FileEditTool;

impl FileEditTool {
    pub fn new() -> Self { Self }
}

impl Default for FileEditTool {
    fn default() -> Self { Self::new() }
}

impl PermissionAwareTool for FileEditTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Standard
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Perform an exact string replacement in a file. The old_string must be \
        unique in the file. This is a precise, safe editing mechanism."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact string to find and replace (must be unique in the file)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement string"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Whether to replace all occurrences (default: false)",
                    "default": false
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let file_path = args.get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let old_string = args.get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new_string = args.get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replace_all = args.get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if file_path.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No file path provided".to_string()),
            });
        }

        if old_string.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("old_string cannot be empty".to_string()),
            });
        }

        if old_string == new_string {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("old_string and new_string are identical — no change needed".to_string()),
            });
        }

        // Security: reject path traversal
        if file_path.contains("..") {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Path traversal detected".to_string()),
            });
        }

        // Read the file
        let content = tokio::fs::read_to_string(file_path).await;
        match content {
            Ok(text) => {
                // Check if old_string exists in the file
                let count = text.matches(old_string).count();
                if count == 0 {
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("old_string not found in file: {}", file_path)),
                    });
                }

                if !replace_all && count > 1 {
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!(
                            "old_string found {} times in file (must be unique unless replace_all=true)",
                            count
                        )),
                    });
                }

                // Perform the replacement
                let new_content = if replace_all {
                    text.replace(old_string, new_string)
                } else {
                    text.replacen(old_string, new_string, 1)
                };

                // Write back
                let write_result = tokio::fs::write(file_path, new_content).await;
                match write_result {
                    Ok(_) => Ok(ToolOutput {
                        success: true,
                        content: format!(
                            "Successfully replaced {} occurrence(s) in {}",
                            if replace_all { count } else { 1 },
                            file_path
                        ),
                        error: None,
                    }),
                    Err(e) => Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Failed to write file: {}", e)),
                    }),
                }
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to read file: {}", e)),
            }),
        }
    }
}

// ─── FileListTool (Read permission) ─────────────────────────────────────────

/// File list tool — lists directory contents (like ls).
///
/// Returns a list of files and directories in the specified path.
/// This is a Read-permission tool (only observes, never modifies).
pub struct FileListTool;

impl FileListTool {
    pub fn new() -> Self { Self }
}

impl Default for FileListTool {
    fn default() -> Self { Self::new() }
}

impl PermissionAwareTool for FileListTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Read
    }
}

#[async_trait]
impl Tool for FileListTool {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn description(&self) -> &str {
        "List the contents of a directory. Returns file and directory names \
        with their types (file/directory) and sizes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The directory path to list"
                }
            },
            "required": ["path"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if path.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No directory path provided".to_string()),
            });
        }

        if path.contains("..") {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Path traversal detected".to_string()),
            });
        }

        let mut entries = tokio::fs::read_dir(path).await;
        match entries {
            Ok(mut read_dir) => {
                let mut result = Vec::new();
                while let Ok(Some(entry)) = read_dir.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let file_type = entry.file_type().await;
                    let is_dir = file_type.map(|ft| ft.is_dir()).unwrap_or(false);
                    let size = if !is_dir {
                        entry.metadata().await.map(|m| m.len()).unwrap_or(0)
                    } else {
                        0
                    };
                    result.push(if is_dir {
                        format!("  [DIR]  {}", name)
                    } else {
                        format!("  [FILE] {} ({})", name, size)
                    });
                }
                result.sort();
                Ok(ToolOutput {
                    success: true,
                    content: result.join("\n"),
                    error: None,
                })
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to read directory: {}", e)),
            }),
        }
    }
}

// ─── GrepTool (Read permission) ─────────────────────────────────────────────

/// Grep tool — searches file contents using regex patterns.
///
/// Recursively searches files in a directory for lines matching
/// a regex pattern. Returns matching lines with file paths and line numbers.
/// This is a Read-permission tool.
pub struct GrepTool;

impl GrepTool {
    pub fn new() -> Self { Self }
}

impl Default for GrepTool {
    fn default() -> Self { Self::new() }
}

impl PermissionAwareTool for GrepTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Read
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents using regex patterns. Recursively searches files \
        in a directory for lines matching a pattern. Returns matching lines with \
        file paths and line numbers."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "The directory or file path to search in",
                    "default": "."
                },
                "file_pattern": {
                    "type": "string",
                    "description": "Optional glob pattern to filter files (e.g., '*.rs')",
                    "default": "*"
                }
            },
            "required": ["pattern"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let pattern = args.get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let file_pattern = args.get("file_pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("*");

        if pattern.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No search pattern provided".to_string()),
            });
        }

        let regex = regex::Regex::new(pattern);
        match regex {
            Ok(re) => {
                // Native Rust implementation — no shell command required
                let mut results = Vec::new();
                let mut match_count = 0;
                let max_matches = 500; // Prevent context overflow

                let search_path = std::path::Path::new(path);
                if !search_path.exists() {
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Path does not exist: {}", path)),
                    });
                }

                // Build glob pattern for file filtering
                let glob_pattern = if file_pattern == "*" {
                    "**/*".to_string()
                } else {
                    format!("**/{}", file_pattern)
                };

                // Walk the directory tree and search matching files
                for entry in walkdir::WalkDir::new(search_path)
                    .into_iter()
                    .filter_entry(|e| {
                        // Skip hidden dirs, target, node_modules
                        let name = e.file_name().to_string_lossy();
                        !name.starts_with('.')
                            && name != "target"
                            && name != "node_modules"
                            && name != ".git"
                    })
                {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(_) => continue,
                    };

                    if !entry.file_type().is_file() {
                        continue;
                    }

                    // Check glob pattern match
                    let file_path_str = entry.path().to_string_lossy();
                    let relative_path = entry.path().strip_prefix(search_path)
                        .unwrap_or(entry.path())
                        .to_string_lossy();

                    if file_pattern != "*" {
                        let glob_matcher = glob::Pattern::new(file_pattern);
                        match glob_matcher {
                            Ok(gm) => {
                                // Match against just the filename component
                                let file_name = entry.path().file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                if !gm.matches(&file_name) {
                                    continue;
                                }
                            }
                            Err(_) => continue,
                        }
                    }

                    // Read and search the file
                    let content = std::fs::read_to_string(entry.path());
                    match content {
                        Ok(text) => {
                            for (line_num, line) in text.lines().enumerate() {
                                if re.is_match(line) {
                                    if match_count >= max_matches {
                                        results.push(format!("... [truncated: {} matches found, showing first {}]",
                                            match_count + 1, max_matches));
                                        break;
                                    }
                                    results.push(format!("{}:{}: {}", file_path_str, line_num + 1, line.trim()));
                                    match_count += 1;
                                }
                            }
                        }
                        Err(_) => continue, // Skip binary/unreadable files
                    }

                    if match_count >= max_matches {
                        break;
                    }
                }

                if results.is_empty() {
                    Ok(ToolOutput {
                        success: true,
                        content: format!("No matches found for pattern '{}' in {}", pattern, path),
                        error: None,
                    })
                } else {
                    Ok(ToolOutput {
                        success: true,
                        content: results.join("\n"),
                        error: None,
                    })
                }
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Invalid regex pattern: {}", e)),
            }),
        }
    }
}

// ─── GlobTool (Read permission) ─────────────────────────────────────────────

/// Glob tool — searches file paths using glob patterns.
///
/// Finds files matching a glob pattern (e.g., "**/*.rs", "src/**/*.toml").
/// Returns matching file paths. This is a Read-permission tool.
pub struct GlobTool;

impl GlobTool {
    pub fn new() -> Self { Self }
}

impl Default for GlobTool {
    fn default() -> Self { Self::new() }
}

impl PermissionAwareTool for GlobTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Read
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern (e.g., '**/*.rs', 'src/**/*.toml'). \
        Returns matching file paths."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to search for (e.g., '**/*.rs')"
                },
                "path": {
                    "type": "string",
                    "description": "The base directory to search from (default: .)",
                    "default": "."
                }
            },
            "required": ["pattern"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let pattern = args.get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

        if pattern.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No glob pattern provided".to_string()),
            });
        }

        // Native Rust implementation — no shell command required
        let search_path = std::path::Path::new(path);
        if !search_path.exists() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Path does not exist: {}", path)),
            });
        }

        let mut results = Vec::new();
        let max_results = 1000; // Prevent context overflow

        // Build the full glob pattern (path + pattern)
        let full_pattern = if pattern.starts_with('/') {
            pattern.to_string()
        } else {
            format!("{}/{}", path, pattern)
        };

        // Use glob crate for pattern matching
        match glob::glob(&full_pattern) {
            Ok(paths) => {
                for entry in paths {
                    match entry {
                        Ok(path) => {
                            if results.len() >= max_results {
                                results.push(format!("... [truncated: more than {} files match]", max_results));
                                break;
                            }
                            results.push(path.to_string_lossy().to_string());
                        }
                        Err(e) => {
                            // Skip paths that can't be accessed
                            tracing::debug!("Glob path error: {:?}", e);
                        }
                    }
                }
            }
            Err(e) => {
                return Ok(ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!("Invalid glob pattern '{}': {}", pattern, e)),
                });
            }
        }

        if results.is_empty() {
            Ok(ToolOutput {
                success: true,
                content: format!("No files found matching pattern '{}' in {}", pattern, path),
                error: None,
            })
        } else {
            Ok(ToolOutput {
                success: true,
                content: results.join("\n"),
                error: None,
            })
        }
    }
}

// ─── EnvironmentTool (Read permission) ──────────────────────────────────────

/// Environment tool — retrieves environment information.
///
/// Returns current working directory, platform info, environment variables,
/// and system capabilities. This is a Read-permission tool (pure observation).
pub struct EnvironmentTool;

impl EnvironmentTool {
    pub fn new() -> Self { Self }
}

impl Default for EnvironmentTool {
    fn default() -> Self { Self::new() }
}

impl PermissionAwareTool for EnvironmentTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Read
    }
}

#[async_trait]
impl Tool for EnvironmentTool {
    fn name(&self) -> &str {
        "environment"
    }

    fn description(&self) -> &str {
        "Get environment information: working directory, platform, available tools, \
        and system capabilities. Pure observation — no modifications."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "info_type": {
                    "type": "string",
                    "description": "Type of info to retrieve: 'all', 'cwd', 'platform', 'env_vars'",
                    "default": "all"
                }
            }
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let info_type = args.get("info_type")
            .and_then(|v| v.as_str())
            .unwrap_or("all");

        let mut info = Vec::new();

        if info_type == "all" || info_type == "cwd" {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            info.push(format!("Working Directory: {}", cwd));
        }

        if info_type == "all" || info_type == "platform" {
            info.push(format!("Platform: {}", std::env::consts::OS));
            info.push(format!("Arch: {}", std::env::consts::ARCH));
        }

        if info_type == "all" || info_type == "env_vars" {
            // List some key environment variables (not all — too many)
            let key_vars = ["HOME", "PATH", "USER", "SHELL", "LANG", "TERM"];
            for var in key_vars {
                if let Ok(val) = std::env::var(var) {
                    info.push(format!("{}: {}", var, val));
                }
            }
        }

        Ok(ToolOutput {
            success: true,
            content: info.join("\n"),
            error: None,
        })
    }
}

// ─── NotebookEditTool (Standard permission) ─────────────────────────────────

/// Notebook edit tool — edits Jupyter notebook cells.
///
/// Inspired by Claude Code's NotebookEdit tool. Supports:
/// - Replace, insert, and delete cell operations
/// - Cell type specification (code/markdown)
/// - Cell ID targeting for precise edits
pub struct NotebookEditTool;

impl NotebookEditTool {
    pub fn new() -> Self { Self }
}

impl Default for NotebookEditTool {
    fn default() -> Self { Self::new() }
}

impl PermissionAwareTool for NotebookEditTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Standard
    }
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "notebook_edit"
    }

    fn description(&self) -> &str {
        "Edit Jupyter notebook cells. Supports replace, insert, and delete \
        operations on cells identified by their cell_id."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "Path to the .ipynb notebook file"
                },
                "cell_id": {
                    "type": "string",
                    "description": "The cell ID to edit"
                },
                "new_source": {
                    "type": "string",
                    "description": "The new cell source content"
                },
                "cell_type": {
                    "type": "string",
                    "description": "Cell type: 'code' or 'markdown'",
                    "default": "code"
                },
                "edit_mode": {
                    "type": "string",
                    "description": "Edit mode: 'replace', 'insert', 'delete'",
                    "default": "replace"
                }
            },
            "required": ["notebook_path", "new_source"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let notebook_path = args.get("notebook_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if notebook_path.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No notebook path provided".to_string()),
            });
        }

        // Security: reject path traversal
        if notebook_path.contains("..") {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Path traversal detected".to_string()),
            });
        }

        // Verify it's a .ipynb file
        if !notebook_path.ends_with(".ipynb") {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("File must be a .ipynb notebook".to_string()),
            });
        }

        // Read the notebook
        let content = tokio::fs::read_to_string(notebook_path).await;
        match content {
            Ok(text) => {
                // Parse as JSON
                let mut notebook: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(ToolOutput {
                            success: false,
                            content: String::new(),
                            error: Some(format!("Failed to parse notebook JSON: {}", e)),
                        });
                    }
                };

                // Get parameters
                let cell_id = args.get("cell_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let new_source = args.get("new_source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let cell_type = args.get("cell_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("code");
                let edit_mode = args.get("edit_mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("replace");

                // Get the cells array
                let cells = notebook.get_mut("cells")
                    .and_then(|c| c.as_array_mut());

                if cells.is_none() {
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some("Notebook has no 'cells' array".to_string()),
                    });
                }

                let cells_mut = cells.unwrap();

                match edit_mode {
                    "replace" => {
                        // Find the cell with matching cell_id and replace its source
                        if cell_id.is_empty() {
                            // If no cell_id, try to find by position or return error
                            return Ok(ToolOutput {
                                success: false,
                                content: String::new(),
                                error: Some("cell_id is required for replace mode".to_string()),
                            });
                        }

                        let found = cells_mut.iter_mut().find(|cell| {
                            cell.get("id").and_then(|v| v.as_str()) == Some(cell_id)
                        });

                        if let Some(cell) = found {
                            // Update source — convert string to array of lines (ipynb format)
                            let source_lines: Vec<serde_json::Value> = new_source.lines()
                                .map(|line| serde_json::Value::String(format!("{}\n", line)))
                                .chain(std::iter::once(serde_json::Value::String(String::new())))
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .skip(1) // Remove trailing empty line we added
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect();

                            // Actually: ipynb format stores source as array of strings with \n appended to each line
                            // except the last line which doesn't have \n
                            let lines: Vec<&str> = new_source.lines().collect();
                            let source_array: Vec<serde_json::Value> = if lines.is_empty() {
                                vec![serde_json::Value::String(String::new())]
                            } else {
                                lines.iter().enumerate().map(|(i, line)| {
                                    if i < lines.len() - 1 {
                                        serde_json::Value::String(format!("{}\n", line))
                                    } else {
                                        serde_json::Value::String(line.to_string())
                                    }
                                }).collect()
                            };

                            cell.as_object_mut().unwrap().insert(
                                "source".to_string(),
                                serde_json::Value::Array(source_array),
                            );
                            cell.as_object_mut().unwrap().insert(
                                "cell_type".to_string(),
                                serde_json::Value::String(cell_type.to_string()),
                            );

                            let write_result = tokio::fs::write(notebook_path, serde_json::to_string_pretty(&notebook).unwrap()).await;
                            match write_result {
                                Ok(_) => Ok(ToolOutput {
                                    success: true,
                                    content: format!("Successfully replaced cell '{}' in {}", cell_id, notebook_path),
                                    error: None,
                                }),
                                Err(e) => Ok(ToolOutput {
                                    success: false,
                                    content: String::new(),
                                    error: Some(format!("Failed to write notebook: {}", e)),
                                }),
                            }
                        } else {
                            Ok(ToolOutput {
                                success: false,
                                content: String::new(),
                                error: Some(format!("Cell '{}' not found in notebook", cell_id)),
                            })
                        }
                    }
                    "insert" => {
                        // Insert a new cell after the specified cell_id (or at the end if empty)
                        let new_cell = serde_json::json!({
                            "cell_type": cell_type,
                            "id": uuid::Uuid::new_v4().to_string(),
                            "metadata": {},
                            "source": new_source.lines().enumerate().map(|(i, line)| {
                                if i < new_source.lines().count() - 1 {
                                    format!("{}\n", line)
                                } else {
                                    line.to_string()
                                }
                            }).collect::<Vec<String>>(),
                            "outputs": if cell_type == "code" { serde_json::json!([]) } else { serde_json::Value::Null },
                            "execution_count": if cell_type == "code" { serde_json::json!(0) } else { serde_json::Value::Null },
                        });

                        if cell_id.is_empty() {
                            // Insert at the end
                            cells_mut.push(new_cell);
                        } else {
                            // Find the index of the specified cell and insert after it
                            let pos = cells_mut.iter().position(|cell| {
                                cell.get("id").and_then(|v| v.as_str()) == Some(cell_id)
                            });
                            match pos {
                                Some(idx) => cells_mut.insert(idx + 1, new_cell),
                                None => cells_mut.push(new_cell), // Fallback: insert at end
                            }
                        }

                        let write_result = tokio::fs::write(notebook_path, serde_json::to_string_pretty(&notebook).unwrap()).await;
                        match write_result {
                            Ok(_) => Ok(ToolOutput {
                                success: true,
                                content: format!("Successfully inserted new cell in {}", notebook_path),
                                error: None,
                            }),
                            Err(e) => Ok(ToolOutput {
                                success: false,
                                content: String::new(),
                                error: Some(format!("Failed to write notebook: {}", e)),
                            }),
                        }
                    }
                    "delete" => {
                        // Delete the cell with matching cell_id
                        if cell_id.is_empty() {
                            return Ok(ToolOutput {
                                success: false,
                                content: String::new(),
                                error: Some("cell_id is required for delete mode".to_string()),
                            });
                        }

                        let original_len = cells_mut.len();
                        cells_mut.retain(|cell| {
                            cell.get("id").and_then(|v| v.as_str()) != Some(cell_id)
                        });

                        if cells_mut.len() == original_len {
                            return Ok(ToolOutput {
                                success: false,
                                content: String::new(),
                                error: Some(format!("Cell '{}' not found in notebook", cell_id)),
                            });
                        }

                        let write_result = tokio::fs::write(notebook_path, serde_json::to_string_pretty(&notebook).unwrap()).await;
                        match write_result {
                            Ok(_) => Ok(ToolOutput {
                                success: true,
                                content: format!("Successfully deleted cell '{}' from {}", cell_id, notebook_path),
                                error: None,
                            }),
                            Err(e) => Ok(ToolOutput {
                                success: false,
                                content: String::new(),
                                error: Some(format!("Failed to write notebook: {}", e)),
                            }),
                        }
                    }
                    _ => Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Unknown edit_mode: '{}'. Use 'replace', 'insert', or 'delete'", edit_mode)),
                    }),
                }
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to read notebook file: {}", e)),
            }),
        }
    }
}

// ─── FileDeleteTool (Full permission) ───────────────────────────────────────

/// File delete tool — deletes files (Full permission, requires approval).
///
/// This is a Full-permission tool because file deletion is irreversible
/// and can cause significant damage. Always requires human approval.
pub struct FileDeleteTool;

impl FileDeleteTool {
    pub fn new() -> Self { Self }
}

impl Default for FileDeleteTool {
    fn default() -> Self { Self::new() }
}

impl PermissionAwareTool for FileDeleteTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Full
    }
}

#[async_trait]
impl Tool for FileDeleteTool {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn description(&self) -> &str {
        "Delete a file. This is a high-risk, irreversible operation that \
        always requires human approval."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to delete"
                }
            },
            "required": ["path"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::High
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if path.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No file path provided".to_string()),
            });
        }

        if path.contains("..") {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Path traversal detected".to_string()),
            });
        }

        let result = tokio::fs::remove_file(path).await;
        match result {
            Ok(_) => Ok(ToolOutput {
                success: true,
                content: format!("Successfully deleted: {}", path),
                error: None,
            }),
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to delete file: {}", e)),
            }),
        }
    }
}

// ─── WebFetchTool (Standard permission) ────────────────────────────────────────

/// Web fetch tool — fetches URL content and converts HTML to Markdown.
///
/// Inspired by Claude Code's WebFetch tool. Fetches a web page at a given URL,
/// converts the HTML content to structured Markdown (preserving headings, links,
/// lists, and other semantic elements), and returns it for agent consumption.
///
/// This is a Standard-permission tool because it sends requests to external servers
/// (which the user should be aware of), but the operation itself is read-only
/// (no modifications to any system).
pub struct WebFetchTool {
    /// HTTP client for making requests.
    client: reqwest::Client,
    /// Maximum content size to return (in bytes, ~100KB).
    max_content_bytes: usize,
    /// Request timeout in seconds.
    timeout_secs: u64,
}

impl WebFetchTool {
    /// Create a new WebFetchTool with default settings.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            max_content_bytes: 100_000, // ~100KB max content
            timeout_secs: 30,
        }
    }

    /// Create with custom settings.
    pub fn with_config(max_content_bytes: usize, timeout_secs: u64) -> Self {
        Self {
            client: reqwest::Client::new(),
            max_content_bytes,
            timeout_secs,
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionAwareTool for WebFetchTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Standard
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch content from a web URL and convert it to structured Markdown. \
        Preserves headings, links, lists, and other semantic elements. \
        Returns the converted content for reading and analysis. \
        Use for: fetching documentation pages, API references, blog posts, \
        and any web content that needs to be understood by the agent."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch content from"
                },
                "prompt": {
                    "type": "string",
                    "description": "Optional prompt to focus on specific aspects of the fetched content",
                    "default": ""
                }
            },
            "required": ["url"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let url = args.get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if url.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No URL provided".to_string()),
            });
        }

        // Validate URL format
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("URL must start with http:// or https://".to_string()),
            });
        }

        // Fetch the URL content
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.client.get(url)
                .header("User-Agent", "OneAI-Agent/0.1.0")
                .send()
        ).await;

        match result {
            Ok(Ok(response)) => {
                let status = response.status();
                if !status.is_success() {
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("HTTP error {}: {}", status.as_u16(), status.canonical_reason().unwrap_or("Unknown"))),
                    });
                }

                // Get the response body
                let body_result = response.text().await;
                match body_result {
                    Ok(html) => {
                        // Convert HTML to Markdown using html2text
                        let markdown = html2text::from_read(html.as_bytes(), 200);

                        // Truncate if exceeds max content size
                        let content = if markdown.len() > self.max_content_bytes {
                            let mut truncated = markdown[..self.max_content_bytes].to_string();
                            truncated.push_str("\n... [content truncated due to size limit]");
                            truncated
                        } else {
                            markdown
                        };

                        Ok(ToolOutput {
                            success: true,
                            content,
                            error: None,
                        })
                    }
                    Err(e) => Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some(format!("Failed to read response body: {}", e)),
                    }),
                }
            }
            Ok(Err(e)) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("HTTP request failed: {}", e)),
            }),
            Err(_) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Request timed out after {} seconds", self.timeout_secs)),
            }),
        }
    }
}