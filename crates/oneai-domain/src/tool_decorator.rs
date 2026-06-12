//! ToolDecorator + DecoratedTool — domain-specific tool description overrides.
//!
//! This implements the design doc's "方案C" (Decision 2): base tools provide
//! universal capability, DomainPacks add domain-specific decoration.
//!
//! The DecoratedTool wraps an existing `Arc<dyn Tool>` and overrides:
//! - description: domain-specific description that guides the LLM
//! - parameters_schema: merge base schema with domain extra_params
//! - risk_level: domain-specific risk classification override
//!
//! The execute() method delegates to the inner tool unchanged — only the
//! *description* that the LLM sees is modified. This is the key insight:
//! "工具描述就是编码工作流的隐性编程" (tool descriptions are the implicit
//! programming of the coding workflow).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use oneai_core::{PermissionLevel, RiskLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use async_trait::async_trait;

// ─── ToolDecorator ─────────────────────────────────────────────────────────────

/// Configuration for decorating a base tool with domain-specific overrides.
///
/// When a DomainPack includes a ToolDecorator for "read_file", the tool
/// definition built for the LLM uses the decorator's description instead
/// of `FileReadTool::description()`. This is applied at the point where
/// ToolDefinition objects are constructed for inference requests.
///
/// Example (CodingPack):
/// ```ignore
/// ToolDecorator {
///     tool_name: "read_file",
///     description_override: Some("Read source code files. Supports line offset/limit \
///         for large files, encoding detection for different languages..."),
///     permission_override: Some(PermissionLevel::Read),  // Read code = always safe
///     extra_params: serde_json::json!({"encoding": {"type": "string", "default": "utf-8"}}),
/// }
/// ```
///
/// Example (DataAnalysisPack):
/// ```ignore
/// ToolDecorator {
///     tool_name: "read_file",
///     description_override: Some("Read data files (CSV, JSON, Parquet). Auto-detects \
///         format and shows column headers and sample rows..."),
///     permission_override: Some(PermissionLevel::Read),
///     extra_params: serde_json::json!({"format": {"type": "string", "default": "auto"}}),
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDecorator {
    /// The base tool name being decorated.
    pub tool_name: String,

    /// Override for the tool's description.
    /// When None, the base tool's description is used unchanged.
    /// When Some, this replaces the base description — this is the primary
    /// mechanism for domain-specific workflow embedding. The description
    /// tells the LLM *when* and *how* to use the tool in this domain context.
    pub description_override: Option<String>,

    /// Override for the tool's permission level.
    /// When None, the base tool's risk_level() is used.
    /// When Some, this replaces the base permission classification.
    /// Use for domain-specific risk reclassification.
    pub permission_override: Option<PermissionLevel>,

    /// Additional parameter schema properties to merge into the base schema.
    ///
    /// These are merged into the base tool's parameters_schema "properties" object.
    /// The merge is additive — new properties are added, existing properties
    /// are NOT overwritten. Use this to add domain-specific parameters.
    pub extra_params: serde_json::Value,
}

impl ToolDecorator {
    /// Create a simple decorator that only overrides the description.
    pub fn with_description(tool_name: &str, description: &str) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            description_override: Some(description.to_string()),
            permission_override: None,
            extra_params: serde_json::json!({}),
        }
    }

    /// Create a decorator with description and permission override.
    pub fn with_description_and_permission(
        tool_name: &str,
        description: &str,
        permission: PermissionLevel,
    ) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            description_override: Some(description.to_string()),
            permission_override: Some(permission),
            extra_params: serde_json::json!({}),
        }
    }
}

// ─── DecoratedTool ─────────────────────────────────────────────────────────────

