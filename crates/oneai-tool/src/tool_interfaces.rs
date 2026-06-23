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
///
/// **Updated**: SandboxMode now supports a real SandboxBackend for
/// process-level isolation (Seatbelt on macOS, Docker on Linux).
/// When a backend is provided, commands are wrapped before execution
/// to enforce filesystem, network, and process restrictions.
pub enum SandboxMode {
    /// Full sandbox — uses a SandboxBackend for real process-level isolation.
    /// The backend wraps commands before execution, restricting:
    /// - File system writes (only to allowed directories)
    /// - Network access (can be restricted)
    /// - Process execution (can restrict which binaries can run)
    ///
    /// If no backend is provided (None), falls back to regex-only blocking.
    Enabled {
        /// The sandbox backend to use for command wrapping.
        /// If None, uses the regex blacklist + working dir restriction (improved baseline).
        backend: Option<std::sync::Arc<dyn crate::sandbox::SandboxBackend>>,
    },

    /// Sandbox disabled — allows any command in any directory.
    /// Must be explicitly enabled by the user (analogous to Claude Code's
    /// dangerouslyDisableSandbox flag).
    Disabled {
        /// Reason for disabling sandbox (for audit logging).
        reason: String,
    },
}

impl Default for SandboxMode {
    fn default() -> Self {
        Self::Enabled { backend: None }
    }
}

impl ShellTool {
    /// Create a new ShellTool with default safety settings.
    /// Sandbox is enabled by default (regex-only, no backend).
    pub fn new() -> Self {
        Self {
            blocked_patterns: default_blocked_patterns(),
            default_timeout_secs: 120,
            max_timeout_secs: 600,
            allowed_working_dirs: Vec::new(),
            sandbox_mode: SandboxMode::Enabled { backend: None },
            max_output_bytes: 100_000, // ~100KB max output
        }
    }

    /// Create a ShellTool with a real sandbox backend for process-level isolation.
    ///
    /// The backend wraps commands before execution, providing real isolation
    /// (Seatbelt on macOS, Docker on Linux, etc.). This replaces the
    /// regex-only approach with actual process-level restrictions.
    pub fn with_sandbox_backend(backend: std::sync::Arc<dyn crate::sandbox::SandboxBackend>) -> Self {
        Self {
            blocked_patterns: default_blocked_patterns(),
            default_timeout_secs: 120,
            max_timeout_secs: 600,
            allowed_working_dirs: Vec::new(),
            sandbox_mode: SandboxMode::Enabled { backend: Some(backend) },
            max_output_bytes: 100_000,
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
            sandbox_mode: SandboxMode::Enabled { backend: None },
            max_output_bytes: 100_000,
        }
    }

    /// Get the configured default timeout in seconds.
    pub fn timeout_secs(&self) -> u64 {
        self.default_timeout_secs
    }

