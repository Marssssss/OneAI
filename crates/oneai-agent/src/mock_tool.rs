//! MockTool — test tool with call logging for E2E verification.
//!
//! Each MockTool implements the Tool trait and returns a fixed output,
//! while recording every call in a shared log. Tests can inspect the
//! log to verify which tools were called, in what order, and with
//! what arguments.
//!
//! Predefined mock tools:
//! - `read_file_mock()` — returns "hello world" (Read permission)
//! - `edit_file_mock()` — returns "File edited successfully" (Standard permission)
//! - `shell_mock()` — returns "Command output: OK" (Full permission)
//! - `shell_mock_error()` — returns a timeout error (Full permission)
//! - `grep_mock()` — returns "3 matches found" (Read permission)
//! - `glob_mock()` — returns "5 files found" (Read permission)
//! - `environment_mock()` — returns environment info (Read permission)
//!
//! Usage:
//! ```ignore
//! let tool = MockTool::read_file_mock();
//! let call_log = tool.call_log();
//!
//! // After AgentLoop runs:
//! let log = call_log.lock().await;
//! assert_eq!(log.len(), 1);
//! assert_eq!(log[0].args["path"], "/test.txt");
//! ```

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::Mutex;

use oneai_core::{PermissionLevel, RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;

// ─── ToolCallLog ──────────────────────────────────────────────────────────────

/// A record of a tool call — for test assertions.
#[derive(Debug, Clone)]
pub struct ToolCallLog {
    /// The arguments passed to the tool.
    pub args: serde_json::Value,
    /// When the call was made (relative to test start).
    pub timestamp: Instant,
}

// ─── MockTool ──────────────────────────────────────────────────────────────────

/// A test tool that returns a fixed output and logs all calls.
///
/// The `call_log` is shared via Arc<Mutex> so tests can inspect it
/// after the AgentLoop completes. The fixed output can be either
/// success or failure, depending on the test scenario.
pub struct MockTool {
    /// Tool name (e.g., "read_file", "shell").
    name: String,
    /// Human-readable description.
    description: String,
    /// JSON Schema for parameters.
    parameters_schema: serde_json::Value,
    /// The fixed output this tool always returns.
    fixed_output: ToolOutput,
    /// The permission level of this tool.
    permission_level: PermissionLevel,
    /// Shared call log — records every execute() call.
    call_log: Arc<Mutex<Vec<ToolCallLog>>>,
}

impl MockTool {
    /// Create a new MockTool with custom name, output, and permission level.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters_schema: serde_json::Value,
        fixed_output: ToolOutput,
        permission_level: PermissionLevel,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters_schema,
            fixed_output,
            permission_level,
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Get a shared reference to the call log.
    /// Tests can inspect this after the AgentLoop completes.
    pub fn call_log(&self) -> Arc<Mutex<Vec<ToolCallLog>>> {
        self.call_log.clone()
    }

    /// Get the number of times this tool was called (async).
    pub async fn call_count(&self) -> usize {
        self.call_log.lock().await.len()
    }

    // ─── Predefined mock tools ───────────────────────────────────────────

    /// Read-file mock — returns "hello world" content, Read permission.
    /// Auto-approved by any approval gate.
    pub fn read_file_mock() -> Self {
        Self::new(
            "read_file",
            "Read the contents of a file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to read" }
                },
                "required": ["path"]
            }),
            ToolOutput {
                success: true,
                content: "hello world".to_string(),
                error: None,
            },
            PermissionLevel::Read,
        )
    }

    /// Read-file mock with custom content.
    pub fn read_file_mock_with_content(content: impl Into<String>) -> Self {
        Self::new(
            "read_file",
            "Read the contents of a file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to read" }
                },
                "required": ["path"]
            }),
            ToolOutput {
                success: true,
                content: content.into(),
                error: None,
            },
            PermissionLevel::Read,
        )
    }

    /// Edit-file mock — returns "File edited successfully", Standard permission.
    pub fn edit_file_mock() -> Self {
        Self::new(
            "edit_file",
            "Edit a file with specified changes",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to edit" },
                    "changes": { "type": "string", "description": "Changes to apply" }
                },
                "required": ["path", "changes"]
            }),
            ToolOutput {
                success: true,
                content: "File edited successfully".to_string(),
                error: None,
            },
            PermissionLevel::Standard,
        )
    }

    /// Shell mock — returns "Command output: OK", Full permission.
    /// This tool always requires approval from the ApprovalGate.
    pub fn shell_mock() -> Self {
        Self::new(
            "shell",
            "Execute a shell command",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" }
                },
                "required": ["command"]
            }),
            ToolOutput {
                success: true,
                content: "Command output: OK".to_string(),
                error: None,
            },
            PermissionLevel::Full,
        )
    }

    /// Shell mock that returns a timeout error — Full permission.
    /// Used for testing error recovery scenarios.
    pub fn shell_mock_error() -> Self {
        Self::new(
            "shell",
            "Execute a shell command",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" }
                },
                "required": ["command"]
            }),
            ToolOutput {
                success: false,
                content: String::new(),
                error: Some("Error: Command timed out after 30 seconds".to_string()),
            },
            PermissionLevel::Full,
        )
    }

    /// Shell mock with custom error message — Full permission.
    pub fn shell_mock_with_error(error_msg: impl Into<String>) -> Self {
        Self::new(
            "shell",
            "Execute a shell command",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" }
                },
                "required": ["command"]
            }),
            ToolOutput {
                success: false,
                content: String::new(),
                error: Some(error_msg.into()),
            },
            PermissionLevel::Full,
        )
    }

    /// Grep mock — returns "3 matches found", Read permission.
    pub fn grep_mock() -> Self {
        Self::new(
            "grep",
            "Search for patterns in files",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Pattern to search" },
                    "path": { "type": "string", "description": "Path to search in" }
                },
                "required": ["pattern"]
            }),
            ToolOutput {
                success: true,
                content: "3 matches found:\nline 10: fn main()\nline 25: let result\nline 42: return result".to_string(),
                error: None,
            },
            PermissionLevel::Read,
        )
    }

    /// Glob mock — returns "5 files found", Read permission.
    pub fn glob_mock() -> Self {
        Self::new(
            "glob",
            "Find files matching a pattern",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern" }
                },
                "required": ["pattern"]
            }),
            ToolOutput {
                success: true,
                content: "5 files found: main.rs, lib.rs, mod.rs, utils.rs, test.rs".to_string(),
                error: None,
            },
            PermissionLevel::Read,
        )
    }

    /// Environment mock — returns environment info, Read permission.
    pub fn environment_mock() -> Self {
        Self::new(
            "environment",
            "Get environment information",
            serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            ToolOutput {
                success: true,
                content: "Platform: macOS, Working dir: /tmp/test, Shell: zsh".to_string(),
                error: None,
            },
            PermissionLevel::Read,
        )
    }

    /// List-directory mock — returns directory contents, Read permission.
    pub fn list_directory_mock() -> Self {
        Self::new(
            "list_directory",
            "List contents of a directory",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory path" }
                },
                "required": ["path"]
            }),
            ToolOutput {
                success: true,
                content: "main.rs, lib.rs, Cargo.toml, tests/".to_string(),
                error: None,
            },
            PermissionLevel::Read,
        )
    }

    /// Web-fetch mock — returns web content, Standard permission.
    pub fn web_fetch_mock() -> Self {
        Self::new(
            "web_fetch",
            "Fetch content from a URL",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" }
                },
                "required": ["url"]
            }),
            ToolOutput {
                success: true,
                content: "Fetched content from URL: <html><body>Example page</body></html>".to_string(),
                error: None,
            },
            PermissionLevel::Standard,
        )
    }

    /// Generic mock tool with custom name and success output.
    pub fn success_tool(name: impl Into<String>, output: impl Into<String>) -> Self {
        Self::new(
            name,
            "A mock tool for testing",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                }
            }),
            ToolOutput {
                success: true,
                content: output.into(),
                error: None,
            },
            PermissionLevel::Read,
        )
    }

    /// Generic mock tool with custom name and error output.
    pub fn error_tool(name: impl Into<String>, error_msg: impl Into<String>) -> Self {
        Self::new(
            name,
            "A mock tool that always fails",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string" }
                }
            }),
            ToolOutput {
                success: false,
                content: String::new(),
                error: Some(error_msg.into()),
            },
            PermissionLevel::Standard,
        )
    }
}