/// A Tool wrapper that applies domain-specific overrides to a base tool.
///
/// The DecoratedTool implements the `Tool` trait, delegating:
/// - `name()` → inner.name()
/// - `description()` → decorator override or inner.description()
/// - `parameters_schema()` → merge inner schema with extra_params
/// - `risk_level()` → decorator override or inner.risk_level()
/// - `execute()` → inner.execute(args) (behavior unchanged)
///
/// Only the *description* that the LLM sees is modified — the actual
/// tool execution logic is unchanged. This is the core insight from
/// the design doc: tool descriptions are the primary carrier of
/// domain workflow knowledge.
pub struct DecoratedTool {
    inner: Arc<dyn Tool>,
    decorator: ToolDecorator,
}

impl DecoratedTool {
    /// Create a new DecoratedTool wrapping an inner tool with a decorator.
    pub fn new(inner: Arc<dyn Tool>, decorator: ToolDecorator) -> Self {
        assert_eq!(inner.name(), decorator.tool_name,
            "DecoratedTool: decorator tool_name '{}' must match inner tool name '{}'",
            decorator.tool_name, inner.name()
        );
        Self { inner, decorator }
    }

    /// Get the decorator configuration.
    pub fn decorator(&self) -> &ToolDecorator {
        &self.decorator
    }

    /// Get the inner (base) tool.
    pub fn inner(&self) -> &Arc<dyn Tool> {
        &self.inner
    }
}

#[async_trait]
impl Tool for DecoratedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        // Use decorator override if present, otherwise fall back to inner
        self.decorator.description_override.as_deref()
            .unwrap_or_else(|| self.inner.description())
    }

    fn parameters_schema(&self) -> serde_json::Value {
        // Merge inner schema with extra_params
        let base_schema = self.inner.parameters_schema();

        if self.decorator.extra_params.is_null()
            || self.decorator.extra_params == serde_json::json!({})
        {
            return base_schema; // No extra params → return base unchanged
        }

        merge_tool_schemas(base_schema, self.decorator.extra_params.clone())
    }

    fn risk_level(&self) -> RiskLevel {
        // Use decorator override if present, otherwise fall back to inner
        self.decorator.permission_override
            .map(|p| p.to_risk_level())
            .unwrap_or_else(|| self.inner.risk_level())
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        // Always delegate to inner tool — behavior is unchanged
        self.inner.execute(args).await
    }
}

