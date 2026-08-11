//! `McpLazyConnectTool` — the model-transparent lazy-connect trigger for MCP
//! servers marked `lazy: true` (issue #31 Stage 5).
//!
//! A lazy server is skipped at startup (`McpPluginRegistry::connect_all_enabled`
//! filters `!e.lazy`). Its real `mcp__<server>__<tool>` wrappers aren't
//! registered until a connect happens, so the model can neither see nor call
//! them. This tool bridges that gap:
//!
//! - Registered once per lazy server at `AppBuilder::build()` time, with
//!   `ToolExposure::Deferred` — it is **excluded from the initial tool schema**
//!   the model sees (#27 exposure gate), but is **discoverable via
//!   `tool_search`**.
//! - On execute: calls `McpPluginRegistry::ensure_connected` (connect + discover)
//!   → `DataLayerReloader::reload_data_layer` (register the now-known wrappers
//!   into the live `ToolRegistry`) → returns the discovered tool names so the
//!   model knows what's newly available.
//! - After a successful connect, `service_available()` returns `false`, so the
//!   Footprint gate **vanishes this tool from `tool_search`** too — the real
//!   tools are now in the registry, the connect trigger is redundant. This is
//!   the codex "LazyWhenCached + vanish-after-cache" pattern.
//!
//! The next AgentLoop iteration reads the live `ToolRegistry`, so the real
//! `mcp__<server>__<tool>` tools surface to the model automatically.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::{DataLayerReloader, Tool};
use oneai_core::{PermissionLevel, RiskLevel, ToolExposure, ToolOutput};

use crate::plugin::McpPluginRegistry;

/// One lazy-connect trigger per `lazy: true` MCP server.
///
/// Construct with [`McpLazyConnectTool::build`] and register the returned
/// `Arc<dyn Tool>` into the `ToolRegistry`.
pub struct McpLazyConnectTool {
    /// The namespaced tool name (`mcp_connect_<server>`).
    tool_name: String,
    /// The raw MCP server name (used to call `ensure_connected`).
    server_name: String,
    description: String,
    registry: Arc<McpPluginRegistry>,
    reloader: Arc<dyn DataLayerReloader>,
    /// Set `true` after a successful connect → `service_available()` returns
    /// `false` → the Footprint gate drops this tool from `tool_search`.
    connected_flag: Arc<AtomicBool>,
}

impl McpLazyConnectTool {
    /// Build a lazy-connect trigger for `server_name`. `description` is the
    /// server entry's description (surfaced in `tool_search` results).
    /// Returns an `Arc<dyn Tool>` ready to register.
    pub fn build(
        server_name: String,
        description: String,
        registry: Arc<McpPluginRegistry>,
        reloader: Arc<dyn DataLayerReloader>,
    ) -> Arc<dyn Tool> {
        let tool_name = Self::tool_name(&server_name);
        Arc::new(Self {
            tool_name,
            server_name,
            description,
            registry,
            reloader,
            connected_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The namespaced trigger name `mcp_connect_<server>`. Distinct from the
    /// real wrappers `mcp__<server>__<tool>` so there's no collision.
    fn tool_name(server: &str) -> String {
        // Mirror the sanitize step of `normalize_tool_name` (lowercase
        // alnum, others → `_`) so the trigger name is a stable, schema-safe
        // identifier even for oddly-named servers.
        let mut out = String::with_capacity(server.len());
        let mut prev_under = false;
        for c in server.chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_lowercase());
                prev_under = false;
            } else if !prev_under {
                out.push('_');
                prev_under = true;
            }
        }
        format!("mcp_connect_{}", out.trim_matches('_'))
    }
}