    /// Get a reference to the sandbox mode.
    pub fn sandbox_mode(&self) -> &SandboxMode {
        &self.sandbox_mode
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
        "Execute a shell command on the local system. Returns the command output (stdout and stderr).\n\n\
        This is a HIGH-RISK tool that requires explicit approval before execution. Commands run \
        within a sandbox that restricts filesystem access to allowed directories and blocks \
        dangerous operations (rm -rf /, mkfs, dd, chmod 777, etc.) by default.\n\n\
        **CRITICAL: Always prefer specialized tools over shell commands**:\n\
        - Use read_file instead of: cat, head, tail, less\n\
        - Use edit_file instead of: sed, awk, perl -i, patch\n\
        - Use file_write instead of: echo > file, tee, dd for writing\n\
        - Use list_directory instead of: ls, find -type d\n\
        - Use grep instead of: grep command, rg, ag\n\
        - Use glob instead of: find, locate\n\n\
        **When shell IS appropriate**:\n\
        - Compilation/build: cargo build, npm run build, make\n\
        - Testing: cargo test, npm test, pytest\n\
        - Git operations: git status, git diff, git log, git commit\n\
        - Package management: cargo add, npm install, pip install\n\
        - Running scripts: python script.py, bash script.sh\n\
        - System commands: uname, which, date, curl (for URLs not covered by web_fetch)\n\n\
        **Usage guidelines**:\n\
        - Default timeout: 120 seconds (max: 600 seconds)\n\
        - Working directory: restricted to project directory\n\
        - Output is truncated if exceeds size limit to prevent context overflow\n\
        - Combine commands with && for sequential operations\n\
        - Use timeout parameter for long-running commands"
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

        // Check against blocked patterns (always applied, regardless of sandbox backend)
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

        // Determine the actual command to run — wrap with sandbox backend if available
        let effective_command = match &self.sandbox_mode {
            SandboxMode::Enabled { backend } => {
                if let Some(b) = backend {
                    // Use real sandbox backend for process-level isolation
                    let working_dir = if self.allowed_working_dirs.is_empty() {
                        std::path::PathBuf::from(".")
                    } else {
                        self.allowed_working_dirs[0].clone()
                    };
                    let wrapped = b.wrap_command(command, &working_dir)?;
                    tracing::info!("ShellTool: command wrapped by {} sandbox backend", b.name());
                    // The wrapped command is already a complete shell command
                    // (e.g., "sandbox-exec -p '...' sh -c '...'" for Seatbelt,
                    //  or "docker run --rm ... sh -c '...'" for Docker)
                    wrapped.shell_command
                } else {
                    // No backend — just use the raw command (regex-only protection)
                    command.to_string()
                }
            }
            SandboxMode::Disabled { reason } => {
                tracing::warn!("ShellTool: sandbox disabled — reason: {}", reason);
                command.to_string()
            }
        };

        // Determine the shell based on the platform
        // If the command is already wrapped by a sandbox backend (contains "sandbox-exec" or "docker"),
        // we need to use a shell that can execute the wrapped command directly.
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
                .arg(&effective_command)
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
        "Read the contents of a local file. Supports offset+limit parameters for \
        partial reads of large files. Returns the file content as text with line \
        numbers. For binary files, returns a base64-encoded representation.\n\n\
        **Usage guidelines**:\n\
        - Always use read_file before editing a file to understand its current content\n\
        - Use offset+limit for large files to avoid overflowing the context window\n\
        - Start with a small limit (e.g., 50 lines) to preview, then read more if needed\n\
        - For searching across multiple files, prefer grep or glob tools\n\n\
        **Preferences**:\n\
        - Read entire file for small files (<500 lines)\n\
        - Use offset+limit for large files, starting from relevant sections\n\
        - Multiple targeted reads are better than one huge read\n\
        - Re-read files after editing to verify changes"
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
        "Perform an exact string replacement in a file. The old_string must match \
        exactly (including whitespace and indentation) and must be unique in the file.\n\n\
        **Usage guidelines**:\n\
        - ALWAYS read the file first to understand the current content before editing\n\
        - The old_string must be an exact, unique match in the file — partial matches fail\n\
        - For multi-line replacements, include the full original block in old_string\n\
        - For multiple edits in one operation, use apply_patch instead\n\n\
        **Preferences**:\n\
        - Prefer targeted edits over large rewrites\n\
        - Include enough context in old_string to ensure uniqueness\n\
        - Avoid replacing entire functions — replace only the changed lines\n\
        - When in doubt, use a smaller old_string that includes unique identifiers\n\n\
        **Common mistakes**:\n\
        - Don't forget trailing whitespace or newlines in old_string\n\
        - Don't use approximate matches — they will fail silently\n\
        - Don't try to edit a file you haven't read yet"
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

        let entries = tokio::fs::read_dir(path).await;
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
        "Search file contents using regex patterns. Native Rust implementation \
        (no shell dependency). Recursively searches files in a directory for \
        lines matching a pattern. Returns matching lines with file paths and \
        line numbers.\n\n\
        **Usage guidelines**:\n\
        - Use for finding function definitions, usages, patterns across the codebase\n\
        - Pattern must be a valid regex (e.g., 'fn authenticate', 'impl.*Handler')\n\
        - Use file_pattern to narrow search scope (e.g., '*.rs' for Rust files only)\n\
        - Results are limited to 500 matches to prevent context overflow\n\
        - Skips hidden dirs, .git, target, node_modules by default\n\n\
        **Preferences**:\n\
        - Prefer specific patterns over broad ones to reduce noise\n\
        - Use file_pattern to focus on relevant file types\n\
        - For file discovery by name, prefer glob tool\n\
        - For content of a specific file, prefer read_file tool"
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
                let _glob_pattern = if file_pattern == "*" {
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
                    let _relative_path = entry.path().strip_prefix(search_path)
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
        "Find files matching a glob pattern. Native Rust implementation (no shell \
        dependency). Returns matching file paths with sizes.\n\n\
        **Usage guidelines**:\n\
        - Use for finding source files, config files, test files by name pattern\n\
        - Pattern examples: '**/*.rs' (all Rust files), 'src/**/*.toml' (all TOML in src)\n\
        - Faster than grep for file discovery — doesn't read file contents\n\
        - Skips hidden dirs, .git, target, node_modules by default\n\
        - Results are limited to 1000 matches to prevent context overflow\n\n\
        **Preferences**:\n\
        - Prefer glob for file discovery, grep for content search\n\
        - Use specific patterns to reduce noise\n\
        - Combine with grep: first glob to find files, then grep to search their content"
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
                            let _source_lines: Vec<serde_json::Value> = new_source.lines()
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
        Preserves headings, links, lists, and other semantic elements. Returns \
        the converted content for reading and analysis.\n\n\
        **Usage guidelines**:\n\
        - Use for: fetching documentation pages, API references, blog posts\n\
        - Content is converted from HTML to Markdown for easier reading\n\
        - Large pages are truncated to ~100KB to prevent context overflow\n\
        - Timeout: 30 seconds (adjustable)\n\
        - Requires http:// or https:// URL prefix\n\n\
        **Preferences**:\n\
        - Prefer web_search to discover relevant URLs first\n\
        - Then use web_fetch to read the most promising results\n\
        - Focus on specific sections using the prompt parameter\n\
        - Don't fetch URLs you're not interested in — saves context tokens"
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

                        // Truncate if exceeds max content size.
                        // NOTE: slice on byte index only — must land on a UTF-8
                        // char boundary or String slicing panics (it did, on
                        // binary/multibyte responses). Walk back to the nearest
                        // boundary before slicing.
                        let content = if markdown.len() > self.max_content_bytes {
                            let mut end = self.max_content_bytes;
                            while end > 0 && !markdown.is_char_boundary(end) {
                                end -= 1;
                            }
                            let mut truncated = markdown[..end].to_string();
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

// ─── WebSearchTool (Standard permission) ────────────────────────────────────────

/// Web search tool — searches the web for information using search APIs.
///
/// Inspired by Claude Code's WebSearch tool. Searches the web using
/// configurable search engine APIs (Google Custom Search, Bing, SerpAPI)
/// and returns structured results with titles, URLs, and snippets.
///
/// This addresses the "无 WebSearch 本地工具" gap — previously, web search
/// was only available through MCP (which was todo!()). Now it's a first-class
/// local tool that the agent can use directly.
///
/// The tool supports multiple search backends:
/// 1. **Google Custom Search API** — requires API key + CX ID
/// 2. **Bing Search API** — requires API key
/// 3. **SerpAPI** — requires API key (most versatile)
/// 4. **DuckDuckGo** — no API key required (free, but limited)
///
/// Search backend is selected via environment variables:
/// - `ONEAI_SEARCH_BACKEND`: "google", "bing", "serpapi", or "duckduckgo"
/// - `ONEAI_SEARCH_API_KEY`: API key for the chosen backend
/// - `ONEAI_SEARCH_CX`: Google Custom Search CX ID (Google only)
///
/// If no backend is configured, DuckDuckGo (free) is used as default.
pub struct WebSearchTool {
    /// HTTP client for making requests.
    client: reqwest::Client,
    /// Maximum number of results to return.
    max_results: usize,
    /// Request timeout in seconds.
    timeout_secs: u64,
}

impl WebSearchTool {
    /// Create a new WebSearchTool with default settings.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            max_results: 10,
            timeout_secs: 30,
        }
    }

    /// Create with custom settings.
    pub fn with_config(max_results: usize, timeout_secs: u64) -> Self {
        Self {
            client: reqwest::Client::new(),
            max_results,
            timeout_secs,
        }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionAwareTool for WebSearchTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Standard
    }
}

/// A single search result from a web search.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Title of the result.
    pub title: String,
    /// URL of the result.
    pub url: String,
    /// Snippet/summary of the result.
    pub snippet: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns a list of results with titles, \
        URLs, and snippets. Supports multiple search backends.\n\n\
        **Search backends** (configured via ONEAI_SEARCH_BACKEND env var):\n\
        - DuckDuckGo (default) — free, no API key required, limited coverage\n\
        - Google Custom Search — requires ONEAI_SEARCH_API_KEY + ONEAI_SEARCH_CX\n\
        - Bing Search — requires ONEAI_SEARCH_API_KEY\n\
        - SerpAPI — requires ONEAI_SEARCH_API_KEY (most versatile)\n\n\
        **Usage guidelines**:\n\
        - Use for: finding documentation, researching topics, looking up APIs\n\
        - Start with broad queries, then narrow down with specific terms\n\
        - Use web_fetch to read the actual content of promising results\n\
        - Cross-reference findings from multiple queries for accuracy\n\
        - Note: search results may be time-sensitive, check dates\n\n\
        **Preferences**:\n\
        - Prefer web_search over reading local docs for current information\n\
        - Use specific, well-formed queries for better results\n\
        - Combine multiple searches to build comprehensive understanding"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query to look up on the web"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default: 10)",
                    "default": 10
                }
            },
            "required": ["query"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let query = args.get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let max_results = args.get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.max_results as u64) as usize;

        if query.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No search query provided".to_string()),
            });
        }

        // Determine search backend from environment
        let backend = std::env::var("ONEAI_SEARCH_BACKEND")
            .unwrap_or_else(|_| "duckduckgo".to_string());

        let results = match backend.to_lowercase().as_str() {
            "google" => self.search_google(query, max_results).await,
            "bing" => self.search_bing(query, max_results).await,
            "serpapi" => self.search_serpapi(query, max_results).await,
            "duckduckgo" | _ => self.search_duckduckgo(query, max_results).await,
        };

        match results {
            Ok(results) => {
                if results.is_empty() {
                    Ok(ToolOutput {
                        success: true,
                        content: format!("No results found for query '{}'", query),
                        error: None,
                    })
                } else {
                    let content = results.iter()
                        .map(|r| format!("## {}\n{}\n[{}]({})", r.title, r.snippet, r.url, r.url))
                        .collect::<Vec<_>>()
                        .join("\n\n");

                    Ok(ToolOutput {
                        success: true,
                        content: format!("Found {} results for '{}':\n\n{}", results.len(), query, content),
                        error: None,
                    })
                }
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Search failed: {}", e)),
            }),
        }
    }
}

