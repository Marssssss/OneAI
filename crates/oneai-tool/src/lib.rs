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

    #[tokio::test]
    async fn test_file_write_tool_creates_parent_dirs() {
        let tool = FileWriteTool::new();
        let tmp = std::env::temp_dir();
        // Use a unique nested path that definitely doesn't exist yet.
        let unique = format!("oneai_write_test_{}", std::process::id());
        let nested = tmp.join(&unique).join("nested").join("dir").join("file.txt");
        // Sanity: parent really doesn't exist before the call.
        assert!(!nested.parent().unwrap().exists());

        let result = tool.execute(serde_json::json!({
            "path": nested.to_str().unwrap(),
            "content": "hello",
        })).await.unwrap();
        assert!(result.success, "write_file should succeed: {:?}", result.error);
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "hello");

        // Cleanup
        let _ = std::fs::remove_dir_all(tmp.join(&unique));
    }

    #[tokio::test]
    async fn test_shell_tool_rejects_cat_redirect() {
        // Regression for the "cat > file <<EOF" failure: the model must NOT be
        // allowed to fall back to shell file-writing; it should be redirected
        // to write_file / apply_patch. The rejection must happen before any
        // process is spawned, so this test is safe to run anywhere.
        //
        // Only the genuinely-broken constructs are blocked: `cat >` (hangs on
        // stdin) and heredoc-to-file (body breaks under `sh -c` quoting / sandbox
        // wrapping). `echo >`/`printf >`/`tee` work fine under `sh -c` and are
        // exercised in the no-false-positives test below — see the rationale in
        // `detect_shell_file_write`'s doc comment.
        let tool = ShellTool::new();
        let cases = [
            "cat > /tmp/oneai_shell_test.txt",
            "cat >> /tmp/oneai_shell_test.txt",
            "cat>/tmp/oneai_shell_test.txt",
            "cat > /tmp/oneai_shell_test.txt <<EOF\nhello\nEOF",
            "cat <<EOF > /tmp/oneai_shell_test.txt\nhello\nEOF",
        ];
        for cmd in cases {
            let result = tool.execute(serde_json::json!({"command": cmd})).await.unwrap();
            assert!(!result.success, "expected rejection for: {cmd}");
            let err = result.error.unwrap_or_default();
            assert!(
                err.contains("write_file") || err.contains("apply_patch"),
                "rejection should redirect to write_file/apply_patch, got: {err}"
            );
        }
    }

    #[test]
    fn test_shell_file_write_guard_no_false_positives() {
        // Commands that are NOT file-writing must not trip the guard. Guards
        // against bit-shift false positives (`1 << 8`) and legitimate output
        // redirection from non-text commands (`cargo build > log.txt`).
        //
        // `echo >`/`printf >`/`tee` are intentionally NOT blocked: they work
        // under `sh -c` and have legitimate uses (`cmd | tee log` for live+
        // captured output, `echo x > marker`). Blocking them rejected the whole
        // compound command and took down the `mkdir`/`python`/`cargo` in front
        // of them — the regression this test pins in place.
        use crate::tool_interfaces::detect_shell_file_write;
        let safe = [
            "cargo build --release",
            "cargo build > build.log",
            "git diff > patch.diff",
            "echo $((1 << 8))",
            "echo $((1 << y))",
            "python -c 'print(1 << 4)'",
            "ls -la",
            "cat existing_file.txt",
            "grep -rn 'pattern' src/",
            // `echo >`/`printf >`/`tee` are NOT file-authoring antipatterns that
            // break — they run fine and are preferred over hard-blocking.
            "echo hello > /tmp/oneai_shell_test.txt",
            "printf 'x' > /tmp/oneai_shell_test.txt",
            "tee /tmp/oneai_shell_test.txt",
            // Compound commands the model writes in practice — must NOT be
            // rejected wholesale (these were the mkdir/python failures).
            "mkdir -p foo && echo done > foo/done.txt",
            "python run.py 2>&1 | tee run.log",
            "mkdir -p out && python gen.py | tee out/result.txt",
            // P2: a pure stdout heredoc (no file redirect) is NOT file writing
            // and now works cross-platform via the POSIX-sh fallback, so it
            // must not be rejected.
            "mysql <<EOF\nSELECT 1;\nEOF",
            "psql <<'EOF'\n\\d\nEOF",
        ];
        for cmd in safe {
            assert!(
                detect_shell_file_write(cmd).is_none(),
                "falsely flagged as shell file write: {cmd}"
            );
        }
    }

    #[test]
    fn test_shell_file_write_guard_flags_heredoc_to_file() {
        // A heredoc whose body is redirected to a file IS shell-based file
        // writing and must be flagged even when `cat`/`echo`/`tee` aren't used.
        use crate::tool_interfaces::detect_shell_file_write;
        let cases = [
            "python <<EOF > out.txt\nprint(1)\nEOF",
            "awk <<'END' > report.txt\n{print}\nEND",
        ];
        for cmd in cases {
            assert!(
                detect_shell_file_write(cmd).is_some(),
                "heredoc-to-file not flagged: {cmd}"
            );
        }
    }

    // ─── P2: cross-platform shell resolution tests ──────────────────────────

    #[test]
    fn test_find_sh_in_dirs_finds_sh() {
        use crate::tool_interfaces::find_sh_in_dirs;
        let tmp = std::env::temp_dir();
        let unique = format!("oneai_sh_scan_{}", std::process::id());
        let dir = tmp.join(&unique);
        std::fs::create_dir_all(&dir).unwrap();
        // Create a fake `sh` so find_sh_in_dirs returns it.
        let fake_sh = dir.join("sh");
        std::fs::write(&fake_sh, b"#!/bin/sh\n").unwrap();

        let dir_path = dir.as_path();
        let found = find_sh_in_dirs([dir_path]);
        assert_eq!(found, Some(fake_sh));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_sh_in_dirs_none_when_absent() {
        use crate::tool_interfaces::find_sh_in_dirs;
        let tmp = std::env::temp_dir();
        let unique = format!("oneai_sh_scan_empty_{}", std::process::id());
        let dir = tmp.join(&unique);
        std::fs::create_dir_all(&dir).unwrap();

        let dir_path = dir.as_path();
        assert_eq!(find_sh_in_dirs([dir_path]), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_resolve_shell_uses_sh_on_unix() {
        // On Unix we always use sh -c. This pins the contract; the Windows
        // branch (POSIX-sh-or-PowerShell) is exercised on Windows hosts.
        use crate::tool_interfaces::resolve_shell;
        let (shell, arg) = resolve_shell();
        assert_eq!(shell, std::path::PathBuf::from("sh"));
        assert_eq!(arg, "-c");
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
    async fn test_deny_all_interaction_gate() {
        use oneai_core::traits::InteractionGate;
        use oneai_core::{ApprovalRequest, InteractionRequest, InteractionResponse, PermissionLevel};

        let gate = DenyAllInteractionGate;
        assert!(gate.enabled(oneai_core::InteractionPoint::ToolApproval));
        let request = ApprovalRequest {
            tool_name: "shell".to_string(),
            args: serde_json::json!({"command": "rm -rf /"}),
            risk_level: RiskLevel::High,
            permission_level: Some(PermissionLevel::Full),
            justification: "Delete everything".to_string(),
        };

        let response = gate
            .request(InteractionRequest::ToolApproval { approval: request })
            .await
            .unwrap();
        match response {
            InteractionResponse::Abort { reason } => {
                assert!(reason.contains("denied by DenyAllInteractionGate"));
            }
            _ => panic!("Expected Abort response"),
        }
    }
}