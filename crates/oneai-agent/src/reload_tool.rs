//! `reload` tool — runtime data-layer hot reload (evolution-plan §3.4).
//!
//! The model (or the CLI `oneai reload`) invokes this tool to re-read the
//! DomainPack **data layer** mid-session — discovered skill markdown and MCP
//! tool registrations — without restarting. The `AgentLoop` reads the live
//! `ToolRegistry` / `SkillRegistry` every turn, so newly-registered skills /
//! tools appear in the next turn's schema and skill menu automatically; this
//! tool's only job is to trigger the re-read and report what changed.
//!
//! Registered via `ToolRegistry::register_gated` with a `check_fn` that
//! returns `true` only when a reloader is wired — so the `reload` tool has
//! **zero footprint** (vanishes from the schema) in minimal apps with no
//! data-layer reload configured. Its `risk_level` is `Low` (a data refresh,
//! auto-approvable). Newly surfaced tools it registers go through the normal
//! permission resolver + `InteractionGate` on first invocation.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::{DataLayerReloader, Tool};
use oneai_core::{RiskLevel, ToolOutput};

/// A tool that triggers a runtime data-layer reload (skills / MCP tools).
///
/// Holds a shared `Arc<dyn DataLayerReloader>` — the concrete impl
/// (`AppDataLayerReloader`, in `oneai-app`) re-runs skill discovery and MCP
/// re-registration. The trait seam lives in `oneai-core` so this tool (in
/// `oneai-agent`) can hold it without depending on `oneai-skill` /
/// `oneai-mcp`.
pub struct ReloadTool {
    reloader: Arc<dyn DataLayerReloader>,
}

impl ReloadTool {
    /// Create a new `reload` tool backed by the given data-layer reloader.
    pub fn new(reloader: Arc<dyn DataLayerReloader>) -> Self {
        Self { reloader }
    }
}

#[async_trait]
impl Tool for ReloadTool {
    fn name(&self) -> &str {
        "reload"
    }

    fn description(&self) -> &str {
        "Re-read the agent's runtime data layer (discovered skills, MCP tool \
        registrations) without restarting the session. Call this after skills \
        have been added/edited on disk or an MCP server's tool set changed, \
        so the new tools/skills become available on the next step. Returns the \
        names of items (re-)loaded."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
        match self.reloader.reload_data_layer().await {
            Ok(names) => {
                let content = if names.is_empty() {
                    "Data layer reloaded; nothing new found.".to_string()
                } else {
                    format!(
                        "Data layer reloaded. {} item(s) now available:\n{}",
                        names.len(),
                        names
                            .iter()
                            .map(|n| format!("- {n}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                Ok(ToolOutput {
                    success: true,
                    content,
                    error: None,
                    // The reloaded items are skills / MCP tools — not tools
                    // this tool *itself* newly registered into the schema
                    // (that's the reloader's side effect, surfaced next turn
                    // via the live registry read). Self-report the names so
                    // the loop's diff union also catches them promptly.
                    added_tool_names: names,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("Reload failed: {e}")),
                ..Default::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct StubReloader {
        calls: Mutex<usize>,
        names: Vec<String>,
    }

    #[async_trait]
    impl DataLayerReloader for StubReloader {
        async fn reload_data_layer(&self) -> Result<Vec<String>> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.names.clone())
        }
    }

    #[tokio::test]
    async fn reload_tool_reports_loaded_names() {
        let reloader = Arc::new(StubReloader {
            calls: Mutex::new(0),
            names: vec!["new_skill".into(), "mcp_tool_a".into()],
        });
        let tool = ReloadTool::new(reloader.clone() as Arc<dyn DataLayerReloader>);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(out.success);
        assert!(out.content.contains("new_skill"));
        assert!(out.content.contains("mcp_tool_a"));
        assert_eq!(out.added_tool_names, vec!["new_skill", "mcp_tool_a"]);
        assert_eq!(*reloader.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn reload_tool_empty_is_success() {
        let reloader = Arc::new(StubReloader {
            calls: Mutex::new(0),
            names: vec![],
        });
        let tool = ReloadTool::new(reloader as Arc<dyn DataLayerReloader>);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(out.success);
        assert!(out.content.contains("nothing new"));
        assert!(out.added_tool_names.is_empty());
    }

    #[tokio::test]
    async fn reload_tool_surfaces_reloader_error() {
        struct ErrReloader;
        #[async_trait]
        impl DataLayerReloader for ErrReloader {
            async fn reload_data_layer(&self) -> Result<Vec<String>> {
                Err(oneai_core::error::OneAIError::Config("boom".into()))
            }
        }
        let tool = ReloadTool::new(Arc::new(ErrReloader) as Arc<dyn DataLayerReloader>);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!out.success);
        assert!(out.error.as_deref().unwrap().contains("boom"));
    }
}
