//! Local tool implementations — shell execution, file read, etc.
//!
//! Provides concrete implementations of the Tool trait for common
//! local operations. These tools are platform-independent where possible,
//! with platform-specific variations handled in the platform adaptation layer.

use async_trait::async_trait;
use oneai_core::{RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;

// ─── ShellTool ──────────────────────────────────────────────────────────────

/// Shell command execution tool.
///
/// Executes shell commands on the local system.
/// This is a HIGH-RISK tool — it requires approval from the ApprovalGate.
///
/// Platform-specific behavior:
/// - Windows: Uses PowerShell
/// - Unix (Mac/Linux/Android/iOS): Uses sh
pub struct ShellTool {
    /// Maximum timeout for command execution (in seconds).
    timeout_secs: u64,
}

impl ShellTool {
    /// Create a new shell tool with default timeout (30 seconds).
    pub fn new() -> Self {
        Self { timeout_secs: 30 }
    }

    /// Create with a custom timeout.
    pub fn with_timeout(timeout_secs: u64) -> Self {
        Self { timeout_secs }
    }

    /// Get the configured timeout in seconds.
    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command on the local system. Returns the command output (stdout and stderr). \
        Use with caution — this is a high-risk tool that requires human approval before execution."
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
                    "description": "Optional timeout in seconds (default: 30)",
                    "default": 30
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

        // Determine the shell based on the platform
        let (shell, shell_arg) = if cfg!(target_os = "windows") {
            ("powershell", "-Command")
        } else {
            ("sh", "-c")
        };

        let timeout = args.get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.timeout_secs);

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

                Ok(ToolOutput {
                    success: output.status.success(),
                    content,
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

// ─── FileReadTool ───────────────────────────────────────────────────────────

/// File read tool — reads the contents of a local file.
///
/// This is a MEDIUM-RISK tool because it can read sensitive files.
pub struct FileReadTool {
    /// Maximum file size to read (in bytes).
    max_size_bytes: usize,
}

impl FileReadTool {
    /// Create a new file read tool with default max size (1MB).
    pub fn new() -> Self {
        Self { max_size_bytes: 1024 * 1024 }
    }

    /// Create with a custom max size.
    pub fn with_max_size(max_size_bytes: usize) -> Self {
        Self { max_size_bytes }
    }
}

impl Default for FileReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a local file. Returns the file content as text. \
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
                }
            },
            "required": ["path"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
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
                    "File too large: {} bytes (max: {} bytes)",
                    file_size, self.max_size_bytes
                )),
            });
        }

        // Read the file content
        let content = tokio::fs::read_to_string(path).await;

        match content {
            Ok(text) => Ok(ToolOutput {
                success: true,
                content: text,
                error: None,
            }),
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

// ─── FileWriteTool ──────────────────────────────────────────────────────────

/// File write tool — writes content to a local file.
///
/// This is a HIGH-RISK tool — writing files can overwrite important data.
pub struct FileWriteTool;

impl FileWriteTool {
    /// Create a new file write tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a local file. This is a high-risk tool that requires approval. \
        Can create new files or overwrite existing ones."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                },
                "append": {
                    "type": "boolean",
                    "description": "Whether to append to existing file (default: false)",
                    "default": false
                }
            },
            "required": ["path", "content"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::High
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = args.get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let append = args.get("append")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if path.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No file path provided".to_string()),
            });
        }

        // Security: reject path traversal
        if path.contains("..") {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Path traversal detected".to_string()),
            });
        }

        let result = if append {
            // Append mode: open file in append mode and write content
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await;
            match file {
                Ok(f) => {
                    use tokio::io::AsyncWriteExt;
                    let mut writer = tokio::io::BufWriter::new(f);
                    writer.write_all(content.as_bytes()).await
                }
                Err(e) => Err(e),
            }
        } else {
            tokio::fs::write(path, content).await
        };

        match result {
            Ok(_) => Ok(ToolOutput {
                success: true,
                content: format!("Successfully wrote {} bytes to {}", content.len(), path),
                error: None,
            }),
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to write file: {}", e)),
            }),
        }
    }
}

// ─── CalculatorTool ─────────────────────────────────────────────────────────