#[async_trait]
impl Tool for McpLazyConnectTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        // No parameters — the server is implied by the tool's identity.
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    /// Vanish from `tool_search` once the server is connected (the real tools
    /// are now registered, the trigger is redundant).
    fn service_available(&self) -> bool {
        !self.connected_flag.load(Ordering::Relaxed)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Deferred
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
        // Connect (idempotent — returns known tools if already connected) +
        // discover. Errors surface so the model can retry.
        let tools = match self.registry.ensure_connected(&self.server_name).await {
            Ok(names) => names,
            Err(e) => {
                return Ok(ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!(
                        "MCP lazy-connect to '{}' failed: {}",
                        self.server_name, e
                    )),
                    ..Default::default()
                });
            }
        };
        // Surface the now-discovered wrappers into the live ToolRegistry. The
        // `AppDataLayerReloader` re-runs `McpPluginRegistry::register_tools`.
        if let Err(e) = self.reloader.reload_data_layer().await {
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!(
                    "MCP '{}' connected but reload_data_layer failed: {}",
                    self.server_name, e
                )),
                ..Default::default()
            });
        }
        // Vanish this trigger from `tool_search` — the real tools are in place.
        self.connected_flag.store(true, Ordering::Relaxed);

        let content = serde_json::json!({
            "server": self.server_name,
            "connected": true,
            "tools": tools,
            "hint": "tools are now registered; call them by name next turn",
        });
        Ok(ToolOutput {
            success: true,
            content: content.to_string(),
            error: None,
            ..Default::default()
        })
    }
}

// `McpLazyConnectTool` declares Standard permission. It only triggers a
// connect + reload — no destructive side effect. The DomainPack
// `PermissionProfile.permission_overrides` can still tighten `mcp_connect_*`
// per name as for any tool.
impl oneai_tool::tool_interfaces::PermissionAwareTool for McpLazyConnectTool {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Standard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct NoopReloader;
    #[async_trait]
    impl DataLayerReloader for NoopReloader {
        async fn reload_data_layer(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
    }

    fn tool_name_for(server: &str) -> String {
        McpLazyConnectTool::tool_name(server)
    }

    #[test]
    fn tool_name_sanitizes_and_namespaces() {
        assert_eq!(tool_name_for("filesystem"), "mcp_connect_filesystem");
        assert_eq!(tool_name_for("My-Server.io"), "mcp_connect_my_server_io");
        assert_eq!(tool_name_for("a b c"), "mcp_connect_a_b_c");
    }

    #[tokio::test]
    async fn service_available_true_before_connect() {
        // A fresh trigger with an empty registry (ensure_connected would
        // error, but service_available is about the connected flag only).
        let reg = Arc::new(McpPluginRegistry::new());
        let t = McpLazyConnectTool::build(
            "fs".to_string(),
            "d".to_string(),
            reg,
            Arc::new(NoopReloader) as Arc<dyn DataLayerReloader>,
        );
        assert!(t.service_available());
        assert_eq!(t.exposure(), ToolExposure::Deferred);
        assert_eq!(t.risk_level(), RiskLevel::Medium);
        assert_eq!(t.name(), "mcp_connect_fs");
    }

    #[tokio::test]
    async fn execute_failure_keeps_tool_visible_and_reports_error() {
        // No entry named "missing" → ensure_connected errors → the tool stays
        // service_available (flag still false) and returns a failure output.
        let reg = Arc::new(McpPluginRegistry::new());
        let t = McpLazyConnectTool::build(
            "missing".to_string(),
            "no such server".to_string(),
            reg,
            Arc::new(NoopReloader) as Arc<dyn DataLayerReloader>,
        );
        let out = t.execute(serde_json::json!({})).await.unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("lazy-connect"));
        // Flag unchanged → still discoverable for retry.
        assert!(t.service_available());
    }

    #[tokio::test]
    async fn connected_flag_set_after_successful_connect_path() {
        // Build a registry with an enabled entry whose command will fail to
        // spawn (nonexistent binary) — ensure_connected errors, flag stays
        // false. This guards that we only set the flag on real success.
        let mut reg = McpPluginRegistry::new();
        reg.add_entry(crate::plugin::McpPluginEntry {
            name: "noexec".to_string(),
            description: "won't spawn".to_string(),
            source: crate::plugin::McpPluginSource::Stdio {
                command: "/nonexistent/binary/that/does/not/exist".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            enabled: true,
            ..Default::default()
        });
        let reg = Arc::new(reg);
        let t = McpLazyConnectTool::build(
            "noexec".to_string(),
            "won't spawn".to_string(),
            reg.clone(),
            Arc::new(NoopReloader) as Arc<dyn DataLayerReloader>,
        );
        let out = t.execute(serde_json::json!({})).await.unwrap();
        assert!(!out.success);
        assert!(t.service_available()); // still visible (failed → retryable)
        assert!(!reg.is_connected("noexec").await);
    }
}
