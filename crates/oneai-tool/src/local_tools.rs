//! Local tool implementations — legacy tools that haven't been migrated to tool_interfaces.
//!
//! Tools remaining here: FileWriteTool, CalculatorTool.

use async_trait::async_trait;
use oneai_core::{RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;

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
            if op == '*' as u8 || op == '/' as u8 {
                *pos += 1;
                let factor = parse_factor(chars, pos)?;
                if op == '*' as u8 {
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