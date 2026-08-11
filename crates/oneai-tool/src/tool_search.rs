//! The `tool_search` discovery tool (#27 — Codex `tool_search`).
//!
//! Not every registered tool is handed to the model up-front: a tool whose
//! [`ToolExposure`](oneai_core::ToolExposure) is `Deferred` or
//! `DeferredModelOnly` is excluded from the initial schema (see the
//! `build_tool_definitions_*` filter in `oneai-agent`) and only surfaced when
//! the model asks for it. This tool is how the model asks — it lists the
//! deferred tools (name + description + parameter schema), optionally
//! narrowed by a query substring, so the model can then call one by name.
//! Calling the discovered tool goes through the normal
//! `execute_tool_calls` / `execute_with_approval` path; `Deferred` and
//! `DeferredModelOnly` are model-dispatchable, so the call is not rejected.
//!
//! The tool itself is `Direct` exposure (the model must always be able to
//! see it to discover anything). When no deferred tools exist it returns an
//! empty list — registered always-on by `AppBuilder` (per #27 decision).
//!
//! Stays below `oneai-app`: holds the shared `tools_map` and an optional
//! `ExposureResolver` (the `PermissionProfile` injected from `oneai-domain`
//! via the trait object, so this crate does not depend on `oneai-domain`).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{effective_exposure, ExposureResolver, Tool};
use oneai_core::{RiskLevel, ToolOutput};

/// The tool name.
pub const TOOL_SEARCH_TOOL: &str = "tool_search";

/// The discovery tool — lists deferred tools the model can call.
///
/// Holds the shared registry `tools_map` (the same `Arc<RwLock<HashMap>>`
/// `ToolRegistry::tools_map()` hands out) and an optional
/// [`ExposureResolver`] (the DomainPack `PermissionProfile`). When the
/// resolver is `None`, the effective exposure is the tool's own
/// [`Tool::exposure`] — so the tool works with or without a DomainPack.
pub struct ToolSearchTool {
    tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    exposure_resolver: Option<Arc<dyn ExposureResolver>>,
}

impl ToolSearchTool {
    /// Construct with the shared registry map and an optional exposure
    /// resolver (the DomainPack's `PermissionProfile`, wired by `AppBuilder`).
    pub fn new(
        tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
        exposure_resolver: Option<Arc<dyn ExposureResolver>>,
    ) -> Self {
        Self {
            tools_map,
            exposure_resolver,
        }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_TOOL
    }

