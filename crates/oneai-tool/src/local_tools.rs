//! Local tool implementations — legacy tools that haven't been migrated to tool_interfaces.
//!
//! Tools remaining here: FileWriteTool, CalculatorTool.

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use oneai_core::{Artifact, RiskLevel, ToolOutput};

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Best-effort MIME inference by file extension — no new dependency. Used to
/// populate [`Artifact::mime_type`] on file-writing tool outputs so a frontend
/// render an icon / preview hint. Unknown extensions default to `text/plain`.
pub(crate) fn infer_mime(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().filter(|e| !e.is_empty());
    match ext.map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("rs") => "text/rust",
        Some("ts" | "tsx") => "text/typescript",
        Some("js" | "jsx" | "mjs" | "cjs") => "text/javascript",
        Some("json") => "application/json",
        Some("toml") => "application/toml",
        Some("md") => "text/markdown",
        Some("py") => "text/x-python",
        Some("go") => "text/x-go",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("yaml" | "yml") => "text/yaml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        _ => "text/plain",
    }
}

// ─── FileWriteTool ──────────────────────────────────────────────────────────

/// File write tool — writes content to a local file.
///
/// This is a HIGH-RISK tool — writing files can overwrite important data.
pub struct FileWriteTool {
    /// The file-operations backend (Phase 4.2). Defaults to `LocalFileOps`
    /// (current behavior, verbatim); a `RemoteFileOps` routes writes through
    /// a `TerminalBackend` for a `ContainerizedCodingPack`.
    file_ops: std::sync::Arc<dyn crate::file_ops::FileOperations>,
}

impl FileWriteTool {
    /// Create a new file write tool with the local filesystem backend.
    pub fn new() -> Self {
        Self {
            file_ops: std::sync::Arc::new(crate::file_ops::LocalFileOps::new()),
        }
    }