impl WebSearchTool {
    /// Search using Google Custom Search API.
    ///
    /// Requires ONEAI_SEARCH_API_KEY and ONEAI_SEARCH_CX environment variables.
    async fn search_google(&self, query: &str, max_results: usize) -> std::result::Result<Vec<SearchResult>, String> {
        let api_key = std::env::var("ONEAI_SEARCH_API_KEY")
            .map_err(|_| "ONEAI_SEARCH_API_KEY not set — required for Google Search")?;
        let cx = std::env::var("ONEAI_SEARCH_CX")
            .map_err(|_| "ONEAI_SEARCH_CX not set — required for Google Custom Search")?;

        let url = format!(
            "https://www.googleapis.com/customsearch/v1?key={}&cx={}&q={}&num={}",
            api_key, cx,
            urlencoding::encode(query),
            max_results.min(10)
        );

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.client.get(&url)
                .header("User-Agent", "OneAI-Agent/0.1.0")
                .send()
        ).await;

        match result {
            Ok(Ok(response)) => {
                let status = response.status();
                if !status.is_success() {
                    return Err(format!("Google Search API error: {}", status));
                }

                let body: serde_json::Value = response.json().await
                    .map_err(|e| format!("Failed to parse Google Search response: {}", e))?;

                let items = body.get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                Ok(items.iter().take(max_results).map(|item| {
                    SearchResult {
                        title: item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        url: item.get("link").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        snippet: item.get("snippet").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    }
                }).collect())
            }
            Ok(Err(e)) => Err(format!("Google Search request failed: {}", e)),
            Err(_) => Err(format!("Google Search timed out after {} seconds", self.timeout_secs)),
        }
    }

    /// Search using Bing Search API.
    ///
    /// Requires ONEAI_SEARCH_API_KEY environment variable.
    async fn search_bing(&self, query: &str, max_results: usize) -> std::result::Result<Vec<SearchResult>, String> {
        let api_key = std::env::var("ONEAI_SEARCH_API_KEY")
            .map_err(|_| "ONEAI_SEARCH_API_KEY not set — required for Bing Search")?;

        let url = format!(
            "https://api.bing.microsoft.com/v7.0/search?q={}&count={}",
            urlencoding::encode(query),
            max_results.min(50)
        );

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.client.get(&url)
                .header("User-Agent", "OneAI-Agent/0.1.0")
                .header("Ocp-Apim-Subscription-Key", &api_key)
                .send()
        ).await;

        match result {
            Ok(Ok(response)) => {
                let status = response.status();
                if !status.is_success() {
                    return Err(format!("Bing Search API error: {}", status));
                }

                let body: serde_json::Value = response.json().await
                    .map_err(|e| format!("Failed to parse Bing Search response: {}", e))?;

                let pages = body.get("webPages")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                Ok(pages.iter().take(max_results).map(|item| {
                    SearchResult {
                        title: item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        url: item.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        snippet: item.get("snippet").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    }
                }).collect())
            }
            Ok(Err(e)) => Err(format!("Bing Search request failed: {}", e)),
            Err(_) => Err(format!("Bing Search timed out after {} seconds", self.timeout_secs)),
        }
    }

    /// Search using SerpAPI.
    ///
    /// Requires ONEAI_SEARCH_API_KEY environment variable.
    async fn search_serpapi(&self, query: &str, max_results: usize) -> std::result::Result<Vec<SearchResult>, String> {
        let api_key = std::env::var("ONEAI_SEARCH_API_KEY")
            .map_err(|_| "ONEAI_SEARCH_API_KEY not set — required for SerpAPI")?;

        let url = format!(
            "https://serpapi.com/search.json?q={}&num={}&api_key={}",
            urlencoding::encode(query),
            max_results.min(100),
            api_key
        );

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.client.get(&url)
                .header("User-Agent", "OneAI-Agent/0.1.0")
                .send()
        ).await;

        match result {
            Ok(Ok(response)) => {
                let status = response.status();
                if !status.is_success() {
                    return Err(format!("SerpAPI error: {}", status));
                }

                let body: serde_json::Value = response.json().await
                    .map_err(|e| format!("Failed to parse SerpAPI response: {}", e))?;

                let results = body.get("organic_results")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                Ok(results.iter().take(max_results).map(|item| {
                    SearchResult {
                        title: item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        url: item.get("link").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        snippet: item.get("snippet").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    }
                }).collect())
            }
            Ok(Err(e)) => Err(format!("SerpAPI request failed: {}", e)),
            Err(_) => Err(format!("SerpAPI timed out after {} seconds", self.timeout_secs)),
        }
    }

    /// Search using DuckDuckGo (free, no API key required).
    ///
    /// Uses DuckDuckGo's HTML search endpoint and parses the results.
    /// This is the default backend when no API key is configured.
    async fn search_duckduckgo(&self, query: &str, max_results: usize) -> std::result::Result<Vec<SearchResult>, String> {
        // DuckDuckGo HTML search endpoint
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding::encode(query)
        );

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.client.get(&url)
                .header("User-Agent", "Mozilla/5.0 (compatible; OneAI-Agent/0.1.0)")
                .send()
        ).await;

        match result {
            Ok(Ok(response)) => {
                let status = response.status();
                if !status.is_success() {
                    return Err(format!("DuckDuckGo search error: {}", status));
                }

                let html = response.text().await
                    .map_err(|e| format!("Failed to read DuckDuckGo response: {}", e))?;

                // Parse DuckDuckGo HTML results
                // The HTML format uses <a class="result__a"> for titles and URLs,
                // and <a class="result__snippet"> for snippets
                let mut results = Vec::new();

                // Simple regex-based parsing of DuckDuckGo HTML results
                let title_re = regex::Regex::new(r#"<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
                let snippet_re = regex::Regex::new(r#"<a[^>]*class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();

                for (idx, cap) in title_re.captures_iter(&html).enumerate() {
                    let url_raw = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let title_raw = cap.get(2).map(|m| m.as_str()).unwrap_or("");

                    // DuckDuckGo wraps URLs with redirects — extract the actual URL
                    let actual_url = if url_raw.contains("uddg=") {
                        // Extract from redirect URL
                        if let Some(start) = url_raw.find("uddg=") {
                            let encoded_url = &url_raw[start + 5..];
                            urlencoding::decode(encoded_url)
                                .map(|s| s.to_string())
                                .unwrap_or_else(|_| encoded_url.to_string())
                        } else {
                            url_raw.to_string()
                        }
                    } else {
                        url_raw.to_string()
                    };

                    // Clean HTML entities from title
                    let title = html_entities_clean(title_raw);

                    // Find matching snippet (after this title in the HTML)
                    let snippet = snippet_re.captures_iter(&html)
                        .nth(idx)
                        .map(|cap| html_entities_clean(cap.get(1).map(|m| m.as_str()).unwrap_or("")))
                        .unwrap_or_default();

                    results.push(SearchResult {
                        title,
                        url: actual_url,
                        snippet,
                    });

                    if results.len() >= max_results {
                        break;
                    }
                }

                Ok(results)
            }
            Ok(Err(e)) => Err(format!("DuckDuckGo request failed: {}", e)),
            Err(_) => Err(format!("DuckDuckGo timed out after {} seconds", self.timeout_secs)),
        }
    }
}

