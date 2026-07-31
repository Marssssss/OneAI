//! `AppDataLayerReloader` — concrete `DataLayerReloader` for the default
//! OneAI app (evolution-plan §3.4). Re-reads the DomainPack **data layer**:
//! discovered skill markdown (`SkillRegistry::load_discovered`) and MCP tool
//! registrations (`McpPluginRegistry::register_tools`). The `AgentLoop` reads
//! the live `ToolRegistry` / `SkillRegistry` every turn, so a reload surfaces
//! on the next step automatically.
//!
//! Lives in `oneai-app` (depends on `oneai-skill` / `oneai-mcp` / `oneai-tool`)
//! — the `DataLayerReloader` trait seam is in `oneai-core` so the
//! `ReloadTool` (in `oneai-agent`) can hold it without inverting deps.

use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::DataLayerReloader;
use oneai_mcp::McpPluginRegistry;
use oneai_skill::SkillRegistry;
use oneai_tool::ToolRegistry;

/// Default data-layer reloader — skills + MCP tools.
///
/// Holds shared handles to the same registries the `AgentLoop` reads, so a
/// reload mutates the live tables the next turn observes. `MemoryProfile`
/// JSON and `StateGraph` re-read are documented follow-ups (they have no
/// existing from-disk reload entry point yet).
pub struct AppDataLayerReloader {
    skill_registry: Arc<SkillRegistry>,
    /// `None` when no MCP plugin registry is configured — MCP re-registration
    /// is skipped.
    mcp_plugin_registry: Option<Arc<McpPluginRegistry>>,
    tool_registry: Arc<ToolRegistry>,
}

impl AppDataLayerReloader {
    /// Build a reloader from the shared app registries.
    pub fn new(
        skill_registry: Arc<SkillRegistry>,
        mcp_plugin_registry: Option<Arc<McpPluginRegistry>>,
        tool_registry: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            skill_registry,
            mcp_plugin_registry,
            tool_registry,
        }
    }
}

#[async_trait]
impl DataLayerReloader for AppDataLayerReloader {
    async fn reload_data_layer(&self) -> Result<Vec<String>> {
        let mut loaded: Vec<String> = Vec::new();

        // Skills — re-scan the convention directories and (re-)register.
        // `load_discovered` overwrites same-named entries; return the full
        // set of skill names now registered as the "loaded" set.
        self.skill_registry.load_discovered().await;
        loaded.extend(self.skill_registry.skill_names().await);

        // MCP — re-register discovered remote tools into the ToolRegistry.
        if let Some(mcp) = &self.mcp_plugin_registry {
            let names = mcp.register_tools(&self.tool_registry).await?;
            loaded.extend(names);
        }

        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::SkillDescriptor;

    /// `reload_data_layer` runs the skill path and aggregates the live skill
    /// names — a pre-registered skill stays present (load_discovered re-scans
    /// and overwrites same-named entries; it does NOT clear existing ones).
    /// Asserts at the seam level (the from-disk discovery itself is exercised
    /// in `oneai-skill`'s own tests), avoiding env/Home mutation that would
    /// race with parallel tests.
    #[tokio::test]
    async fn reload_runs_skill_path_and_reports_names() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(SkillDescriptor {
                name: "pre".into(),
                description: "pre-existing".into(),
                prompt_template: "do pre".into(),
                trigger_keywords: vec![],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(registry.find_by_name("pre").await.is_some());

        let reloader =
            AppDataLayerReloader::new(registry.clone(), None, Arc::new(ToolRegistry::new()));
        let loaded = reloader.reload_data_layer().await.unwrap();

        assert!(
            loaded.iter().any(|n| n == "pre"),
            "reloaded names should include the pre-registered skill: {loaded:?}"
        );
        assert!(registry.find_by_name("pre").await.is_some());
    }

    /// Reload with an (empty) MCP registry succeeds and returns Ok — the MCP
    /// path runs without error when no servers are connected.
    #[tokio::test]
    async fn reload_mcp_path_runs_without_error() {
        let reloader = AppDataLayerReloader::new(
            Arc::new(SkillRegistry::new()),
            Some(Arc::new(McpPluginRegistry::new())),
            Arc::new(ToolRegistry::new()),
        );
        let loaded = reloader.reload_data_layer().await;
        assert!(loaded.is_ok(), "reload must not error: {:?}", loaded.err());
    }

    /// Reload is idempotent — calling it twice doesn't lose a pre-registered
    /// skill (no destructive clear on re-scan).
    #[tokio::test]
    async fn reload_is_idempotent() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(SkillDescriptor {
                name: "pre".into(),
                description: "pre-existing".into(),
                prompt_template: "do pre".into(),
                trigger_keywords: vec![],
                ..Default::default()
            })
            .await
            .unwrap();
        let reloader =
            AppDataLayerReloader::new(registry.clone(), None, Arc::new(ToolRegistry::new()));
        let _ = reloader.reload_data_layer().await.unwrap();
        let _ = reloader.reload_data_layer().await.unwrap();
        assert!(registry.find_by_name("pre").await.is_some());
    }
}