/// Simple calculator tool — evaluates mathematical expressions.
///
/// This is a LOW-RISK tool — no approval needed.
pub struct CalculatorTool;

impl CalculatorTool {
    /// Create a new calculator tool.
    pub fn new() -> Self {
        Self
    }
}

impl Default for CalculatorTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for CalculatorTool {
    fn name(&self) -> &str {
        "calculator"
    }

    fn description(&self) -> &str {
        "Evaluate a mathematical expression. Supports basic arithmetic: +, -, *, /, parentheses. \
        Returns the numeric result."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "The mathematical expression to evaluate (e.g., '2 + 3 * 4')"
                }
            },
            "required": ["expression"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let expression = args.get("expression")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if expression.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No expression provided".to_string()),
            });
        }

        // Simple expression evaluator — supports +, -, *, /, and parentheses
        // This is a basic implementation; a production version would use a proper parser
        let result = evaluate_expression(expression);

        match result {
            Ok(value) => Ok(ToolOutput {
                success: true,
                content: format!("{}", value),
                error: None,
            }),
            Err(msg) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(msg),
            }),
        }
    }
}

/// Simple mathematical expression evaluator.
///
/// Supports: +, -, *, /, parentheses, and integer/float literals.
/// This is a basic recursive descent parser.
fn evaluate_expression(expr: &str) -> std::result::Result<f64, String> {
    // Remove whitespace
    let expr = expr.replace(" ", "");

    // Validate that the expression only contains safe characters
    for ch in expr.chars() {
        if !ch.is_ascii_digit() && ch != '.' && ch != '+' && ch != '-' && ch != '*' && ch != '/' && ch != '(' && ch != ')' {
            return Err(format!("Invalid character in expression: '{}'", ch));
        }
    }

    // Use a simple tokenizer + recursive descent parser
    let mut pos = 0;
    let chars = expr.as_bytes();

    fn parse_number(chars: &[u8], pos: &mut usize) -> std::result::Result<f64, String> {
        let start = *pos;
        while *pos < chars.len() && (chars[*pos].is_ascii_digit() || chars[*pos] == '.' as u8) {
            *pos += 1;
        }
        let num_str = std::str::from_utf8(&chars[start..*pos]).unwrap();
        num_str.parse::<f64>().map_err(|e| format!("Invalid number: {}", e))
    }

    fn parse_expr(chars: &[u8], pos: &mut usize) -> std::result::Result<f64, String> {
        let mut result = parse_term(chars, pos)?;

        while *pos < chars.len() {
            let op = chars[*pos];
            if op == '+' as u8 || op == '-' as u8 {
                *pos += 1;
                let term = parse_term(chars, pos)?;
                if op == '+' as u8 {
                    result += term;
                } else {
                    result -= term;
                }
            } else {
                break;
            }
        }

        Ok(result)
    }

    fn parse_term(chars: &[u8], pos: &mut usize) -> std::result::Result<f64, String> {
        let mut result = parse_factor(chars, pos)?;

        while *pos < chars.len() {
            let op = chars[*pos];
            if op == '*' as u8 || op == '/' as u8 {
                *pos += 1;
                let factor = parse_factor(chars, pos)?;
                if op == '*' as u8 {
                    result *= factor;
                } else {
                    if factor == 0.0 {
                        return Err("Division by zero".to_string());
                    }
                    result /= factor;
                }
            } else {
                break;
            }
        }

        Ok(result)
    }

    fn parse_factor(chars: &[u8], pos: &mut usize) -> std::result::Result<f64, String> {
        // Handle negative numbers
        if *pos < chars.len() && chars[*pos] == '-' as u8 {
            *pos += 1;
            return Ok(-parse_factor(chars, pos)?);
        }

        // Handle parentheses
        if *pos < chars.len() && chars[*pos] == '(' as u8 {
            *pos += 1;
            let result = parse_expr(chars, pos)?;
            if *pos >= chars.len() || chars[*pos] != ')' as u8 {
                return Err("Missing closing parenthesis".to_string());
            }
            *pos += 1;
            return Ok(result);
        }

        // Handle number
        parse_number(chars, pos)
    }

    let result = parse_expr(&chars, &mut pos)?;

    if pos != chars.len() {
        return Err("Unexpected characters at end of expression".to_string());
    }

    Ok(result)
}