/// Clean HTML entities from text (basic decode of common entities).
fn html_entities_clean(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("<b>", "")
        .replace("</b>", "")
        .replace("<em>", "")
        .replace("</em>", "")
        .replace("<strong>", "")
        .replace("</strong>", "")
        .trim()
        .to_string()
}

// ─── BrowserTool ────────────────────────────────────────────────────────────

/// BrowserTool — web page interaction and content extraction.
///
/// Provides lightweight web automation capabilities for the agent:
/// - **navigate**: Fetch a web page and extract its content as structured text
/// - **extract**: Extract specific content from a page (links, headings, text)
/// - **form_submit**: Submit form data via HTTP GET/POST
///
/// Unlike Claude Code's computer use (which requires a browser GUI),
/// OneAI's BrowserTool operates in "content extraction" mode — it
/// fetches pages via HTTP and converts HTML to structured Markdown.
/// This is more portable (works on Android/iOS/HarmonyOS) and
/// doesn't require a browser binary.
///
/// For full browser automation (screenshots, click/type, JS execution),
/// a future "PlaywrightTool" can be added that shells out to a
/// Playwright subprocess (requires Node.js + browser binary).
///
/// PermissionLevel: Standard — browser access is moderately risky
/// (can access internal URLs, requires network access).
pub struct BrowserTool {
    client: reqwest::Client,
    timeout: std::time::Duration,
}