#[async_trait]
impl Tool for MockTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters_schema.clone()
    }

    fn risk_level(&self) -> RiskLevel {
        self.permission_level.to_risk_level()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        // Log this call
        self.call_log.lock().await.push(ToolCallLog {
            args: args.clone(),
            timestamp: Instant::now(),
        });

        // Return the fixed output
        Ok(self.fixed_output.clone())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_tool_read_file() {
        let tool = MockTool::read_file_mock();
        assert_eq!(tool.name(), "read_file");
        assert_eq!(tool.risk_level(), RiskLevel::Low); // Read → Low

        let result = tool.execute(serde_json::json!({"path": "/test.txt"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "hello world");
    }

    #[tokio::test]
    async fn test_mock_tool_call_logging() {
        let tool = MockTool::read_file_mock();

        tool.execute(serde_json::json!({"path": "/a.txt"})).await.unwrap();
        tool.execute(serde_json::json!({"path": "/b.txt"})).await.unwrap();

        let log = tool.call_log.lock().await;
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].args["path"], "/a.txt");
        assert_eq!(log[1].args["path"], "/b.txt");
    }

    #[tokio::test]
    async fn test_mock_tool_shell_full_permission() {
        let tool = MockTool::shell_mock();
        assert_eq!(tool.risk_level(), RiskLevel::High); // Full → High

        let result = tool.execute(serde_json::json!({"command": "ls"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "Command output: OK");
    }

    #[tokio::test]
    async fn test_mock_tool_error() {
        let tool = MockTool::shell_mock_error();
        assert_eq!(tool.risk_level(), RiskLevel::High);

        let result = tool.execute(serde_json::json!({"command": "timeout_cmd"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn test_mock_tool_edit_standard_permission() {
        let tool = MockTool::edit_file_mock();
        assert_eq!(tool.risk_level(), RiskLevel::Medium); // Standard → Medium

        let result = tool.execute(serde_json::json!({"path": "/test.rs", "changes": "fix"})).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_custom_mock_tool() {
        let tool = MockTool::success_tool("custom_tool", "custom output");
        assert_eq!(tool.name(), "custom_tool");

        let result = tool.execute(serde_json::json!({"input": "test"})).await.unwrap();
        assert_eq!(result.content, "custom output");
    }

    #[tokio::test]
    async fn test_call_count() {
        let tool = MockTool::read_file_mock();

        assert_eq!(tool.call_count().await, 0);

        tool.execute(serde_json::json!({"path": "/a"})).await.unwrap();
        assert_eq!(tool.call_count().await, 1);

        tool.execute(serde_json::json!({"path": "/b"})).await.unwrap();
        assert_eq!(tool.call_count().await, 2);
    }
}
