//! # OneAI Tool
//!
//! Tool management, registry, MCP integration, approval gates, and tool executor.
//! New: PermissionAwareTool trait, expanded tool interfaces, real MCP implementation,
//! ApplyPatchTool for batch editing via unified diff format.

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.


pub mod registry;
pub mod local_tools;
pub mod mcp_tools;
pub mod mcp_real;
pub mod approval;
pub mod interaction_gate;
pub mod executor;
pub mod tool_interfaces;
pub mod apply_patch;
pub mod sandbox;

// Explicit imports to avoid ambiguity between local_tools and tool_interfaces
// (both used to define ShellTool and FileReadTool, but those are now only in tool_interfaces)
pub use registry::*;
pub use local_tools::{FileWriteTool, CalculatorTool};
pub use mcp_tools::*;
pub use mcp_real::{McpConnection, McpFramingParser, McpServerConfig, McpTransport, McpToolInfo};
pub use mcp_real::McpToolWrapper as RealMcpToolWrapper;
pub use mcp_real::McpServerManager as RealMcpServerManager;
pub use mcp_real::{default_mcp_configs, optional_mcp_configs};
pub use approval::*;
pub use interaction_gate::*;
pub use executor::*;
pub use tool_interfaces::*;
pub use apply_patch::{ApplyPatchTool, DiffHunk, DiffLine, parse_unified_diff};
pub use sandbox::{SandboxBackend, SeatbeltBackend, DockerBackend, RegexBackend, WrappedCommand, default_sandbox_backend};
pub use tool_interfaces::{WebSearchTool, SearchResult};

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::traits::Tool;
    use oneai_core::RiskLevel;

    #[tokio::test]
    async fn test_tool_registry_register_and_get() {
        let registry = ToolRegistry::new();
        let shell_tool = std::sync::Arc::new(ShellTool::new());
        registry.register(shell_tool.clone()).await.unwrap();

        let tool = registry.get("shell").await;
        assert!(tool.is_some());
        assert_eq!(tool.unwrap().name(), "shell");
    }

    #[tokio::test]
    async fn test_tool_registry_list_names() {
        let registry = ToolRegistry::new();
        let calc_tool = std::sync::Arc::new(CalculatorTool::new());
        let read_tool = std::sync::Arc::new(FileReadTool::new());
        registry.register(calc_tool).await.unwrap();
        registry.register(read_tool).await.unwrap();

        let names = registry.list_names().await;
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n == "calculator"));
        assert!(names.iter().any(|n| n == "read_file"));
    }

    #[tokio::test]
    async fn test_tool_registry_execute() {
        let registry = ToolRegistry::new();
        let calc_tool = std::sync::Arc::new(CalculatorTool::new());
        registry.register(calc_tool).await.unwrap();

        let result = registry.execute("calculator", serde_json::json!({"expression": "2 + 3"})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.success);
        assert_eq!(output.content, "5");
    }

    #[tokio::test]
    async fn test_tool_registry_not_found() {
        let registry = ToolRegistry::new();
        let result = registry.execute("nonexistent", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_calculator_tool() {
        let tool = CalculatorTool::new();
        assert_eq!(tool.name(), "calculator");
        assert_eq!(tool.risk_level(), RiskLevel::Low);

        // Test basic arithmetic
        let result = tool.execute(serde_json::json!({"expression": "2 + 3"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5");

        // Test multiplication
        let result = tool.execute(serde_json::json!({"expression": "3 * 4"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "12");

        // Test parentheses
        let result = tool.execute(serde_json::json!({"expression": "(2 + 3) * 4"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "20");

        // Test division
        let result = tool.execute(serde_json::json!({"expression": "10 / 2"})).await.unwrap();
        assert!(result.success);

        // Test negative number
        let result = tool.execute(serde_json::json!({"expression": "-5 + 10"})).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_calculator_division_by_zero() {
        let tool = CalculatorTool::new();
        let result = tool.execute(serde_json::json!({"expression": "10 / 0"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_calculator_invalid_expression() {
        let tool = CalculatorTool::new();
        let result = tool.execute(serde_json::json!({"expression": "abc"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_calculator_empty_expression() {
        let tool = CalculatorTool::new();
        let result = tool.execute(serde_json::json!({"expression": ""})).await.unwrap();
        assert!(!result.success);
    }

    #[test]
    fn test_shell_tool_properties() {
        let tool = ShellTool::new();
        assert_eq!(tool.name(), "shell");
        assert_eq!(tool.risk_level(), RiskLevel::High);
    }

    #[test]
    fn test_shell_tool_timeout() {
        let tool = ShellTool::new();
        assert_eq!(tool.timeout_secs(), 120);
    }

    #[test]
    fn test_file_read_tool_properties() {
        let tool = FileReadTool::new();
        assert_eq!(tool.name(), "read_file");
        // FileReadTool now uses PermissionLevel::Read → RiskLevel::Low
        assert_eq!(tool.risk_level(), RiskLevel::Low);
    }

    #[test]
    fn test_file_write_tool_properties() {
        let tool = FileWriteTool::new();
        assert_eq!(tool.name(), "write_file");
        assert_eq!(tool.risk_level(), RiskLevel::High);
    }

    #[test]
    fn test_mcp_tool_wrapper() {
        let tool = McpToolWrapper::new(
            "web_search".to_string(),
            "Search the web".to_string(),
            serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            "web_server".to_string(),
        );
        assert_eq!(tool.name(), "web_search");
        assert_eq!(tool.risk_level(), RiskLevel::Medium);
    }

    #[test]
    fn test_mcp_server_manager() {
        let mut manager = McpServerManager::new();
        let tools = vec![
            McpToolWrapper::new(
                "search".to_string(),
                "Search tool".to_string(),
                serde_json::json!({}),
                "server1".to_string(),
            ),
        ];
        manager.register_server_tools("server1".to_string(), tools);

        assert_eq!(manager.server_names().len(), 1);
        assert_eq!(manager.all_tools().len(), 1);
    }

    #[tokio::test]
    async fn test_blocking_approval_gate() {
        use oneai_core::traits::ApprovalGate;
        use oneai_core::{ApprovalRequest, ApprovalResponse, PermissionLevel};

        let gate = BlockingApprovalGate;
        let request = ApprovalRequest {
            tool_name: "shell".to_string(),
            args: serde_json::json!({"command": "rm -rf /"}),
            risk_level: RiskLevel::High,
            permission_level: Some(PermissionLevel::Full),
            justification: "Delete everything".to_string(),
        };

        let response = gate.request_approval(request).await.unwrap();
        match response {
            ApprovalResponse::Denied { reason } => {
                assert!(reason.contains("placeholder"));
            }
            _ => panic!("Expected Denied response"),
        }
    }
}