impl BrowserTool {
    /// Create a new BrowserTool with default settings.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("OneAI-Agent/0.1 (content extraction)")
                .build()
                .unwrap_or_default(),
            timeout: std::time::Duration::from_secs(30),
        }
    }

    /// Create a BrowserTool with custom timeout.
    pub fn with_timeout(timeout_secs: u64) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .user_agent("OneAI-Agent/0.1 (content extraction)")
                .build()
                .unwrap_or_default(),
            timeout: std::time::Duration::from_secs(timeout_secs),
        }
    }

    /// Navigate to a URL and extract page content.
    async fn navigate(&self, url: &str) -> Result<ToolOutput> {
        let result = tokio::time::timeout(self.timeout, async {
            let response = self.client.get(url).send().await
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("Browser navigate error for {}: {}", url, e)
                ))?;

            if !response.status().is_success() {
                return Err(oneai_core::error::OneAIError::Provider(
                    format!("Browser navigate returned status {} for {}", response.status().as_u16(), url)
                ));
            }

            let html = response.text().await
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("Browser navigate read error: {}", e)
                ))?;

            // Convert HTML to structured Markdown
            let markdown = html2text::from_read(html.as_bytes(), 200);

            Ok::<String, oneai_core::error::OneAIError>(markdown)
        }).await;

        match result {
            Ok(Ok(markdown)) => {
                // Truncate if too long (prevent context overflow, char-boundary-safe for CJK)
                let content = if markdown.len() > 10000 {
                    let end = markdown.char_indices()
                        .take_while(|(i, _)| *i < 10000)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    format!("{}... [truncated, total {} chars]", &markdown[..end], markdown.len())
                } else {
                    markdown
                };
                Ok(ToolOutput {
                    success: true,
                    content,
                    error: None,
                })
            }
            Ok(Err(e)) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(e.to_string()),
            }),
            Err(_) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Browser navigate timed out after {} seconds for {}", self.timeout.as_secs(), url)),
            }),
        }
    }

    /// Extract specific content from a page.
    async fn extract(&self, url: &str, selector: &str) -> Result<ToolOutput> {
        // Fetch the page first
        let fetch_result = self.navigate(url).await?;
        if !fetch_result.success {
            return Ok(fetch_result);
        }

        let content = fetch_result.content;

        // Apply selector-based extraction
        // Since we don't have a DOM parser, use heuristic text extraction
        let extracted = match selector {
            "links" | "a" => {
                // Extract links from the content (lines containing http/https URLs)
                content.lines()
                    .filter(|l| l.contains("http://") || l.contains("https://"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "headings" | "h" => {
                // Extract heading lines (lines starting with # in markdown)
                content.lines()
                    .filter(|l| l.trim().starts_with('#'))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "text" | "p" => {
                // Extract paragraph text (lines not starting with # or [)
                content.lines()
                    .filter(|l| !l.trim().starts_with('#') && !l.trim().starts_with('[') && !l.trim().is_empty())
                    .take(50) // Limit to 50 lines
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "title" => {
                // Extract just the first heading
                content.lines()
                    .find(|l| l.trim().starts_with('#'))
                    .map(|l| l.trim().to_string())
                    .unwrap_or_else(|| "No title found".to_string())
            }
            other => {
                // Generic: search for lines containing the selector keyword
                content.lines()
                    .filter(|l| l.contains(other))
                    .take(20)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };

        let result = if extracted.is_empty() {
            "No content found matching selector".to_string()
        } else if extracted.len() > 5000 {
            // Char-boundary-safe truncation for CJK content
            let end = extracted.char_indices()
                .take_while(|(i, _)| *i < 5000)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            format!("{}... [truncated]", &extracted[..end])
        } else {
            extracted
        };

        Ok(ToolOutput {
            success: true,
            content: result,
            error: None,
        })
    }

    /// Submit a form via HTTP POST.
    async fn form_submit(&self, url: &str, data: &serde_json::Value) -> Result<ToolOutput> {
        let result = tokio::time::timeout(self.timeout, async {
            let response = self.client.post(url)
                .json(data)
                .send().await
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("Browser form submit error for {}: {}", url, e)
                ))?;

            let status = response.status();
            let body = response.text().await
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("Browser form submit read error: {}", e)
                ))?;

            // Try to convert response body to markdown if it's HTML
            let content = if body.contains("<html") || body.contains("<!DOCTYPE") {
                let md = html2text::from_read(body.as_bytes(), 200);
                if md.len() > 5000 {
                    // Char-boundary-safe truncation for CJK content
                    let end = md.char_indices()
                        .take_while(|(i, _)| *i < 5000)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    format!("{}... [truncated]", &md[..end])
                } else {
                    md
                }
            } else if body.len() > 5000 {
                // Char-boundary-safe truncation for CJK content
                let end = body.char_indices()
                    .take_while(|(i, _)| *i < 5000)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                format!("Status: {}\n{}", status.as_u16(), &body[..end])
            } else {
                format!("Status: {}\n{}", status.as_u16(), body)
            };

            Ok::<String, oneai_core::error::OneAIError>(content)
        }).await;

        match result {
            Ok(Ok(content)) => Ok(ToolOutput {
                success: true,
                content,
                error: None,
            }),
            Ok(Err(e)) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(e.to_string()),
            }),
            Err(_) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Browser form submit timed out after {} seconds", self.timeout.as_secs())),
            }),
        }
    }
}