/// Merge a base tool parameter schema with domain extra_params.
///
/// The merge is additive:
/// - New properties from extra_params are added to the base "properties"
/// - Existing properties in the base are NOT overwritten
/// - The "required" list from extra_params is appended to the base "required"
pub fn merge_tool_schemas(
    base: serde_json::Value,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut merged = base.clone();

    if let (Some(base_obj), Some(extra_props)) =
        (merged.as_object_mut(), extra.get("properties").and_then(|p| p.as_object()))
    {
        // Merge "properties"
        if let Some(base_props) = base_obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
            for (key, value) in extra_props {
                // Only add if not already in base (don't overwrite)
                if !base_props.contains_key(key) {
                    base_props.insert(key.clone(), value.clone());
                }
            }
        }

        // Merge "required"
        if let Some(extra_required) = extra.get("required").and_then(|r| r.as_array()) {
            if let Some(base_required) = base_obj.get_mut("required").and_then(|r| r.as_array_mut()) {
                for item in extra_required {
                    // Only add if not already in base required
                    if !base_required.iter().any(|b| b == item) {
                        base_required.push(item.clone());
                    }
                }
            }
        }

        // Merge any other top-level keys from extra (not properties/required)
        if let Some(extra_obj) = extra.as_object() {
            for (key, value) in extra_obj {
                if key != "properties" && key != "required" {
                    base_obj.insert(key.clone(), value.clone());
                }
            }
        }
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_tool::CalculatorTool;

    #[test]
    fn test_tool_decorator_simple() {
        let decorator = ToolDecorator::with_description("calculator", "Domain-specific calculator");
        assert_eq!(decorator.tool_name, "calculator");
        assert_eq!(decorator.description_override.as_deref(), Some("Domain-specific calculator"));
        assert!(decorator.permission_override.is_none());
    }

    #[test]
    fn test_decorated_tool_description_override() {
        let inner: Arc<dyn Tool> = Arc::new(CalculatorTool::new());
        let decorator = ToolDecorator::with_description(
            "calculator",
            "Scientific calculator for data analysis"
        );
        let decorated = DecoratedTool::new(inner.clone(), decorator);

        // Description overridden
        assert_eq!(decorated.description(), "Scientific calculator for data analysis");

        // Name unchanged
        assert_eq!(decorated.name(), "calculator");
    }

    #[test]
    fn test_decorated_tool_risk_level_override() {
        let inner: Arc<dyn Tool> = Arc::new(CalculatorTool::new());
        // CalculatorTool has Low risk level
        assert_eq!(inner.risk_level(), RiskLevel::Low);

        let decorator = ToolDecorator::with_description_and_permission(
            "calculator",
            "Scientific calculator",
            PermissionLevel::Standard,
        );
        let decorated = DecoratedTool::new(inner.clone(), decorator);

        // Risk level overridden to Standard (Medium)
        assert_eq!(decorated.risk_level(), RiskLevel::Medium);
    }

    #[test]
    fn test_decorated_tool_no_override_fallback() {
        let inner: Arc<dyn Tool> = Arc::new(CalculatorTool::new());
        let original_desc = inner.description();
        let original_risk = inner.risk_level();
        let decorator = ToolDecorator {
            tool_name: "calculator".to_string(),
            description_override: None,  // No override
            permission_override: None,    // No override
            extra_params: serde_json::json!({}),
        };
        let decorated = DecoratedTool::new(inner.clone(), decorator);

        // Falls back to inner tool's values
        assert_eq!(decorated.description(), original_desc);
        assert_eq!(decorated.risk_level(), original_risk);
    }

    #[tokio::test]
    async fn test_decorated_tool_execute_delegates() {
        let inner: Arc<dyn Tool> = Arc::new(CalculatorTool::new());
        let decorator = ToolDecorator::with_description("calculator", "Overridden description");
        let decorated = DecoratedTool::new(inner, decorator);

        // Execute delegates to inner tool
        let result = decorated.execute(serde_json::json!({"expression": "2+3"})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.content, "5");
    }

    #[test]
    fn test_merge_tool_schemas_additive() {
        let base = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"},
                "offset": {"type": "integer", "description": "Line offset"}
            },
            "required": ["path"]
        });

        let extra = serde_json::json!({
            "properties": {
                "encoding": {"type": "string", "default": "utf-8", "description": "File encoding"},
                "format": {"type": "string", "default": "auto"}
            },
            "required": ["encoding"]
        });

        let merged = merge_tool_schemas(base, extra);

        // Should have both base and extra properties
        let props = merged.get("properties").unwrap().as_object().unwrap();
        assert!(props.contains_key("path"));      // base
        assert!(props.contains_key("offset"));    // base
        assert!(props.contains_key("encoding"));  // extra
        assert!(props.contains_key("format"));    // extra

        // Required should include both
        let required = merged.get("required").unwrap().as_array().unwrap();
        assert!(required.iter().any(|r| r == &serde_json::json!("path")));
        assert!(required.iter().any(|r| r == &serde_json::json!("encoding")));
    }

    #[test]
    fn test_merge_tool_schemas_no_overwrite() {
        let base = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Original description"}
            },
            "required": ["path"]
        });

        let extra = serde_json::json!({
            "properties": {
                "path": {"type": "string", "description": "New description"}  // Should NOT overwrite
            },
            "required": ["path"]  // Should NOT duplicate
        });

        let merged = merge_tool_schemas(base, extra);

        // Base property should not be overwritten
        let props = merged.get("properties").unwrap().as_object().unwrap();
        let path_prop = props.get("path").unwrap();
        assert_eq!(path_prop.get("description").unwrap().as_str().unwrap(), "Original description");

        // Required should not duplicate
        let required = merged.get("required").unwrap().as_array().unwrap();
        assert_eq!(required.len(), 1);
    }
}