    fn description(&self) -> &str {
        "Discover tools that are not in your initial tool list (deferred tools). \
        Returns each tool's name, description, and parameter schema so you can \
        then call it by name. Pass `query` to filter by name/description \
        substring (case-insensitive). Use this when the initial list doesn't \
        contain a tool you need — many specialized / MCP tools are deferred \
        to keep your context focused and are reachable only through here."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional substring to filter by tool name or \
                    description (case-insensitive). Omit to list every deferred tool."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max number of results. Default 20.",
                    "default": 20
                }
            }
        })
    }

    fn risk_level(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn exposure(&self) -> oneai_core::ToolExposure {
        // The model must always be able to discover deferred tools.
        oneai_core::ToolExposure::Direct
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let query: Option<String> = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());
        let limit: usize = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(20);

        let resolver = self.exposure_resolver.as_deref() as Option<&dyn ExposureResolver>;
        let map = self.tools_map.read().await;

        // Collect candidate tools whose effective exposure is search-
        // discoverable (Deferred / DeferredModelOnly) and whose backing service
        // is available (Footprint gate). Sort by name for deterministic
        // output — the model shouldn't see tool order churn run-to-run.
        let mut found: Vec<(&String, &Arc<dyn Tool>)> = map
            .iter()
            .filter(|(_, tool)| tool.service_available())
            .filter(|(_, tool)| {
                let e = effective_exposure(resolver, tool.as_ref());
                e.is_search_discoverable()
            })
            .filter(|(_, tool)| match &query {
                Some(q) => {
                    tool.name().to_ascii_lowercase().contains(q)
                        || tool.description().to_ascii_lowercase().contains(q)
                }
                None => true,
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(b.0));

        let total = found.len();
        let limited: Vec<_> = found.into_iter().take(limit).collect();

        let results: Vec<serde_json::Value> = limited
            .iter()
            .map(|(_, tool)| {
                serde_json::json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "parameters_schema": tool.parameters_schema(),
                })
            })
            .collect();

        let content = serde_json::to_string(&serde_json::json!({
            "results": results,
            "total": total,
            "truncated": total > limited.len(),
        }))
        .map_err(|e| OneAIError::Other(format!("tool_search: serialize: {e}")))?;

        Ok(ToolOutput {
            success: true,
            content,
            error: None,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::ToolExposure;

    /// Minimal mock tool with a configurable name/description and exposure.
    struct MockTool {
        name: String,
        description: String,
        exposure: ToolExposure,
        available: bool,
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
            serde_json::json!({"type": "object"})
        }
        fn risk_level(&self) -> RiskLevel {
            RiskLevel::Low
        }
        fn service_available(&self) -> bool {
            self.available
        }
        fn exposure(&self) -> ToolExposure {
            self.exposure
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput::default())
        }
    }

    fn make_map(tools: Vec<MockTool>) -> Arc<RwLock<HashMap<String, Arc<dyn Tool>>>> {
        let mut m = HashMap::new();
        for t in tools {
            let name = t.name.clone();
            m.insert(name, Arc::new(t) as Arc<dyn Tool>);
        }
        Arc::new(RwLock::new(m))
    }

    #[tokio::test]
    async fn lists_only_deferred_tools_and_excludes_direct_hidden() {
        let map = make_map(vec![
            MockTool {
                name: "direct_tool".into(),
                description: "always visible".into(),
                exposure: ToolExposure::Direct,
                available: true,
            },
            MockTool {
                name: "deferred_a".into(),
                description: "discover me".into(),
                exposure: ToolExposure::Deferred,
                available: true,
            },
            MockTool {
                name: "deferred_b".into(),
                description: "discover me too".into(),
                exposure: ToolExposure::DeferredModelOnly,
                available: true,
            },
            MockTool {
                name: "hidden_one".into(),
                description: "secret".into(),
                exposure: ToolExposure::Hidden,
                available: true,
            },
            MockTool {
                name: "code_only".into(),
                description: "code mode only".into(),
                exposure: ToolExposure::CodeModeOnly,
                available: true,
            },
        ]);
        let tool = ToolSearchTool::new(map, None);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(out.success);
        let parsed: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        let names: Vec<String> = parsed["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        // Sorted by name: deferred_a, deferred_b only.
        assert_eq!(names, vec!["deferred_a", "deferred_b"]);
        assert_eq!(parsed["total"], 2);
    }

    #[tokio::test]
    async fn query_substring_filters_by_name_or_description_case_insensitive() {
        let map = make_map(vec![
            MockTool {
                name: "db_query".into(),
                description: "query the database".into(),
                exposure: ToolExposure::Deferred,
                available: true,
            },
            MockTool {
                name: "web_search".into(),
                description: "QUERY the web".into(),
                exposure: ToolExposure::Deferred,
                available: true,
            },
            MockTool {
                name: "unrelated".into(),
                description: "nothing here".into(),
                exposure: ToolExposure::Deferred,
                available: true,
            },
        ]);
        let tool = ToolSearchTool::new(map, None);
        let out = tool
            .execute(serde_json::json!({"query": "query"}))
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        let names: Vec<String> = parsed["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["db_query", "web_search"]);
    }

    #[tokio::test]
    async fn returns_empty_when_no_deferred_tools() {
        let map = make_map(vec![MockTool {
            name: "only_direct".into(),
            description: "direct".into(),
            exposure: ToolExposure::Direct,
            available: true,
        }]);
        let tool = ToolSearchTool::new(map, None);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(parsed["total"], 0);
        assert!(parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn excludes_service_unavailable_deferred_tools() {
        // A deferred tool whose backing service is down vanishes entirely
        // (Footprint gate) — even from tool_search.
        let map = make_map(vec![MockTool {
            name: "mcp_down".into(),
            description: "deferred but offline".into(),
            exposure: ToolExposure::Deferred,
            available: false,
        }]);
        let tool = ToolSearchTool::new(map, None);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(parsed["total"], 0);
    }

    #[tokio::test]
    async fn exposure_resolver_override_promotes_a_direct_tool_into_search() {
        // Tool's own exposure is Direct, but the resolver (DomainPack map)
        // overrides it to Deferred → tool_search lists it. This is the
        // config-driven "defer a heavy tool" path.
        struct MapResolver(HashMap<String, ToolExposure>);
        #[async_trait]
        impl ExposureResolver for MapResolver {
            fn resolve_exposure(&self, name: &str, _tool: &dyn Tool) -> ToolExposure {
                self.0.get(name).copied().unwrap_or(ToolExposure::Direct)
            }
        }
        let mut m = HashMap::new();
        m.insert("heavy_tool".into(), ToolExposure::Deferred);
        let resolver = Arc::new(MapResolver(m)) as Arc<dyn ExposureResolver>;

        let map = make_map(vec![MockTool {
            name: "heavy_tool".into(),
            description: "heavy".into(),
            exposure: ToolExposure::Direct, // overridden by resolver
            available: true,
        }]);
        let tool = ToolSearchTool::new(map, Some(resolver));
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(parsed["total"], 1);
        assert_eq!(parsed["results"][0]["name"], "heavy_tool");
    }
}