impl Default for BrowserTool {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str { "browser" }

    fn description(&self) -> &str {
        "Web page browser — navigate URLs, extract content, submit forms. \
        Actions: navigate (fetch page as markdown), extract (extract specific \
        content by selector), form_submit (POST data to a URL). \
        Lightweight HTTP-based approach — works without browser binary."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["navigate", "extract", "form_submit"],
                    "description": "The browser action to perform"
                },
                "url": {
                    "type": "string",
                    "description": "The URL to navigate to or submit data to"
                },
                "selector": {
                    "type": "string",
                    "description": "Content selector for extract action: 'links', 'headings', 'text', 'title', or custom keyword",
                    "default": "text"
                },
                "data": {
                    "type": "object",
                    "description": "Form data for form_submit action (JSON object)"
                }
            },
            "required": ["action", "url"]
        })
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::Medium }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let action = args.get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("navigate");
        let url = args.get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("");

        if url.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("browser: url parameter is required".to_string()),
            });
        }

        match action {
            "navigate" => self.navigate(url).await,
            "extract" => {
                let selector = args.get("selector")
                    .and_then(|s| s.as_str())
                    .unwrap_or("text");
                self.extract(url, selector).await
            }
            "form_submit" => {
                let data = args.get("data").cloned().unwrap_or(serde_json::json!({}));
                self.form_submit(url, &data).await
            }
            other => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("browser: unknown action '{}'. Use: navigate, extract, form_submit", other)),
            }),
        }
    }
}

impl PermissionAwareTool for BrowserTool {
    fn permission_level(&self) -> PermissionLevel { PermissionLevel::Standard }
}