    /// Create with a custom file-operations backend (Phase 4.2). Route writes
    /// through a `TerminalBackend` (VM-backed) instead of the local FS.
    pub fn with_file_ops(file_ops: std::sync::Arc<dyn crate::file_ops::FileOperations>) -> Self {
        Self { file_ops }
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
        "Create a new file or overwrite an existing file with the given content. \
        Parent directories are created automatically if missing, so you can write \
        to paths like `src/new_mod/mod.rs` in a single call. Set `append=true` to \
        append to an existing file instead of overwriting.\n\n\
        **RECOMMENDED** for creating/writing files — do NOT use shell for this:\n\
        - Do NOT use `cat > file` / heredoc (`<<EOF`) → use write_file\n\
        - Do NOT use `echo > file` / `tee` / `printf > file` → use write_file\n\
        - For targeted edits to part of an existing file, use edit_file instead\n\
        - For batch/multi-file changes, use apply_patch instead\n\n\
        This is a high-risk tool: writing overwrites existing content without confirmation."
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
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let append = args
            .get("append")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if path.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No file path provided".to_string()),
                ..Default::default()
            });
        }

        // Security: reject path traversal
        if crate::tool_interfaces::path_has_traversal(path) {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Path traversal detected".to_string()),
                ..Default::default()
            });
        }

        // Delegate the write to the file-operations backend (Phase 4.2). The
        // LocalFileOps path is byte-identical to the pre-refactor inline
        // `tokio::fs::write`/append + parent-dir creation logic; a
        // RemoteFileOps routes the write through a TerminalBackend.
        let write_result = self.file_ops.write(path, content, append).await;

        match write_result {
            Ok(_) => Ok(ToolOutput {
                success: true,
                content: format!("Successfully wrote {} bytes to {}", content.len(), path),
                error: None,
                artifacts: vec![Artifact {
                    path: path.to_string(),
                    mime_type: infer_mime(path).to_string(),
                    description: format!(
                        "{} {} bytes",
                        if append { "appended" } else { "wrote" },
                        content.len()
                    ),
                    size_bytes: Some(content.len() as u64),
                }],
                ..Default::default()
            }),
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to write file: {}", e)),
                ..Default::default()
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
        let expression = args
            .get("expression")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if expression.is_empty() {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some("No expression provided".to_string()),
                ..Default::default()
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
                ..Default::default()
            }),
            Err(msg) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(msg),
                ..Default::default()
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
        if !ch.is_ascii_digit()
            && ch != '.'
            && ch != '+'
            && ch != '-'
            && ch != '*'
            && ch != '/'
            && ch != '('
            && ch != ')'
        {
            return Err(format!("Invalid character in expression: '{}'", ch));
        }
    }

    // Use a simple tokenizer + recursive descent parser
    let mut pos = 0;
    let chars = expr.as_bytes();

    fn parse_number(chars: &[u8], pos: &mut usize) -> std::result::Result<f64, String> {
        let start = *pos;
        while *pos < chars.len() && (chars[*pos].is_ascii_digit() || chars[*pos] == b'.') {
            *pos += 1;
        }
        let num_str = std::str::from_utf8(&chars[start..*pos]).unwrap();
        num_str
            .parse::<f64>()
            .map_err(|e| format!("Invalid number: {}", e))
    }

    fn parse_expr(chars: &[u8], pos: &mut usize) -> std::result::Result<f64, String> {
        let mut result = parse_term(chars, pos)?;

        while *pos < chars.len() {
            let op = chars[*pos];
            if op == b'+' || op == b'-' {
                *pos += 1;
                let term = parse_term(chars, pos)?;
                if op == b'+' {
                    result += term
                } else {
                    result -= term
                };
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
            if op == b'*' || op == b'/' {
                *pos += 1;
                let factor = parse_factor(chars, pos)?;
                if op == b'*' {
                    result *= factor
                } else {
                    if factor == 0.0 {
                        return Err("Division by zero".to_string());
                    }
                    result /= factor
                };
            } else {
                break;
            }
        }

        Ok(result)
    }

    fn parse_factor(chars: &[u8], pos: &mut usize) -> std::result::Result<f64, String> {
        // Handle negative numbers
        if *pos < chars.len() && chars[*pos] == b'-' {
            *pos += 1;
            return Ok(-parse_factor(chars, pos)?);
        }

        // Handle parentheses
        if *pos < chars.len() && chars[*pos] == b'(' {
            *pos += 1;
            let result = parse_expr(chars, pos)?;
            if *pos >= chars.len() || chars[*pos] != b')' {
                return Err("Missing closing parenthesis".to_string());
            }
            *pos += 1;
            return Ok(result);
        }

        // Handle number
        parse_number(chars, pos)
    }

    let result = parse_expr(chars, &mut pos)?;

    if pos != chars.len() {
        return Err("Unexpected characters at end of expression".to_string());
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::Tool;

    /// write_file success path must surface the written file as an
    /// [`Artifact`] deliverable (§W4 A2) — the frontend renders the per-turn
    /// deliverable strip off this field.
    #[tokio::test]
    async fn write_file_surfaces_artifact_on_success() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oneai_w4_artifact_{}.rs", std::process::id()));
        let path_str = path.to_str().unwrap();
        let tool = FileWriteTool::new();
        let out = tool
            .execute(serde_json::json!({
                "path": path_str,
                "content": "fn main() {}\n"
            }))
            .await
            .expect("write succeeds");
        assert!(out.success, "content: {:?}", out.content);
        assert_eq!(out.artifacts.len(), 1, "exactly one artifact");
        assert_eq!(out.artifacts[0].path, path_str);
        assert_eq!(out.artifacts[0].mime_type, "text/rust");
        assert!(out.artifacts[0].size_bytes.is_some());
        let _ = std::fs::remove_file(path);
    }

    /// `infer_mime` covers the common code-extension table; unknown → text/plain.
    #[test]
    fn infer_mime_covers_common_extensions() {
        assert_eq!(infer_mime("a.rs"), "text/rust");
        assert_eq!(infer_mime("b.TS"), "text/typescript");
        assert_eq!(infer_mime("c.png"), "image/png");
        assert_eq!(infer_mime("d.svg"), "image/svg+xml");
        assert_eq!(infer_mime("README"), "text/plain");
        assert_eq!(infer_mime("noext"), "text/plain");
    }
}
